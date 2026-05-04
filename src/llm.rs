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
}

impl fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmConfig")
            .field("api_key", &"[REDACTED]")
            .field("api_base", &self.api_base)
            .field("primary_model", &self.primary_model)
            .field("judge_model", &self.judge_model)
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
        let request = CreateChatCompletionRequest {
            model: model.to_string(),
            messages: messages
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
                .collect(),
            stream: Some(true),
            ..Default::default()
        };

        debug!(%label, "sending LLM streaming request");

        let mut stream = self
            .client
            .chat()
            .create_stream(request)
            .await
            .context("LLM streaming request failed")?;

        let mut content = String::new();
        let mut finish_reason: Option<FinishReason> = None;

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(response) => {
                    if let Some(choice) = response.choices.first() {
                        if let Some(delta_content) = &choice.delta.content {
                            content.push_str(delta_content);
                        }
                        if let Some(fr) = &choice.finish_reason {
                            finish_reason = Some(*fr);
                        }
                    }
                }
                Err(async_openai::error::OpenAIError::StreamError(e)) => {
                    if content.is_empty() {
                        anyhow::bail!("LLM stream error with no content received: {e}");
                    }
                    warn!(%label, error = %e, bytes = content.len(), "LLM stream ended prematurely, returning partial content");
                    break;
                }
                Err(e) => {
                    return Err(e).context("LLM stream chunk error");
                }
            }
        }

        if matches!(finish_reason, Some(FinishReason::Length)) {
            warn!(%label, bytes = content.len(), "LLM response truncated (finish_reason=length)");
        }

        let content = content.trim().to_string();
        info!(%label, bytes = content.len(), "LLM response received");
        debug!(%label, %content, "full LLM response");

        Ok(content)
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
