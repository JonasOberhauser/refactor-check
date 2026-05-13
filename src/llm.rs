use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, CreateChatCompletionRequest, FinishReason,
};

pub use async_openai::types::chat::ServiceTier;
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;
use tracing::{debug, info, instrument, trace, warn};

use crate::provider::{LlmProvider, LlmRole};

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
    pub api_base: String,
    pub primary_model: String,
    pub judge_model: String,
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
            api_base: String::new(),
            primary_model: String::new(),
            judge_model: String::new(),
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
            .field("api_base", &self.api_base)
            .field("primary_model", &self.primary_model)
            .field("judge_model", &self.judge_model)
            .field("stream_timeout_ms", &self.stream_timeout_ms)
            .field("max_stream_retries", &self.max_stream_retries)
            .field("service_tier", &self.service_tier)
            .finish()
    }
}

pub struct LlmClient {
    primary_client: async_openai::Client<async_openai::config::OpenAIConfig>,
    judge_client: async_openai::Client<async_openai::config::OpenAIConfig>,
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

impl LlmClient {
    #[must_use]
    pub fn new(config: LlmConfig) -> Self {
        let primary_config = async_openai::config::OpenAIConfig::new()
            .with_api_key(&config.api_key)
            .with_api_base(&config.api_base);

        let judge_key = config.judge_api_key.as_deref().unwrap_or(&config.api_key);
        let judge_config = async_openai::config::OpenAIConfig::new()
            .with_api_key(judge_key)
            .with_api_base(&config.api_base);

        let primary_client = async_openai::Client::with_config(primary_config);
        let judge_client = async_openai::Client::with_config(judge_config);

        Self { primary_client, judge_client, config }
    }

    #[instrument(skip_all, fields(model = %self.config.primary_model))]
    pub async fn chat_primary(&self, messages: Vec<Message>) -> Result<String> {
        self.chat_inner("primary", &self.primary_client, &self.config.primary_model, messages, |content| {
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

    async fn chat_inner(
        &self,
        label: &str,
        client: &async_openai::Client<async_openai::config::OpenAIConfig>,
        model: &str,
        messages: Vec<Message>,
        is_partial_valid: impl Fn(&str) -> bool,
    ) -> Result<String> {
        // TODO: Instead of discarding unusable partial content on retry, we could
        // feed it back to the LLM and ask it to complete the response.
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

        let timeout = std::time::Duration::from_millis(self.config.stream_timeout_ms);
        let max_retries = self.config.max_stream_retries;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            let request = CreateChatCompletionRequest {
                model: model.to_string(),
                messages: request_messages.clone(),
                stream: Some(true),
                service_tier: Some(self.config.service_tier.clone()),
                ..Default::default()
            };

            debug!(%label, attempt, "sending LLM streaming request");

            let mut stream = match client.chat().create_stream(request).await {
                Ok(s) => s,
                Err(e) => {
                    if attempt < max_retries {
                        warn!(%label, attempt, max_retries, error = %e, bytes = 0, reason = "stream creation failed", "LLM retry");
                        continue;
                    }
                    return Err(e).context("LLM streaming request failed after all retries");
                }
            };

            let mut content = String::new();
            let mut finish_reason: Option<FinishReason> = None;
            let mut stream_ended = false;
            let mut got_first_chunk = false;
            let mut tracer = ConnectionTracer::new();

            loop {
                let current_timeout = if got_first_chunk {
                    timeout
                } else {
                    timeout * 4
                };
                match tokio::time::timeout(current_timeout, stream.next()).await {
                    Ok(Some(Ok(response))) => {
                        got_first_chunk = true;
                        if let Some(choice) = response.choices.first() {
                            let has_content = choice.delta.content.as_ref().is_some_and(|s| !s.is_empty());
                            let _has_tool_calls = choice.delta.tool_calls.as_ref().is_some_and(|v| !v.is_empty());
                            #[allow(deprecated)]
                            let _has_function_call = choice.delta.function_call.is_some();

                            if has_content {
                                let text = choice.delta.content.as_ref().unwrap();
                                tracer.on_content(label, text);
                                content.push_str(text);
                            } else {
                                tracer.on_empty(label);
                            }
                            if let Some(fr) = &choice.finish_reason {
                                finish_reason = Some(*fr);
                            }
                        }
                    }
                    Ok(Some(Err(async_openai::error::OpenAIError::StreamError(e)))) => {
                        if !content.is_empty() && is_partial_valid(&content) {
                            warn!(%label, error = %e, bytes = content.len(), "LLM stream error, but partial content is valid, returning it");
                            stream_ended = true;
                            break;
                        }
                        if attempt < max_retries {
                            warn!(%label, attempt, max_retries, error = %e, bytes = content.len(), reason = "SSE stream error", "LLM retry");
                            break;
                        }
                        if content.is_empty() {
                            anyhow::bail!("LLM stream error with no content received after all retries: {e}");
                        }
                        anyhow::bail!("LLM stream error after all retries, partial content not usable: {e}");
                    }
                    Ok(Some(Err(e))) => {
                        if !content.is_empty() && is_partial_valid(&content) {
                            warn!(%label, error = %e, bytes = content.len(), "LLM chunk error, but partial content is valid, returning it");
                            stream_ended = true;
                            break;
                        }
                        if attempt < max_retries {
                            warn!(%label, attempt, max_retries, error = %e, bytes = content.len(), reason = "SSE chunk error", "LLM retry");
                            break;
                        }
                        return Err(e).context("LLM stream chunk error after all retries");
                    }
                    Ok(None) => {
                        stream_ended = true;
                        break;
                    }
                    Err(_) => {
                        if !content.is_empty() && is_partial_valid(&content) {
                            warn!(%label, bytes = content.len(), "stream timed out, but partial content is valid, returning it");
                            stream_ended = true;
                            break;
                        }
                        if attempt < max_retries {
                            warn!(%label, attempt, max_retries, error = "timeout", bytes = content.len(), reason = "timeout", timeout_ms = self.config.stream_timeout_ms, "LLM retry");
                            break;
                        }
                        if content.is_empty() {
                            anyhow::bail!(
                                "LLM stream timed out ({}ms) with no content after all retries",
                                self.config.stream_timeout_ms
                            );
                        }
                        anyhow::bail!(
                            "LLM stream timed out ({}ms) after all retries, partial content not usable",
                            self.config.stream_timeout_ms
                        );
                    }
                }
            }

            if stream_ended {
                if tracer.empty_chunks > 0 {
                    trace!(%label, total_empty_chunks = tracer.empty_chunks, total_content_bytes = tracer.total_content_bytes, "SSE stream ended");
                }
                if matches!(finish_reason, Some(FinishReason::Length)) {
                    warn!(%label, bytes = content.len(), "LLM response truncated (finish_reason=length)");
                }

                let content = content.trim().to_string();
                info!(%label, bytes = content.len(), "LLM response received");
                debug!(%label, %content, "full LLM response");
                return Ok(content);
            }
        }

        anyhow::bail!("LLM stream exhausted all {max_retries} retries without completing");
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
            LlmRole::Primary => self.chat_primary(messages).await,
            LlmRole::Judge => self.chat_judge(messages).await,
        }
    }
}
