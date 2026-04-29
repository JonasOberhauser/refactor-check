use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub api_key: String,
    pub api_base: String,
    pub primary_model: String,
    pub judge_model: String,
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

pub struct LlmClient {
    http: reqwest::Client,
    config: LlmConfig,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> Self {
        let http = reqwest::Client::new();
        Self { http, config }
    }

    pub async fn chat_primary(&self, messages: Vec<(String, String)>) -> Result<String> {
        self.chat(&self.config.primary_model, messages).await
    }

    pub async fn chat_judge(&self, messages: Vec<(String, String)>) -> Result<String> {
        self.chat(&self.config.judge_model, messages).await
    }

    async fn chat(&self, model: &str, messages: Vec<(String, String)>) -> Result<String> {
        let chat_messages: Vec<ChatMessage> = messages
            .into_iter()
            .map(|(role, content)| ChatMessage { role, content })
            .collect();

        let request = ChatRequest {
            model: model.to_string(),
            messages: chat_messages,
        };

        let url = format!("{}/chat/completions", self.config.api_base.trim_end_matches('/'));

        let max_retries = 5;
        for attempt in 0..max_retries {
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
                return Ok(choice.message.content.unwrap_or_default().trim().to_string());
            }

            let error_msg = if let Ok(api_err) = serde_json::from_str::<ApiError>(&body) {
                api_err
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| body.clone())
            } else {
                body.clone()
            };

            if status.as_u16() == 429 && attempt < max_retries - 1 {
                let wait = std::cmp::min(2u64.pow(attempt as u32 + 1), 30);
                eprintln!("Rate limited, retrying in {wait}s: {error_msg}");
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                continue;
            }

            anyhow::bail!("LLM API error ({}): {}", status, error_msg);
        }

        anyhow::bail!("LLM API: exhausted retries")
    }
}

pub fn system_message(content: &str) -> (String, String) {
    ("system".to_string(), content.to_string())
}

pub fn user_message(content: &str) -> (String, String) {
    ("user".to_string(), content.to_string())
}