use anyhow::{Context, Result};
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequest,
    },
};
use serde::{Deserialize, Serialize};


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub api_key: String,
    pub api_base: String,
    pub primary_model: String,
    pub judge_model: String,
}

pub struct LlmClient {
    client: Client<OpenAIConfig>,
    config: LlmConfig,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> Self {
        let openai_config = OpenAIConfig::new()
            .with_api_key(&config.api_key)
            .with_api_base(&config.api_base);
        let client = Client::with_config(openai_config);
        Self { client, config }
    }

    pub async fn chat_primary(&self, messages: Vec<ChatCompletionRequestMessage>) -> Result<String> {
        self.chat(&self.config.primary_model, messages).await
    }

    pub async fn chat_judge(&self, messages: Vec<ChatCompletionRequestMessage>) -> Result<String> {
        self.chat(&self.config.judge_model, messages).await
    }

    async fn chat(
        &self,
        model: &str,
        messages: Vec<ChatCompletionRequestMessage>,
    ) -> Result<String> {
        let request = CreateChatCompletionRequest {
            model: model.to_string(),
            messages,
            ..Default::default()
        };
        let response = self
            .client
            .chat()
            .create(request)
            .await
            .context("LLM chat request failed")?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .context("No response from LLM")?;
        Ok(choice
            .message
            .content
            .unwrap_or_default()
            .trim()
            .to_string())
    }
}

pub fn system_message(content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestSystemMessageArgs::default()
        .content(content)
        .build()
        .unwrap()
        .into()
}

pub fn user_message(content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestUserMessageArgs::default()
        .content(content)
        .build()
        .unwrap()
        .into()
}