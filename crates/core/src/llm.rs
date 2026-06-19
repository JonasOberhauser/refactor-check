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
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, instrument, trace, warn};

use crate::config_update::AppConfig;
use crate::context_id::ContextId;
use crate::error_gate::ErrorGate;
use crate::live_config::LiveConfig;
use crate::provider::{IOProvider, LlmRequest, LlmRole, WithContext};

#[derive(Debug, Clone, Copy)]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Clone)]
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
    #[serde(default)]
    pub splitting_judge_api_key: Option<String>,
    #[serde(default)]
    pub analyzer_api_key: Option<String>,
    pub api_base: String,
    pub formalizer_model: String,
    pub fixer_model: String,
    pub judge_model: String,
    #[serde(default)]
    pub splitting_judge_model: String,
    #[serde(default)]
    pub splitter_model: String,
    #[serde(default)]
    pub analyzer_model: String,
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
            splitting_judge_api_key: None,
            analyzer_api_key: None,
            api_base: String::new(),
            formalizer_model: String::new(),
            fixer_model: String::new(),
            judge_model: String::new(),
            splitting_judge_model: String::new(),
            splitter_model: String::new(),
            analyzer_model: String::new(),
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
            .field("splitting_judge_api_key", &self.splitting_judge_api_key.as_ref().map(|_| "[REDACTED]"))
            .field("analyzer_api_key", &self.analyzer_api_key.as_ref().map(|_| "[REDACTED]"))
            .field("api_base", &self.api_base)
            .field("formalizer_model", &self.formalizer_model)
            .field("fixer_model", &self.fixer_model)
            .field("judge_model", &self.judge_model)
            .field("splitting_judge_model", &self.splitting_judge_model)
            .field("splitter_model", &self.splitter_model)
            .field("analyzer_model", &self.analyzer_model)
            .field("stream_timeout_ms", &self.stream_timeout_ms)
            .field("max_stream_retries", &self.max_stream_retries)
            .field("service_tier", &self.service_tier)
            .finish()
    }
}

type OaiClient = async_openai::Client<async_openai::config::OpenAIConfig>;

struct ClientSet {
    version: u64,
    formalizer: OaiClient,
    fixer: OaiClient,
    judge: OaiClient,
    splitting_judge: OaiClient,
    splitter: OaiClient,
    analyzer: OaiClient,
}

impl ClientSet {
    fn build(config: &LlmConfig, version: u64) -> Self {
        let formalizer_key = config.formalizer_api_key.as_deref().unwrap_or(&config.api_key);
        let fixer_key = config.fixer_api_key.as_deref().unwrap_or(&config.api_key);
        let judge_key = config.judge_api_key.as_deref().unwrap_or(&config.api_key);
        let splitting_judge_key = config.splitting_judge_api_key.as_deref().unwrap_or(judge_key);
        let splitter_key = config.splitter_api_key.as_deref().unwrap_or(&config.api_key);
        let analyzer_key = config.analyzer_api_key.as_deref().unwrap_or(&config.api_key);

        Self {
            version,
            formalizer: OaiClient::with_config(
                async_openai::config::OpenAIConfig::new()
                    .with_api_key(formalizer_key)
                    .with_api_base(&config.api_base),
            ),
            fixer: OaiClient::with_config(
                async_openai::config::OpenAIConfig::new()
                    .with_api_key(fixer_key)
                    .with_api_base(&config.api_base),
            ),
            judge: OaiClient::with_config(
                async_openai::config::OpenAIConfig::new()
                    .with_api_key(judge_key)
                    .with_api_base(&config.api_base),
            ),
            splitting_judge: OaiClient::with_config(
                async_openai::config::OpenAIConfig::new()
                    .with_api_key(splitting_judge_key)
                    .with_api_base(&config.api_base),
            ),
            splitter: OaiClient::with_config(
                async_openai::config::OpenAIConfig::new()
                    .with_api_key(splitter_key)
                    .with_api_base(&config.api_base),
            ),
            analyzer: OaiClient::with_config(
                async_openai::config::OpenAIConfig::new()
                    .with_api_key(analyzer_key)
                    .with_api_base(&config.api_base),
            ),
        }
    }
}

pub struct LlmClient {
    config: Arc<LiveConfig<AppConfig>>,
    clients: std::sync::RwLock<ClientSet>,
    error_gate: Option<Arc<ErrorGate>>,
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

fn is_rate_limit(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("too many requests")
        || lower.contains("code: 429")
        || lower.contains("code: 1302")
        || lower.contains("code: 1305")
}

fn rand_random() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

struct StreamHandler<'a> {
    client: &'a async_openai::Client<async_openai::config::OpenAIConfig>,
    label: &'a str,
    timeout: Duration,
    max_retries: u32,
    service_tier: ServiceTier,
    context_id: &'a ContextId,
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
        context_id: &'a ContextId,
    ) -> Self {
        Self { client, label, timeout, max_retries, service_tier, context_id }
    }

    async fn run(
        &self,
        model: &str,
        request_messages: Vec<ChatCompletionRequestMessage>,
        is_valid: &impl Fn(&str) -> bool,
    ) -> Result<String> {
        let mut attempt: u32 = 0;
        let mut transient_backoff_secs: u64 = 5;
        loop {
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

            debug!(%self.label, %self.context_id, attempt, "sending LLM streaming request");

            let mut stream = loop {
                match self.client.chat().create_stream(request.clone()).await {
                    Ok(s) => {
                        transient_backoff_secs = 5;
                        break s;
                    }
                    Err(e) => {
                        let reason = format!("{e}");
                        if is_rate_limit(&reason) {
                            let jitter = (rand_random() % 3) as u64;
                            warn!(%self.label, %self.context_id, bytes = 0usize, backoff_secs = transient_backoff_secs + jitter, reason, "LLM transient error, retrying without incrementing attempt");
                            tokio::time::sleep(Duration::from_secs(transient_backoff_secs + jitter)).await;
                            transient_backoff_secs = (transient_backoff_secs * 2).min(120);
                            continue;
                        }
                        if attempt < self.max_retries {
                            warn!(%self.label, %self.context_id, attempt, max_retries = self.max_retries, reason, "LLM retry");
                            attempt += 1;
                            continue;
                        }
                        return Err(e).context("LLM streaming request failed after all retries");
                    }
                }
            };

            let mut content = String::new();
            let mut got_first_content = false;
            let mut finish_reason: Option<FinishReason> = None;
            let mut tracer = ConnectionTracer::new();
            let mut total_chunks: u64 = 0;

            let mut transient_retry = false;

            loop {
                let current_timeout = if got_first_content { self.timeout } else { self.timeout * 4 };
                match tokio::time::timeout(current_timeout, stream.next()).await {
                    Ok(Some(Ok(response))) => {
                        total_chunks += 1;
                        if let Some(choice) = response.choices.first() {
                            let has_content = choice.delta.content.as_ref().is_some_and(|c| !c.is_empty());
                            if has_content {
                                got_first_content = true;
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
                        if is_rate_limit(&reason) && content.is_empty() {
                            let jitter = (rand_random() % 3) as u64;
                            warn!(%self.label, %self.context_id, bytes = 0usize, backoff_secs = transient_backoff_secs + jitter, reason, "LLM transient error, retrying without incrementing attempt");
                            tokio::time::sleep(Duration::from_secs(transient_backoff_secs + jitter)).await;
                            transient_backoff_secs = (transient_backoff_secs * 2).min(120);
                            transient_retry = true;
                            break;
                        }
                        match self.handle_failure(&content, attempt, is_valid, &reason)? {
                            FailureAction::UseContent(c) => return Ok(c),
                            FailureAction::Retry => break,
                        }
                    }
                    Ok(None) => {
                        return self.finish(content, &tracer, finish_reason);
                    }
                    Err(_) => {
                        let reason = format!(
                            "timeout after {}ms ({} bytes, {} chunks received)",
                            current_timeout.as_millis(),
                            content.len(),
                            total_chunks,
                        );
                        match self.handle_failure(&content, attempt, is_valid, &reason)? {
                            FailureAction::UseContent(c) => return Ok(c),
                            FailureAction::Retry => break,
                        }
                    }
                }
            }

            if !transient_retry {
                attempt += 1;
            }
            if attempt > self.max_retries {
                bail!("LLM stream exhausted all {} retries without completing ({})", self.max_retries, self.context_id);
            }
        }
    }

    fn handle_failure(
        &self,
        content: &str,
        attempt: u32,
        is_valid: &impl Fn(&str) -> bool,
        reason: &str,
    ) -> Result<FailureAction> {
        if !content.is_empty() && is_valid(content) {
            warn!(%self.label, %self.context_id, bytes = content.len(), reason, "LLM error, but partial content is valid, returning it");
            let content = content.trim().to_string();
            info!(%self.label, %self.context_id, bytes = content.len(), "LLM response received");
            Ok(FailureAction::UseContent(content))
        } else if attempt < self.max_retries {
            warn!(%self.label, %self.context_id, bytes = content.len(), attempt, max_retries = self.max_retries, reason, "LLM retry");
            Ok(FailureAction::Retry)
        } else if content.is_empty() {
            bail!("LLM error with no content after all retries: {reason} ({})", self.context_id)
        } else {
            bail!("LLM error after all retries, partial content not usable: {reason} ({})", self.context_id)
        }
    }

    fn finish(
        &self,
        content: String,
        tracer: &ConnectionTracer,
        finish_reason: Option<FinishReason>,
    ) -> Result<String> {
        if tracer.empty_chunks > 0 {
            trace!(%self.label, %self.context_id, total_empty_chunks = tracer.empty_chunks, total_content_bytes = tracer.total_content_bytes, "SSE stream ended");
        }
        if matches!(finish_reason, Some(FinishReason::Length)) {
            warn!(%self.label, %self.context_id, bytes = content.len(), "LLM response truncated (finish_reason=length)");
        }
        let content = content.trim().to_string();
        info!(%self.label, %self.context_id, bytes = content.len(), "LLM response received");
        let prefixed_response: String = content.lines().map(|l| format!("<\t{l}")).collect::<Vec<_>>().join("\n");
        debug!(%self.label, %self.context_id, content = %prefixed_response, "full LLM response");
        Ok(content)
    }
}

struct ChatCtx<'a> {
    label: &'a str,
    client: &'a OaiClient,
    model: &'a str,
    config: &'a LlmConfig,
    context_id: &'a ContextId,
}

impl LlmClient {
    #[must_use]
    pub fn with_live_config(config: Arc<LiveConfig<AppConfig>>) -> Self {
        let (version, snapshot) = config.snapshot();
        let clients = ClientSet::build(&snapshot.llm, version);
        Self {
            config,
            clients: std::sync::RwLock::new(clients),
            error_gate: None,
        }
    }

    #[must_use]
    pub fn with_error_gate(mut self, gate: Arc<ErrorGate>) -> Self {
        self.error_gate = Some(gate);
        self
    }

    fn ensure_current(&self) -> LlmConfig {
        let (version, snapshot) = self.config.snapshot();
        let llm_config = snapshot.llm;
        {
            let guard = self.clients.read().unwrap_or_else(|e| e.into_inner());
            if guard.version == version {
                return llm_config;
            }
        }
        {
            let mut guard = self.clients.write().unwrap_or_else(|e| e.into_inner());
            if guard.version != version {
                *guard = ClientSet::build(&llm_config, version);
            }
        }
        llm_config
    }

    #[instrument(skip_all, fields(model))]
    pub async fn chat_formalizer(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        tracing::Span::current().record("model", &config.formalizer_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).formalizer.clone();
        self.chat_inner(
            ChatCtx { label: "formalizer", client: &client, model: &config.formalizer_model, config: &config, context_id },
            messages,
            |content| !content.trim().is_empty(),
        ).await
    }

    #[instrument(skip_all, fields(model))]
    pub async fn chat_fixer(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        tracing::Span::current().record("model", &config.fixer_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).fixer.clone();
        self.chat_inner(
            ChatCtx { label: "fixer", client: &client, model: &config.fixer_model, config: &config, context_id },
            messages,
            |content| !content.trim().is_empty(),
        ).await
    }

    #[instrument(skip_all, fields(model))]
    pub async fn chat_judge(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        tracing::Span::current().record("model", &config.judge_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).judge.clone();
        self.chat_inner(
            ChatCtx { label: "judge", client: &client, model: &config.judge_model, config: &config, context_id },
            messages,
            |content| {
                let upper = content.trim().to_uppercase();
                let trimmed = upper.trim_start_matches(|c: char| !c.is_alphabetic());
                trimmed.starts_with("REASONABLE") || trimmed.starts_with("RETRY")
            },
        ).await
    }

    #[instrument(skip_all, fields(model))]
    pub async fn chat_splitter(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        tracing::Span::current().record("model", &config.splitter_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).splitter.clone();
        self.chat_inner(
            ChatCtx { label: "splitter", client: &client, model: &config.splitter_model, config: &config, context_id },
            messages,
            |content| !content.trim().is_empty(),
        ).await
    }

    #[instrument(skip_all, fields(model))]
    pub async fn chat_splitting_judge(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        tracing::Span::current().record("model", &config.splitting_judge_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).splitting_judge.clone();
        self.chat_inner(
            ChatCtx { label: "splitting_judge", client: &client, model: &config.splitting_judge_model, config: &config, context_id },
            messages,
            |content| {
                let upper = content.trim().to_uppercase();
                let trimmed = upper.trim_start_matches(|c: char| !c.is_alphabetic());
                trimmed.starts_with("REASONABLE") || trimmed.starts_with("RETRY")
            },
        ).await
    }

    #[instrument(skip_all, fields(model))]
    pub async fn chat_analyzer(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        tracing::Span::current().record("model", &config.analyzer_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).analyzer.clone();
        self.chat_inner(
            ChatCtx { label: "analyzer", client: &client, model: &config.analyzer_model, config: &config, context_id },
            messages,
            |content| !content.trim().is_empty(),
        ).await
    }

    async fn chat_inner(
        &self,
        ctx: ChatCtx<'_>,
        messages: Vec<Message>,
        is_partial_valid: impl Fn(&str) -> bool,
    ) -> Result<String> {
        let ChatCtx { label, client, model, config, context_id } = ctx;
        for msg in &messages {
            if !matches!(msg.role, Role::System) {
                let prefixed: String = msg.content.lines().map(|l| format!(">\t{l}")).collect::<Vec<_>>().join("\n");
                debug!(%label, %context_id, role = ?msg.role, content = %prefixed, "LLM message");
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
            Duration::from_millis(config.stream_timeout_ms),
            config.max_stream_retries,
            config.service_tier.clone(),
            context_id,
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
impl IOProvider<LlmRequest, WithContext<String>> for LlmClient {
    async fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        let LlmRequest { role, messages, context_id } = input;

        loop {
            let result = match role {
                LlmRole::Splitter => self.chat_splitter(messages.clone(), &context_id).await,
                LlmRole::SplittingJudge => self.chat_splitting_judge(messages.clone(), &context_id).await,
                LlmRole::Formalizer => self.chat_formalizer(messages.clone(), &context_id).await,
                LlmRole::Fixer => self.chat_fixer(messages.clone(), &context_id).await,
                LlmRole::Judge => self.chat_judge(messages.clone(), &context_id).await,
                LlmRole::Analyzer => self.chat_analyzer(messages.clone(), &context_id).await,
            };

            match result {
                Ok(content) => return Ok(WithContext { value: content, context_id }),
                Err(e) => {
                    if let Some(gate) = &self.error_gate {
                        gate.report_and_wait(&format!("{e:#}")).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
}
