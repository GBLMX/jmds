//! The HTTP client: one request shape, one stream, and a retry rule that knows when to give up.
//!
//! Three decisions worth stating, because each of them is a bug if made the other way round:
//!
//! - **The crypto provider is installed here, once.** `reqwest` is built with `rustls-no-provider`
//!   (the lockfile then holds one TLS stack, not two), so a process has to install one before its
//!   first handshake. The previous project did it at every call site that built a client — this one
//!   does it in [`Client::new`], where a client is built.
//! - **A stream that has already produced text is never retried.** A retry re-sends the request
//!   from the start; doing that after the model has answered would splice two answers together in
//!   the transcript and burn a second turn's tokens. Retries are for the part before the answer.
//! - **An answer that is not a 2xx is read as an error, not as a stream.** The API explains what
//!   went wrong in the body; that message is what a user needs to see.

use std::time::Duration;

use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    message::ChatMessage,
    stream::{SseBuffer, StreamEvent, parse_delta},
};

/// How many times a turn may be sent before its answer has started.
const MAX_ATTEMPTS: u32 = 3;

/// The first backoff, doubled per attempt: 500 ms, 1 s.
const RETRY_BASE: Duration = Duration::from_millis(500);

/// How long a request may take before the connection is given up on. Long on purpose: a reasoning
/// model can think for minutes, and the timeout is about a dead socket, not about a slow one.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// What one tool looks like on the wire.
///
/// Held as JSON rather than as a typed schema: the tool layer owns what the fields *mean*, and this
/// crate only has to put them in the request. A schema changed by a tool cannot fail to serialize
/// here for a reason this layer would have to know about.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// Where to reach the model.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl ClientConfig {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: api_key.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("请求失败: {0}")]
    Http(#[from] reqwest::Error),
    /// A non-2xx answer. The body is the provider's own explanation.
    #[error("DeepSeek 返回 {status}: {body}")]
    Status { status: u16, body: String },
    /// The request never had a key to send.
    #[error("没有 API key：把 {0} 设成你的 DeepSeek key，或在 config.toml 里改 api_key_env")]
    MissingKey(String),
    /// The stream arrived but a payload in it was not readable. Its own variant rather than a
    /// made-up status: a 200 that failed to parse is not a 200 that failed.
    #[error("流里的 JSON 读不出来: {0}")]
    Stream(String),
}

impl ApiError {
    /// Whether it is worth sending the same request again.
    ///
    /// `emitted` is whether the stream has already produced anything: once it has, a retry would
    /// duplicate what the user can already see.
    pub fn worth_retrying(&self, emitted: bool) -> bool {
        if emitted {
            return false;
        }
        match self {
            // A request that never left, or a socket that died, is worth another try.
            ApiError::Http(error) => error.is_timeout() || error.is_connect() || error.is_request(),
            // The server's fault, or its load balancer's. A 4xx is ours, and repeating it would
            // only repeat the answer.
            ApiError::Status { status, .. } => *status == 429 || *status >= 500,
            // A chunk that did not parse will not parse on a second read either — but only if it
            // was the answer starting; before that, the whole request is worth one more try.
            ApiError::Stream(_) => true,
            ApiError::MissingKey(_) => false,
        }
    }
}

/// The delay before attempt `attempt` (1-based): 500 ms, then 1 s, then 2 s.
pub fn retry_delay(attempt: u32) -> Duration {
    RETRY_BASE * 2u32.saturating_pow(attempt.saturating_sub(1))
}

pub struct Client {
    http: reqwest::Client,
    config: ClientConfig,
}

impl Client {
    pub fn new(config: ClientConfig) -> Result<Self, ApiError> {
        if config.api_key.trim().is_empty() {
            return Err(ApiError::MissingKey(config.model.clone()));
        }
        // Idempotent: the first caller installs it, later ones are told it is already there, which
        // is why the result is dropped. See the module docs for why it happens here and nowhere
        // else.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self { http, config })
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// Where a chat completion is posted, with the base URL's trailing slash tolerated.
    pub fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }

    /// The request body, as the API documents it.
    ///
    /// `stream_options.include_usage` is what makes the accounting arrive at all — without it the
    /// stream ends without a usage chunk and the turn's cost is unknown.
    pub fn body(&self, messages: &[ChatMessage], tools: &[ToolSpec]) -> serde_json::Value {
        let tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
        }
        body
    }

    /// Send one turn and hand every event to `sink` as it arrives.
    ///
    /// Retries happen here, which is why the caller does not have to: a failure before the first
    /// event is retried (see [`ApiError::worth_retrying`]); after that, it is reported.
    pub async fn stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        sink: &UnboundedSender<StreamEvent>,
    ) -> Result<(), ApiError> {
        let mut attempt = 1;
        loop {
            let mut emitted = false;
            match self.stream_once(messages, tools, sink, &mut emitted).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if attempt >= MAX_ATTEMPTS || !error.worth_retrying(emitted) {
                        return Err(error);
                    }
                    let delay = retry_delay(attempt);
                    log::warn!("第 {attempt} 次请求失败（{error}），{delay:?} 后重试");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// One attempt. `emitted` is set as soon as anything reaches the sink, so the caller knows
    /// whether a failure happened before or after the answer started.
    async fn stream_once(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        sink: &UnboundedSender<StreamEvent>,
        emitted: &mut bool,
    ) -> Result<(), ApiError> {
        let response = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.config.api_key)
            .header("Accept", "text/event-stream")
            .json(&self.body(messages, tools))
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ApiError::Status {
                status: status.as_u16(),
                body: body.trim().to_string(),
            });
        }

        let mut buffer = SseBuffer::new();
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            // Lossy on purpose: a chunk boundary can split a multi-byte character, and dropping
            // the whole chunk because of a caller-side encoding hiccup would lose the answer.
            let text = String::from_utf8_lossy(&chunk);
            for payload in buffer.push(&text) {
                for event in parse_delta(&payload)? {
                    *emitted = true;
                    // A closed sink means nobody is listening any more (the turn was cancelled).
                    if sink.send(event).is_err() {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        ApiError::Stream(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ChatMessage;

    fn client() -> Client {
        Client::new(ClientConfig::new(
            "https://api.deepseek.com/",
            "deepseek-chat",
            "sk-test",
        ))
        .expect("a client with a key builds")
    }

    #[test]
    fn a_client_without_a_key_is_refused_before_it_is_built() {
        let error = Client::new(ClientConfig::new(
            "https://api.deepseek.com",
            "deepseek-chat",
            " ",
        ));
        assert!(matches!(error, Err(ApiError::MissingKey(_))));
    }

    #[test]
    fn the_endpoint_tolerates_a_trailing_slash() {
        assert_eq!(
            client().endpoint(),
            "https://api.deepseek.com/chat/completions"
        );
        let bare = Client::new(ClientConfig::new(
            "https://api.deepseek.com",
            "deepseek-chat",
            "sk-test",
        ))
        .unwrap();
        assert_eq!(bare.endpoint(), "https://api.deepseek.com/chat/completions");
    }

    #[test]
    fn the_body_asks_for_streaming_and_for_the_usage_chunk() {
        let body = client().body(&[ChatMessage::user("hi")], &[]);
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["stream_options"]["include_usage"], true,
            "without this the stream ends without any accounting"
        );
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(
            body.get("tools").is_none(),
            "a request with no tools must not carry an empty array: {body}"
        );
    }

    #[test]
    fn a_tool_goes_into_the_request_in_the_openai_shape() {
        let tool = ToolSpec::new("read", "read a file", serde_json::json!({"type": "object"}));
        let body = client().body(&[ChatMessage::user("hi")], &[tool]);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn a_failure_before_the_answer_may_be_retried_and_one_after_it_may_not() {
        let timeout = ApiError::Status {
            status: 504,
            body: String::new(),
        };
        assert!(timeout.worth_retrying(false));
        assert!(
            !timeout.worth_retrying(true),
            "retrying after text has arrived would splice two answers together"
        );
    }

    #[test]
    fn the_client_s_own_mistakes_are_not_retried() {
        // A bad key or a bad request will be bad again; repeating it just wastes a round trip and
        // hides the message the API took the trouble to write.
        for status in [400, 401, 403, 404, 422] {
            assert!(
                !ApiError::Status {
                    status,
                    body: String::new()
                }
                .worth_retrying(false),
                "{status}"
            );
        }
        assert!(
            !ApiError::MissingKey("deepseek-chat".into()).worth_retrying(false),
            "there is nothing to retry"
        );
    }

    #[test]
    fn a_rate_limit_and_a_server_fault_are_retried() {
        for status in [429, 500, 502, 503] {
            assert!(
                ApiError::Status {
                    status,
                    body: String::new()
                }
                .worth_retrying(false),
                "{status}"
            );
        }
    }

    #[test]
    fn the_backoff_doubles() {
        assert_eq!(retry_delay(1), Duration::from_millis(500));
        assert_eq!(retry_delay(2), Duration::from_secs(1));
        assert_eq!(retry_delay(3), Duration::from_secs(2));
        // A large attempt count must not overflow into a negative or absurd delay.
        assert!(retry_delay(40) >= retry_delay(3));
    }
}
