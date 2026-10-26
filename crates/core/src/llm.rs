use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;
use tracing::{debug, info, instrument, trace, warn};

/// Service tier for chat-completion requests (OpenAI-compatible).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceTier {
    Auto,
    Default,
    Flex,
    Scale,
    Priority,
}

/// Why the model stopped generating (streamed `finish_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
}

/// Socket wake-up interval for the SSE reader thread: the reader loops
/// through read timeouts so a dropped [`SseStream`] shuts its thread down
/// promptly instead of parking on a quiet socket forever.
const SSE_SOCKET_WAKEUP: Duration = Duration::from_secs(1);
/// Connection timeout for the initial HTTP POST.
const SSE_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Sync OpenAI-compatible chat client: POSTs to `{api_base}/chat/completions`
/// with `stream: true` and returns the raw SSE body reader.
#[derive(Clone)]
struct SyncChatClient {
    agent: ureq::Agent,
    api_key: String,
    api_base: String,
}

impl SyncChatClient {
    fn new(api_key: &str, api_base: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(SSE_CONNECT_TIMEOUT)
            .timeout_read(SSE_SOCKET_WAKEUP)
            .build();
        Self { agent, api_key: api_key.to_string(), api_base: api_base.to_string() }
    }

    fn open_stream(&self, body: &serde_json::Value) -> Result<SseStream, String> {
        let url = format!("{}{}", self.api_base.trim_end_matches(URL_PATH_SEPARATOR), CHAT_COMPLETIONS_PATH);
        match self
            .agent
            .post(&url)
            .set(HEADER_AUTHORIZATION, &format!("Bearer {}", self.api_key))
            .set(HEADER_ACCEPT, MIME_EVENT_STREAM)
            .send_json(body.clone())
        {
            Ok(response) => Ok(SseStream::new(response)),
            Err(ureq::Error::Status(code, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err(format!("http status {code}: {body}"))
            }
            Err(e) => Err(format!("{e}")),
        }
    }
}

/// One parsed SSE event off the wire.
enum SseEvent {
    /// A `data: ...` payload.
    Data(String),
    /// End of stream (`data: [DONE]` or clean EOF).
    Eof,
    /// Transport error other than the periodic socket wake-up.
    Error(String),
}

/// SSE body reader driven by its own thread; the consumer picks each event
/// with a per-event timeout (the sync equivalent of
/// `tokio::time::timeout(stream.next())`).
///
/// Dropping the receiver unblocks the reader thread: its next send fails,
/// it drops the body, and the connection closes. Read timeouts on the
/// socket are wake-ups, not errors — they keep the abandon path prompt
/// without disturbing long idle gaps between tokens.
struct SseStream {
    rx: mpsc::Receiver<SseEvent>,
}

impl SseStream {
    fn new(response: ureq::Response) -> Self {
        let (tx, rx) = mpsc::channel();
        let _reader_thread = std::thread::spawn(move || {
            let reader = response.into_reader();
            let mut lines = BufReader::new(reader).lines();
            loop {
                match lines.next() {
                    Some(Ok(line)) => {
                        let line = line.trim_start();
                        if let Some(payload) = line.strip_prefix(SSE_DATA_PREFIX) {
                            let payload = payload.trim();
                            if payload == SSE_DONE {
                                break;
                            }
                            if !payload.is_empty() && tx.send(SseEvent::Data(payload.to_string())).is_err() {
                                break;
                            }
                        }
                        // Non-data lines (heartbeats, event/id fields) are skipped.
                    }
                    Some(Err(e)) => match e.kind() {
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                            if tx.send(SseEvent::Data(String::new())).is_err() {
                                break;
                            }
                        }
                        _ => {
                            let _ = tx.send(SseEvent::Error(format!("{e}")));
                            break;
                        }
                    },
                    None => break,
                }
            }
            let _ = tx.send(SseEvent::Eof);
        });
        Self { rx }
    }

    /// Next event, or `None` on consumer-side timeout.
    fn next_event(&self, timeout: Duration) -> Option<SseEvent> {
        self.rx.recv_timeout(timeout).ok()
    }
}

/// One streamed chunk: `choices[0].delta.content` / `finish_reason`.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(rename = "type", default)]
    _event_type: Option<String>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<FinishReason>,
}

#[derive(Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
}

use crate::config_update::AppConfig;
use crate::context_id::ContextId;
use crate::error_gate::ErrorGate;
use crate::live_config::LiveConfig;
use crate::provider::{IOProvider, LlmRequest, LlmRole, WithContext};

/// Wire constants: HTTP headers, SSE framing, JSON keys, role labels.
const HEADER_AUTHORIZATION: &str = "Authorization";
const HEADER_ACCEPT: &str = "Accept";
const MIME_EVENT_STREAM: &str = "text/event-stream";
const URL_PATH_SEPARATOR: &str = "/";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const SSE_DATA_PREFIX: &str = "data:";
const SSE_DONE: &str = "[DONE]";
const KEY_MODEL: &str = "model";
const KEY_MESSAGES: &str = "messages";
const KEY_STREAM: &str = "stream";
const KEY_SERVICE_TIER: &str = "service_tier";
const KEY_ROLE: &str = "role";
const KEY_CONTENT: &str = "content";
const ROLE_SYSTEM: &str = "system";
const ROLE_USER: &str = "user";
const ROLE_ASSISTANT: &str = "assistant";
/// Role tracing labels and judge verdicts.
const LABEL_FORMALIZER: &str = "formalizer";
const LABEL_FIXER: &str = "fixer";
const LABEL_JUDGE: &str = "judge";
const LABEL_SPLITTER: &str = "splitter";
const LABEL_SPLITTING_JUDGE: &str = "splitting_judge";
const LABEL_ANALYZER: &str = "analyzer";
const VERDICT_REASONABLE: &str = "REASONABLE";
const VERDICT_RETRY: &str = "RETRY";
const SPAN_FIELD_MODEL: &str = "model";
/// Rate-limit signatures in error bodies.
const SIG_RATE_LIMIT: &str = "rate limit";
const SIG_RATE_LIMIT_SLASH: &str = "rate_limit";
const SIG_TOO_MANY_REQUESTS: &str = "too many requests";
const SIG_HTTP_429: &str = "code: 429";
const SIG_CODE_1302: &str = "code: 1302";
const SIG_CODE_1305: &str = "code: 1305";
/// Backoff/retry/timeout knobs.
const TRANSIENT_BACKOFF_START_SECS: u64 = 5;
const TRANSIENT_BACKOFF_MAX_SECS: u64 = 120;
const CTX_STREAM_FAILED: &str = "LLM streaming request failed after all retries";
const RETRY_PAUSE_SECS: u64 = 2;
const JITTER_BOUND: u128 = 3;
const HEARTBEAT_EMPTIES: u64 = 1000;
const EMPTY_CHUNKS_RESET: u64 = 0;
const NO_BYTES_RECEIVED: usize = 0;
const FIRST_CONTENT_TIMEOUT_FACTOR: u32 = 4;
const DEFAULT_STREAM_TIMEOUT_MS: u64 = 3000;
const DEFAULT_MAX_STREAM_RETRIES: u32 = 5;
/// Log prefix markers and line join.
const LOG_PREFIX_RESPONSE: &str = "<\t";
const LOG_PREFIX_MESSAGE: &str = ">\t";
const LINE_JOIN: &str = "\n";
/// Debug field labels and the redaction placeholder.
const DEBUG_STRUCT_NAME: &str = "LlmConfig";
const REDACTED_PLACEHOLDER: &str = "[REDACTED]";
const FIELD_API_KEY: &str = "api_key";
const FIELD_JUDGE_API_KEY: &str = "judge_api_key";
const FIELD_FORMALIZER_API_KEY: &str = "formalizer_api_key";
const FIELD_FIXER_API_KEY: &str = "fixer_api_key";
const FIELD_SPLITTER_API_KEY: &str = "splitter_api_key";
const FIELD_SPLITTING_JUDGE_API_KEY: &str = "splitting_judge_api_key";
const FIELD_ANALYZER_API_KEY: &str = "analyzer_api_key";
const FIELD_API_BASE: &str = "api_base";
const FIELD_SERVICE_TIER: &str = "service_tier";
const FIELD_STREAM_TIMEOUT: &str = "stream_timeout_ms";
const FIELD_MAX_RETRIES: &str = "max_stream_retries";
const FIELD_FORMALIZER_MODEL: &str = "formalizer_model";
const FIELD_FIXER_MODEL: &str = "fixer_model";
const FIELD_JUDGE_MODEL: &str = "judge_model";
const FIELD_SPLITTING_JUDGE_MODEL: &str = "splitting_judge_model";
const FIELD_SPLITTER_MODEL: &str = "splitter_model";
const FIELD_ANALYZER_MODEL: &str = "analyzer_model";

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
    DEFAULT_STREAM_TIMEOUT_MS
}

fn default_max_stream_retries() -> u32 {
    DEFAULT_MAX_STREAM_RETRIES
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
        f.debug_struct(DEBUG_STRUCT_NAME)
            .field(FIELD_API_KEY, &REDACTED_PLACEHOLDER)
            .field(FIELD_JUDGE_API_KEY, &self.judge_api_key.as_ref().map(|_| REDACTED_PLACEHOLDER))
            .field(FIELD_FORMALIZER_API_KEY, &self.formalizer_api_key.as_ref().map(|_| REDACTED_PLACEHOLDER))
            .field(FIELD_FIXER_API_KEY, &self.fixer_api_key.as_ref().map(|_| REDACTED_PLACEHOLDER))
            .field(FIELD_SPLITTER_API_KEY, &self.splitter_api_key.as_ref().map(|_| REDACTED_PLACEHOLDER))
            .field(FIELD_SPLITTING_JUDGE_API_KEY, &self.splitting_judge_api_key.as_ref().map(|_| REDACTED_PLACEHOLDER))
            .field(FIELD_ANALYZER_API_KEY, &self.analyzer_api_key.as_ref().map(|_| REDACTED_PLACEHOLDER))
            .field(FIELD_API_BASE, &self.api_base)
            .field(FIELD_FORMALIZER_MODEL, &self.formalizer_model)
            .field(FIELD_FIXER_MODEL, &self.fixer_model)
            .field(FIELD_JUDGE_MODEL, &self.judge_model)
            .field(FIELD_SPLITTING_JUDGE_MODEL, &self.splitting_judge_model)
            .field(FIELD_SPLITTER_MODEL, &self.splitter_model)
            .field(FIELD_ANALYZER_MODEL, &self.analyzer_model)
            .field(FIELD_STREAM_TIMEOUT, &self.stream_timeout_ms)
            .field(FIELD_MAX_RETRIES, &self.max_stream_retries)
            .field(FIELD_SERVICE_TIER, &self.service_tier)
            .finish()
    }
}

struct ClientSet {
    version: u64,
    formalizer: SyncChatClient,
    fixer: SyncChatClient,
    judge: SyncChatClient,
    splitting_judge: SyncChatClient,
    splitter: SyncChatClient,
    analyzer: SyncChatClient,
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
            formalizer: SyncChatClient::new(formalizer_key, &config.api_base),
            fixer: SyncChatClient::new(fixer_key, &config.api_base),
            judge: SyncChatClient::new(judge_key, &config.api_base),
            splitting_judge: SyncChatClient::new(splitting_judge_key, &config.api_base),
            splitter: SyncChatClient::new(splitter_key, &config.api_base),
            analyzer: SyncChatClient::new(analyzer_key, &config.api_base),
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
        self.empty_chunks = EMPTY_CHUNKS_RESET;
        self.total_content_bytes += text.len() as u64;
        trace!(%label, content_len = text.len(), empties_since_last = skipped, content = %text, "SSE content chunk");
        skipped
    }

    fn on_empty(&mut self, label: &str) {
        self.empty_chunks += 1;
        if self.empty_chunks.is_multiple_of(HEARTBEAT_EMPTIES) {
            trace!(%label, total_empty_chunks = self.empty_chunks, total_content_bytes = self.total_content_bytes, "SSE heartbeat");
        }
    }
}

fn is_rate_limit(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains(SIG_RATE_LIMIT)
        || lower.contains(SIG_RATE_LIMIT_SLASH)
        || lower.contains(SIG_TOO_MANY_REQUESTS)
        || lower.contains(SIG_HTTP_429)
        || lower.contains(SIG_CODE_1302)
        || lower.contains(SIG_CODE_1305)
}

fn rand_random() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

struct StreamHandler<'a> {
    client: &'a SyncChatClient,
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
        client: &'a SyncChatClient,
        label: &'a str,
        timeout: Duration,
        max_retries: u32,
        service_tier: ServiceTier,
        context_id: &'a ContextId,
    ) -> Self {
        Self { client, label, timeout, max_retries, service_tier, context_id }
    }

    fn run(
        &self,
        model: &str,
        request_messages: &[serde_json::Value],
        is_valid: &impl Fn(&str) -> bool,
    ) -> Result<String> {
        let mut attempt: u32 = 0;
        let mut transient_backoff_secs: u64 = TRANSIENT_BACKOFF_START_SECS;
        loop {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(RETRY_PAUSE_SECS));
            }

            let request = serde_json::json!({
                KEY_MODEL: model,
                KEY_MESSAGES: request_messages,
                KEY_STREAM: true,
                KEY_SERVICE_TIER: self.service_tier,
            });

            debug!(%self.label, %self.context_id, attempt, "sending LLM streaming request");

            let stream = loop {
                match self.client.open_stream(&request) {
                    Ok(s) => {
                        transient_backoff_secs = TRANSIENT_BACKOFF_START_SECS;
                        break s;
                    }
                    Err(reason) => {
                        if is_rate_limit(&reason) {
                            let jitter = (rand_random() % JITTER_BOUND) as u64;
                            warn!(%self.label, %self.context_id, bytes = NO_BYTES_RECEIVED, backoff_secs = transient_backoff_secs + jitter, reason, "LLM transient error, retrying without incrementing attempt");
                            std::thread::sleep(Duration::from_secs(transient_backoff_secs + jitter));
                            transient_backoff_secs = (transient_backoff_secs * 2).min(TRANSIENT_BACKOFF_MAX_SECS);
                            continue;
                        }
                        if attempt < self.max_retries {
                            warn!(%self.label, %self.context_id, attempt, max_retries = self.max_retries, reason, "LLM retry");
                            attempt += 1;
                            continue;
                        }
                        return Err(anyhow::anyhow!(reason))
                            .context(CTX_STREAM_FAILED);
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
                let current_timeout = if got_first_content { self.timeout } else { self.timeout * FIRST_CONTENT_TIMEOUT_FACTOR };
                match stream.next_event(current_timeout) {
                    Some(SseEvent::Data(payload)) => {
                        if payload.is_empty() {
                            // Socket wake-up with nothing on the wire.
                            tracer.on_empty(self.label);
                            continue;
                        }
                        let chunk: StreamChunk = match serde_json::from_str(&payload) {
                            Ok(c) => c,
                            Err(e) => {
                                let reason = format!("malformed SSE chunk: {e}");
                                match self.handle_failure(&content, attempt, is_valid, &reason)? {
                                    FailureAction::UseContent(c) => return Ok(c),
                                    FailureAction::Retry => break,
                                }
                            }
                        };
                        total_chunks += 1;
                        if let Some(choice) = chunk.choices.first() {
                            if let Some(text) = choice.delta.content.as_ref().filter(|c| !c.is_empty()) {
                                got_first_content = true;
                                let _n = tracer.on_content(self.label, text);
                                content.push_str(text);
                            } else {
                                tracer.on_empty(self.label);
                            }
                            if let Some(fr) = &choice.finish_reason {
                                finish_reason = Some(*fr);
                            }
                        }
                    }
                    Some(SseEvent::Error(reason)) => {
                        if is_rate_limit(&reason) && content.is_empty() {
                            let jitter = (rand_random() % JITTER_BOUND) as u64;
                            warn!(%self.label, %self.context_id, bytes = NO_BYTES_RECEIVED, backoff_secs = transient_backoff_secs + jitter, reason, "LLM transient error, retrying without incrementing attempt");
                            std::thread::sleep(Duration::from_secs(transient_backoff_secs + jitter));
                            transient_backoff_secs = (transient_backoff_secs * 2).min(TRANSIENT_BACKOFF_MAX_SECS);
                            transient_retry = true;
                            break;
                        }
                        match self.handle_failure(&content, attempt, is_valid, &reason)? {
                            FailureAction::UseContent(c) => return Ok(c),
                            FailureAction::Retry => break,
                        }
                    }
                    Some(SseEvent::Eof) => {
                        return self.finish(content, &tracer, finish_reason);
                    }
                    None => {
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
        let prefixed_response: String = content
            .lines()
            .map(|l| format!("{LOG_PREFIX_RESPONSE}{l}"))
            .collect::<Vec<_>>()
            .join(LINE_JOIN);
        debug!(%self.label, %self.context_id, content = %prefixed_response, "full LLM response");
        Ok(content)
    }
}

struct ChatCtx<'a> {
    label: &'a str,
    client: &'a SyncChatClient,
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
    pub fn chat_formalizer(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        let _span = tracing::Span::current().record(SPAN_FIELD_MODEL, &config.formalizer_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).formalizer.clone();
        self.chat_inner(
            ChatCtx { label: LABEL_FORMALIZER, client: &client, model: &config.formalizer_model, config: &config, context_id },
            messages,
            |content| !content.trim().is_empty(),
        )
    }

    #[instrument(skip_all, fields(model))]
    pub fn chat_fixer(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        let _span = tracing::Span::current().record(SPAN_FIELD_MODEL, &config.fixer_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).fixer.clone();
        self.chat_inner(
            ChatCtx { label: LABEL_FIXER, client: &client, model: &config.fixer_model, config: &config, context_id },
            messages,
            |content| !content.trim().is_empty(),
        )
    }

    #[instrument(skip_all, fields(model))]
    pub fn chat_judge(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        let _span = tracing::Span::current().record(SPAN_FIELD_MODEL, &config.judge_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).judge.clone();
        self.chat_inner(
            ChatCtx { label: LABEL_JUDGE, client: &client, model: &config.judge_model, config: &config, context_id },
            messages,
            |content| {
                let upper = content.trim().to_uppercase();
                let trimmed = upper.trim_start_matches(|c: char| !c.is_alphabetic());
                trimmed.starts_with(VERDICT_REASONABLE) || trimmed.starts_with(VERDICT_RETRY)
            },
        )
    }

    #[instrument(skip_all, fields(model))]
    pub fn chat_splitter(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        let _span = tracing::Span::current().record(SPAN_FIELD_MODEL, &config.splitter_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).splitter.clone();
        self.chat_inner(
            ChatCtx { label: LABEL_SPLITTER, client: &client, model: &config.splitter_model, config: &config, context_id },
            messages,
            |content| !content.trim().is_empty(),
        )
    }

    #[instrument(skip_all, fields(model))]
    pub fn chat_splitting_judge(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        let _span = tracing::Span::current().record(SPAN_FIELD_MODEL, &config.splitting_judge_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).splitting_judge.clone();
        self.chat_inner(
            ChatCtx { label: LABEL_SPLITTING_JUDGE, client: &client, model: &config.splitting_judge_model, config: &config, context_id },
            messages,
            |content| {
                let upper = content.trim().to_uppercase();
                let trimmed = upper.trim_start_matches(|c: char| !c.is_alphabetic());
                trimmed.starts_with(VERDICT_REASONABLE) || trimmed.starts_with(VERDICT_RETRY)
            },
        )
    }

    #[instrument(skip_all, fields(model))]
    pub fn chat_analyzer(&self, messages: Vec<Message>, context_id: &ContextId) -> Result<String> {
        let config = self.ensure_current();
        let _span = tracing::Span::current().record(SPAN_FIELD_MODEL, &config.analyzer_model);
        let client = self.clients.read().unwrap_or_else(|e| e.into_inner()).analyzer.clone();
        self.chat_inner(
            ChatCtx { label: LABEL_ANALYZER, client: &client, model: &config.analyzer_model, config: &config, context_id },
            messages,
            |content| !content.trim().is_empty(),
        )
    }

    fn chat_inner(
        &self,
        ctx: ChatCtx<'_>,
        messages: Vec<Message>,
        is_partial_valid: impl Fn(&str) -> bool,
    ) -> Result<String> {
        let ChatCtx { label, client, model, config, context_id } = ctx;
        for msg in &messages {
            if !matches!(msg.role, Role::System) {
                let prefixed: String = msg
            .content
            .lines()
            .map(|l| format!("{LOG_PREFIX_MESSAGE}{l}"))
            .collect::<Vec<_>>()
            .join(LINE_JOIN);
                debug!(%label, %context_id, role = ?msg.role, content = %prefixed, "LLM message");
            }
        }

        let role_str = |role: Role| match role {
            Role::System => ROLE_SYSTEM,
            Role::User => ROLE_USER,
            Role::Assistant => ROLE_ASSISTANT,
        };
        let request_messages: Vec<serde_json::Value> = messages
            .into_iter()
            .map(|msg| {
                serde_json::json!({
                    KEY_ROLE: role_str(msg.role),
                    KEY_CONTENT: msg.content,
                })
            })
            .collect();

        let handler = StreamHandler::new(
            client,
            label,
            Duration::from_millis(config.stream_timeout_ms),
            config.max_stream_retries,
            config.service_tier,
            context_id,
        );

        handler.run(model, &request_messages, &is_partial_valid)
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

impl IOProvider<LlmRequest, WithContext<String>> for LlmClient {
    fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        let LlmRequest { role, messages, context_id } = input;

        loop {
            let result = match role {
                LlmRole::Splitter => self.chat_splitter(messages.clone(), &context_id),
                LlmRole::SplittingJudge => self.chat_splitting_judge(messages.clone(), &context_id),
                LlmRole::Formalizer => self.chat_formalizer(messages.clone(), &context_id),
                LlmRole::Fixer => self.chat_fixer(messages.clone(), &context_id),
                LlmRole::Judge => self.chat_judge(messages.clone(), &context_id),
                LlmRole::Analyzer => self.chat_analyzer(messages.clone(), &context_id),
            };

            match result {
                Ok(content) => return Ok(WithContext { value: content, context_id }),
                Err(e) => {
                    if let Some(gate) = &self.error_gate {
                        gate.report_and_wait(&format!("{e:#}"))?;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
}
