use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use tracing::{debug, info, instrument, warn};

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

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
}

#[derive(Debug, Serialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: Option<ApiErrorDetail>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorDetail {
    message: String,
}

const MAX_RETRIES: usize = 5;

pub struct LlmClient {
    http: reqwest::Client,
    config: LlmConfig,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> Self {
        let http = reqwest::Client::new();
        Self { http, config }
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
        let chat_messages: Vec<ChatMessage> = messages
            .into_iter()
            .map(|msg| ChatMessage {
                role: match msg.role {
                    Role::System => "system".to_string(),
                    Role::User => "user".to_string(),
                },
                content: msg.content,
            })
            .collect();

        let request = ChatRequest {
            model: model.to_string(),
            messages: chat_messages,
        };

        let url = format!("{}/chat/completions", self.config.api_base.trim_end_matches('/'));

        for attempt in 0..MAX_RETRIES {
            debug!(%label, attempt, "sending LLM request");

            let response = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.config.api_key))
                .header("Content-Type", "application/json")
                .json(&request)
                .send()
                .await
                .context("HTTP request to LLM failed")?;

            let status = response.status();
            let body = response.text().await.context("Failed to read LLM response body")?;

            if status.is_success() {
                let chat_response: ChatResponse =
                    serde_json::from_str(&body).context("Failed to parse LLM response")?;
                let choice = chat_response
                    .choices
                    .into_iter()
                    .next()
                    .context("No response from LLM")?;
                let content = choice.message.content.unwrap_or_default().trim().to_string();
                info!(%label, bytes = content.len(), "LLM response received");
                return Ok(content);
            }

            let error_msg = if let Ok(api_err) = serde_json::from_str::<ApiError>(&body) {
                api_err
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| body.clone())
            } else {
                body.clone()
            };

            let is_retryable = status.as_u16() == 429 || status.is_server_error();
            if is_retryable && attempt < MAX_RETRIES - 1 {
                let wait = std::cmp::min(2u64.pow(attempt as u32 + 1), 30);
                warn!(%label, %status, wait_secs = wait, %error_msg, "retryable error, sleeping");
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                continue;
            }

            anyhow::bail!("LLM API error ({}): {}", status, error_msg);
        }

        anyhow::bail!("LLM API: exhausted {MAX_RETRIES} retries")
    }
}

pub fn system_message(content: &str) -> Message {
    Message {
        role: Role::System,
        content: content.to_string(),
    }
}

pub fn user_message(content: &str) -> Message {
    Message {
        role: Role::User,
        content: content.to_string(),
    }
}