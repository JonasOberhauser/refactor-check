use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, CreateChatCompletionRequest, FinishReason,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::fmt;
use tracing::{debug, info, instrument, warn};

use crate::provider::{LlmProvider, LlmRole};

pub enum Role {
    System,
    User,
}

pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub api_key: String,
    pub api_base: String,
    pub primary_model: String,
    pub judge_model: String,
    #[serde(default = "default_stream_timeout_ms")]
    pub stream_timeout_ms: u64,
    #[serde(default = "default_max_stream_retries")]
    pub max_stream_retries: u32,
}

fn default_stream_timeout_ms() -> u64 {
    3000
}

fn default_max_stream_retries() -> u32 {
    5
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_base: String::new(),
            primary_model: String::new(),
            judge_model: String::new(),
            stream_timeout_ms: default_stream_timeout_ms(),
            max_stream_retries: default_max_stream_retries(),
        }
    }
}

impl fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmConfig")
            .field("api_key", &"[REDACTED]")
            .field("api_base", &self.api_base)
            .field("primary_model", &self.primary_model)
            .field("judge_model", &self.judge_model)
            .field("stream_timeout_ms", &self.stream_timeout_ms)
            .field("max_stream_retries", &self.max_stream_retries)
            .finish()
    }
}

pub struct LlmClient {
    client: async_openai::Client<async_openai::config::OpenAIConfig>,
    config: LlmConfig,
}

impl LlmClient {
    #[must_use]
    pub fn new(config: LlmConfig) -> Self {
        let openai_config = async_openai::config::OpenAIConfig::new()
            .with_api_key(&config.api_key)
            .with_api_base(&config.api_base);

        let client = async_openai::Client::with_config(openai_config);

        Self { client, config }
    }

    #[instrument(skip_all, fields(model = %self.config.primary_model))]
    pub async fn chat_primary(&self, messages: Vec<Message>) -> Result<String> {
        self.chat("primary", &self.config.primary_model, messages).await
    }

    #[instrument(skip_all, fields(model = %self.config.judge_model))]
    pub async fn chat_judge(&self, messages: Vec<Message>) -> Result<String> {
        self.chat("judge", &self.config.judge_model, messages).await
    }

    async fn chat(&self, label: &str, model: &str, messages: Vec<Message>) -> Result<String> {
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
            })
            .collect();

        let timeout = std::time::Duration::from_millis(self.config.stream_timeout_ms);
        let max_retries = self.config.max_stream_retries;

        for attempt in 0..=max_retries {
            let request = CreateChatCompletionRequest {
                model: model.to_string(),
                messages: request_messages.clone(),
                stream: Some(true),
                ..Default::default()
            };

            debug!(%label, attempt, "sending LLM streaming request");

            let mut stream = match self.client.chat().create_stream(request).await {
                Ok(s) => s,
                Err(e) => {
                    if attempt < max_retries {
                        warn!(%label, attempt, error = %e, "LLM stream creation failed, retrying");
                        continue;
                    }
                    return Err(e).context("LLM streaming request failed after all retries");
                }
            };

            let mut content = String::new();
            let mut finish_reason: Option<FinishReason> = None;
            let mut stream_ended = false;

            loop {
                match tokio::time::timeout(timeout, stream.next()).await {
                    Ok(Some(Ok(response))) => {
                        if let Some(choice) = response.choices.first() {
                            if let Some(delta_content) = &choice.delta.content {
                                content.push_str(delta_content);
                            }
                            if let Some(fr) = &choice.finish_reason {
                                finish_reason = Some(*fr);
                            }
                        }
                    }
                    Ok(Some(Err(async_openai::error::OpenAIError::StreamError(e)))) => {
                        if attempt < max_retries {
                            warn!(%label, attempt, error = %e, bytes = content.len(), "LLM stream error, retrying");
                            break;
                        }
                        if content.is_empty() {
                            anyhow::bail!("LLM stream error with no content received after all retries: {e}");
                        }
                        warn!(%label, error = %e, bytes = content.len(), "LLM stream ended prematurely, returning partial content");
                        stream_ended = true;
                        break;
                    }
                    Ok(Some(Err(e))) => {
                        if attempt < max_retries {
                            warn!(%label, attempt, error = %e, "LLM stream chunk error, retrying");
                            break;
                        }
                        return Err(e).context("LLM stream chunk error after all retries");
                    }
                    Ok(None) => {
                        stream_ended = true;
                        break;
                    }
                    Err(_) => {
                        if attempt < max_retries {
                            warn!(%label, attempt, "stream timeout ({}ms), retrying", self.config.stream_timeout_ms);
                            break;
                        }
                        if content.is_empty() {
                            anyhow::bail!(
                                "LLM stream timed out ({}ms) with no content after all retries",
                                self.config.stream_timeout_ms
                            );
                        }
                        warn!(%label, bytes = content.len(), "stream timed out, returning partial content");
                        stream_ended = true;
                        break;
                    }
                }
            }

            if stream_ended {
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

#[async_trait]
impl LlmProvider for LlmClient {
    async fn chat(&self, role: LlmRole, messages: Vec<Message>) -> Result<String> {
        match role {
            LlmRole::Primary => self.chat_primary(messages).await,
            LlmRole::Judge => self.chat_judge(messages).await,
        }
    }
}
