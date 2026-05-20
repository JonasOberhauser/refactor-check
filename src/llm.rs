use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, CreateChatCompletionRequest, FinishReason,
};

pub use async_openai::types::chat::ServiceTier;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;
use tracing::{debug, info, instrument, trace, warn};

use crate::provider::{LlmProvider, LlmRole};

#[derive(Debug)]
pub enum Role {
    System,
    User,
    Assistant,
}

pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub api_key: String,
    #[serde(default)]
    pub judge_api_key: Option<String>,
    #[serde(default)]
    pub formalizer_api_key: Option<String>,
    #[serde(default)]
    pub fixer_api_key: Option<String>,
    #[serde(default)]
    pub splitter_api_key: Option<String>,
    pub api_base: String,
    pub formalizer_model: String,
    pub fixer_model: String,
    pub judge_model: String,
    #[serde(default)]
    pub splitter_model: String,
    #[serde(default = "default_stream_timeout_ms")]
    pub stream_timeout_ms: u64,
    #[serde(default = "default_max_stream_retries")]
    pub max_stream_retries: u32,
    #[serde(default = "default_service_tier")]
    pub service_tier: ServiceTier,
}

fn default_stream_timeout_ms() -> u64 {
    3000
}

fn default_max_stream_retries() -> u32 {
    5
}

fn default_service_tier() -> ServiceTier {
    ServiceTier::Priority
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            judge_api_key: None,
            formalizer_api_key: None,
            fixer_api_key: None,
            splitter_api_key: None,
            api_base: String::new(),
            formalizer_model: String::new(),
            fixer_model: String::new(),
            judge_model: String::new(),
            splitter_model: String::new(),
            stream_timeout_ms: default_stream_timeout_ms(),
            max_stream_retries: default_max_stream_retries(),
            service_tier: default_service_tier(),
        }
    }
}

impl fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmConfig")
            .field("api_key", &"[REDACTED]")
            .field("judge_api_key", &self.judge_api_key.as_ref().map(|_| "[REDACTED]"))
            .field("formalizer_api_key", &self.formalizer_api_key.as_ref().map(|_| "[REDACTED]"))
            .field("fixer_api_key", &self.fixer_api_key.as_ref().map(|_| "[REDACTED]"))
            .field("splitter_api_key", &self.splitter_api_key.as_ref().map(|_| "[REDACTED]"))
            .field("api_base", &self.api_base)
            .field("formalizer_model", &self.formalizer_model)
            .field("fixer_model", &self.fixer_model)
            .field("judge_model", &self.judge_model)
            .field("splitter_model", &self.splitter_model)
            .field("stream_timeout_ms", &self.stream_timeout_ms)
            .field("max_stream_retries", &self.max_stream_retries)
            .field("service_tier", &self.service_tier)
            .finish()
    }
}

pub struct LlmClient {
    formalizer_client: async_openai::Client<async_openai::config::OpenAIConfig>,
    fixer_client: async_openai::Client<async_openai::config::OpenAIConfig>,
    judge_client: async_openai::Client<async_openai::config::OpenAIConfig>,
    splitter_client: async_openai::Client<async_openai::config::OpenAIConfig>,
    config: LlmConfig,
}

struct ConnectionTracer {
    empty_chunks: u64,
    total_content_bytes: u64,
}

impl ConnectionTracer {
    fn new() -> Self {
        Self { empty_chunks: 0, total_content_bytes: 0 }
    }

    fn on_content(&mut self, label: &str, text: &str) -> u64 {
        let skipped = self.empty_chunks;
        self.empty_chunks = 0;
        self.total_content_bytes += text.len() as u64;
        trace!(%label, content_len = text.len(), empties_since_last = skipped, content = %text, "SSE content chunk");
        skipped
    }

    fn on_empty(&mut self, label: &str) {
        self.empty_chunks += 1;
        if self.empty_chunks.is_multiple_of(1000) {
            trace!(%label, total_empty_chunks = self.empty_chunks, total_content_bytes = self.total_content_bytes, "SSE heartbeat");
        }
    }
}

struct StreamHandler<'a> {
    client: &'a async_openai::Client<async_openai::config::OpenAIConfig>,
    label: &'a str,
    timeout: Duration,
    max_retries: u32,
    service_tier: ServiceTier,
}

enum FailureAction {
    UseContent(String),
    Retry,
}

impl<'a> StreamHandler<'a> {
    fn new(
        client: &'a async_openai::Client<async_openai::config::OpenAIConfig>,
        label: &'a str,
        timeout: Duration,
        max_retries: u32,
        service_tier: ServiceTier,
    ) -> Self {
        Self { client, label, timeout, max_retries, service_tier }
    }

    async fn run(
        &self,
        model: &str,
        request_messages: Vec<ChatCompletionRequestMessage>,
        is_valid: &impl Fn(&str) -> bool,
    ) -> Result<String> {
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }

            let request = CreateChatCompletionRequest {
                model: model.to_string(),
                messages: request_messages.clone(),
                stream: Some(true),
                service_tier: Some(self.service_tier.clone()),
                ..Default::default()
            };

            debug!(%self.label, attempt, "sending LLM streaming request");

            let mut stream = match self.client.chat().create_stream(request).await {
                Ok(s) => s,
                Err(e) => {
                    if attempt < self.max_retries {
                        warn!(%self.label, attempt, max_retries = self.max_retries, "LLM retry");
                        continue;
                    }
                    return Err(e).context("LLM streaming request failed after all retries");
                }
            };

            let mut content = String::new();
            let mut got_first_chunk = false;
            let mut finish_reason: Option<FinishReason> = None;
            let mut tracer = ConnectionTracer::new();

            loop {
                let current_timeout = if got_first_chunk { self.timeout } else { self.timeout * 4 };
                match tokio::time::timeout(current_timeout, stream.next()).await {
                    Ok(Some(Ok(response))) => {
                        got_first_chunk = true;
                        if let Some(choice) = response.choices.first() {
                            let has_content = choice.delta.content.as_ref().is_some_and(|c| !c.is_empty());
                            if has_content {
                                let text = choice.delta.content.as_ref().unwrap();
                                tracer.on_content(self.label, text);
                                content.push_str(text);
                            } else {
                                tracer.on_empty(self.label);
                            }
                            if let Some(fr) = &choice.finish_reason {
                                finish_reason = Some(*fr);
                            }
                        }
                    }
                    Ok(Some(Err(e))) => {
                        let reason = format!("{e}");
                        match self.handle_failure(&content, attempt, is_valid, &reason)? {
                            FailureAction::UseContent(c) => return Ok(c),
                            FailureAction::Retry => break,
                        }
                    }
                    Ok(None) => {
                        return self.finish(content, &tracer, finish_reason);
                    }
                    Err(_) => {
                        let reason = format!("timeout after {}ms", self.timeout.as_millis());
                        match self.handle_failure(&content, attempt, is_valid, &reason)? {
                            FailureAction::UseContent(c) => return Ok(c),
                            FailureAction::Retry => break,
                        }
                    }
                }
            }
        }

        bail!("LLM stream exhausted all {} retries without completing", self.max_retries)
    }

    fn handle_failure(
        &self,
        content: &str,
        attempt: u32,
        is_valid: &impl Fn(&str) -> bool,
        reason: &str,
    ) -> Result<FailureAction> {
        if !content.is_empty() && is_valid(content) {
            warn!(%self.label, bytes = content.len(), reason, "LLM error, but partial content is valid, returning it");
            let content = content.trim().to_string();
            info!(%self.label, bytes = content.len(), "LLM response received");
            Ok(FailureAction::UseContent(content))
        } else if attempt < self.max_retries {
            warn!(%self.label, attempt, max_retries = self.max_retries, reason, "LLM retry");
            Ok(FailureAction::Retry)
        } else if content.is_empty() {
            bail!("LLM error with no content after all retries: {reason}")
        } else {
            bail!("LLM error after all retries, partial content not usable: {reason}")
        }
    }

    fn finish(
        &self,
        content: String,
        tracer: &ConnectionTracer,
        finish_reason: Option<FinishReason>,
    ) -> Result<String> {
        if tracer.empty_chunks > 0 {
            trace!(%self.label, total_empty_chunks = tracer.empty_chunks, total_content_bytes = tracer.total_content_bytes, "SSE stream ended");
        }
        if matches!(finish_reason, Some(FinishReason::Length)) {
            warn!(%self.label, bytes = content.len(), "LLM response truncated (finish_reason=length)");
        }
        let content = content.trim().to_string();
        info!(%self.label, bytes = content.len(), "LLM response received");
        debug!(%self.label, %content, "full LLM response");
        Ok(content)
    }
}

impl LlmClient {
    #[must_use]
    pub fn new(config: LlmConfig) -> Self {
        let formalizer_key = config.formalizer_api_key.as_deref().unwrap_or(&config.api_key);
        let formalizer_config = async_openai::config::OpenAIConfig::new()
            .with_api_key(formalizer_key)
            .with_api_base(&config.api_base);

        let fixer_key = config.fixer_api_key.as_deref().unwrap_or(&config.api_key);
        let fixer_config = async_openai::config::OpenAIConfig::new()
            .with_api_key(fixer_key)
            .with_api_base(&config.api_base);

        let judge_key = config.judge_api_key.as_deref().unwrap_or(&config.api_key);
        let judge_config = async_openai::config::OpenAIConfig::new()
            .with_api_key(judge_key)
            .with_api_base(&config.api_base);

        let formalizer_client = async_openai::Client::with_config(formalizer_config);
        let fixer_client = async_openai::Client::with_config(fixer_config);
        let judge_client = async_openai::Client::with_config(judge_config);

        let splitter_key = config.splitter_api_key.as_deref().unwrap_or(&config.api_key);
        let splitter_config = async_openai::config::OpenAIConfig::new()
            .with_api_key(splitter_key)
            .with_api_base(&config.api_base);
        let splitter_client = async_openai::Client::with_config(splitter_config);

        Self { formalizer_client, fixer_client, judge_client, splitter_client, config }
    }

    #[instrument(skip_all, fields(model = %self.config.formalizer_model))]
    pub async fn chat_formalizer(&self, messages: Vec<Message>) -> Result<String> {
        self.chat_inner("formalizer", &self.formalizer_client, &self.config.formalizer_model, messages, |content| {
            crate::smt::extract_smt_formula(content).is_some()
        }).await
    }

    #[instrument(skip_all, fields(model = %self.config.fixer_model))]
    pub async fn chat_fixer(&self, messages: Vec<Message>) -> Result<String> {
        self.chat_inner("fixer", &self.fixer_client, &self.config.fixer_model, messages, |content| {
            crate::smt::extract_smt_formula(content).is_some()
        }).await
    }

    #[instrument(skip_all, fields(model = %self.config.judge_model))]
    pub async fn chat_judge(&self, messages: Vec<Message>) -> Result<String> {
        self.chat_inner("judge", &self.judge_client, &self.config.judge_model, messages, |content| {
            let upper = content.trim().to_uppercase();
            let trimmed = upper.trim_start_matches(|c: char| !c.is_alphabetic());
            trimmed.starts_with("REASONABLE") || trimmed.starts_with("RETRY")
        }).await
    }

    #[instrument(skip_all, fields(model = %self.config.splitter_model))]
    pub async fn chat_splitter(&self, messages: Vec<Message>) -> Result<String> {
        self.chat_inner("splitter", &self.splitter_client, &self.config.splitter_model, messages, |content| {
            !content.trim().is_empty()
        }).await
    }

    async fn chat_inner(
        &self,
        label: &str,
        client: &async_openai::Client<async_openai::config::OpenAIConfig>,
        model: &str,
        messages: Vec<Message>,
        is_partial_valid: impl Fn(&str) -> bool,
    ) -> Result<String> {
        for msg in &messages {
            if !matches!(msg.role, Role::System) {
                debug!(%label, role = ?msg.role, content = %msg.content, "LLM message");
            }
        }

        let request_messages: Vec<ChatCompletionRequestMessage> = messages
            .into_iter()
            .map(|msg| match msg.role {
                Role::System => {
                    ChatCompletionRequestMessage::System(
                        ChatCompletionRequestSystemMessage {
                            content: ChatCompletionRequestSystemMessageContent::Text(msg.content),
                            name: None,
                        },
                    )
                }
                Role::User => {
                    ChatCompletionRequestMessage::User(
                        ChatCompletionRequestUserMessage {
                            content: ChatCompletionRequestUserMessageContent::Text(msg.content),
                            name: None,
                        },
                    )
                }
                Role::Assistant => {
                    ChatCompletionRequestMessage::Assistant(
                        async_openai::types::chat::ChatCompletionRequestAssistantMessage {
                            content: Some(async_openai::types::chat::ChatCompletionRequestAssistantMessageContent::Text(msg.content)),
                            name: None,
                            ..Default::default()
                        },
                    )
                }
            })
            .collect();

        let handler = StreamHandler::new(
            client,
            label,
            Duration::from_millis(self.config.stream_timeout_ms),
            self.config.max_stream_retries,
            self.config.service_tier.clone(),
        );

        handler.run(model, request_messages, &is_partial_valid).await
    }
}

#[must_use]
pub fn system_message(content: &str) -> Message {
    Message {
        role: Role::System,
        content: content.to_string(),
    }
}

#[must_use]
pub fn user_message(content: &str) -> Message {
    Message {
        role: Role::User,
        content: content.to_string(),
    }
}

#[must_use]
pub fn assistant_message(content: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: content.to_string(),
    }
}

#[async_trait]
impl LlmProvider for LlmClient {
    async fn chat(&self, role: LlmRole, messages: Vec<Message>) -> Result<String> {
        match role {
            LlmRole::Splitter => self.chat_splitter(messages).await,
            LlmRole::Formalizer => self.chat_formalizer(messages).await,
            LlmRole::Fixer => self.chat_fixer(messages).await,
            LlmRole::Judge => self.chat_judge(messages).await,
        }
    }
}
