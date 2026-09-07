//! Request and response shapes, one per provider.
//!
//! The operator's decision was `GLM-5.3-Flash` on Z.ai, and the operator's other
//! decision was that the model is **a config line, not a rewrite**. This module
//! is where that second decision is either true or a slogan.
//!
//! It is true because everything above this file — planning, batching, caching,
//! staleness, redaction, retries, accounting — is written against
//! [`ChatProvider`], which has exactly two methods: turn a system prompt and a
//! user prompt into an [`HttpRequest`], and turn an [`HttpResponse`] into text
//! plus a token count. Nothing else in the feature knows a provider exists.
//!
//! Three ship:
//!
//! | [`Provider`] | Shape | Default endpoint | Key |
//! |---|---|---|---|
//! | [`Provider::Glm`] | `OpenAI`-compatible chat completions | `https://api.z.ai/api/paas/v4` | `ZAI_API_KEY`, `GLM_API_KEY` |
//! | [`Provider::Ollama`] | the same shape, locally | `http://127.0.0.1:11434/v1` | none |
//! | [`Provider::Anthropic`] | Messages API | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
//!
//! [`Provider::OpenAiCompatible`] is the same code as `Glm` with no defaults
//! filled in, for an endpoint this table does not name.
//!
//! # Structured output is requested and never relied on
//!
//! `models.dev` reports that `glm-5.3-flash` supports structured output; Z.ai's
//! own documentation does not describe the syntax, and this round had no key to
//! settle it with. So the request carries `response_format` when
//! [`crate::llm::LlmConfig::request_json_object`] is set — an operator can turn
//! it off in one line if their endpoint rejects it — and
//! [`crate::llm::prompt::parse_reply`] extracts JSON out of whatever comes back
//! either way. Nothing depends on the field being honoured.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::secret::{bound_message, Secret};
use super::transport::{HeaderValue, HttpRequest, HttpResponse};
use super::{LlmConfig, LlmError, Price};

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// Tokens in and out of one call.
///
/// Reported by the provider, not estimated: the whole point of the accounting is
/// that the operator does not have to trust an estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
}

impl Usage {
    /// Adds another call's usage.
    pub fn add(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }

    /// True when the provider reported nothing.
    ///
    /// Worth knowing: a provider that returns no `usage` block makes the cost
    /// column an estimate again, and the report should say so rather than print
    /// `$0.00`.
    pub fn is_zero(self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0
    }
}

/// One model answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReply {
    /// The assistant's text, exactly as returned.
    pub text: String,
    /// What it cost, as the provider counted it.
    pub usage: Usage,
    /// Why generation stopped, when the provider says. `"length"` and
    /// `"refusal"` both mean the answer may be unusable, and the report should
    /// be able to tell them apart from a parse failure.
    pub stop_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

/// Which wire shape to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    /// `GLM` on Z.ai. `OpenAI`-compatible chat completions. The shipped default.
    Glm,
    /// A local Ollama, through its `OpenAI`-compatible endpoint.
    Ollama,
    /// Anthropic's Messages API.
    Anthropic,
    /// Any other `OpenAI`-compatible endpoint. No defaults.
    OpenAiCompatible,
}

impl Provider {
    /// A stable name for reports and configuration.
    pub fn name(self) -> &'static str {
        match self {
            Self::Glm => "glm",
            Self::Ollama => "ollama",
            Self::Anthropic => "anthropic",
            Self::OpenAiCompatible => "openai-compatible",
        }
    }

    /// The path appended to [`crate::llm::LlmConfig::base_url`].
    pub fn path(self) -> &'static str {
        match self {
            Self::Glm | Self::Ollama | Self::OpenAiCompatible => "/chat/completions",
            Self::Anthropic => "/v1/messages",
        }
    }

    /// The model id to start from.
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Glm => "glm-5.3-flash",
            Self::Ollama => "llama3.1",
            Self::Anthropic => "claude-opus-5",
            Self::OpenAiCompatible => "",
        }
    }

    /// The API root to start from.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Glm => "https://api.z.ai/api/paas/v4",
            Self::Ollama => "http://127.0.0.1:11434/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::OpenAiCompatible => "",
        }
    }

    /// Environment variable **names** to read a key from, in order.
    pub fn default_key_env(self) -> &'static [&'static str] {
        match self {
            Self::Glm => &["ZAI_API_KEY", "GLM_API_KEY"],
            Self::Anthropic => &["ANTHROPIC_API_KEY"],
            // A local endpoint needs no key, and asking for one is how a
            // perfectly working local setup gets reported as "no key".
            Self::Ollama | Self::OpenAiCompatible => &[],
        }
    }

    /// The price to start from. Wrong the day a provider changes it, which is
    /// why it is configuration and why the report prints the price it used.
    pub fn default_price(self) -> Price {
        match self {
            Self::Glm => Price::GLM_FLASH_PROMO,
            // Claude Opus 5: $5 / $25 per million, per the Anthropic model
            // table. Override it in `.polis/llm.json` for a cheaper model.
            Self::Anthropic => Price {
                usd_per_m_input: 5.0,
                usd_per_m_output: 25.0,
            },
            // A local endpoint costs nothing, and an unnamed one has no price
            // to guess at; both report zero rather than a fiction.
            Self::Ollama | Self::OpenAiCompatible => Price::FREE,
        }
    }

    /// The implementation for this shape.
    pub fn client(self) -> Box<dyn ChatProvider> {
        match self {
            Self::Anthropic => Box::new(AnthropicProvider),
            Self::Glm | Self::Ollama | Self::OpenAiCompatible => {
                Box::new(OpenAiCompatibleProvider(self))
            }
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One chat completion, in whatever shape the endpoint speaks.
pub trait ChatProvider: std::fmt::Debug + Send + Sync {
    /// Which shape this is.
    fn provider(&self) -> Provider;

    /// Builds the request.
    ///
    /// `key` is `None` for an endpoint that needs none. A provider whose
    /// endpoint *does* need one returns [`LlmError::NoKey`] here, which is the
    /// single place that decision is made.
    fn request(
        &self,
        config: &LlmConfig,
        key: Option<&Arc<Secret>>,
        system: &str,
        user: &str,
    ) -> Result<HttpRequest, LlmError>;

    /// Turns a response into text and a token count.
    fn parse(&self, response: &HttpResponse) -> Result<ChatReply, LlmError>;
}

/// The maximum answer length asked for, in tokens.
///
/// A batch of eight districts at a 70-character label and a 240-character
/// detail is well under a thousand tokens; the headroom is for a model that
/// pads its JSON. Anthropic requires the field, and `OpenAI`-compatible
/// endpoints treat it as a ceiling.
const MAX_OUTPUT_TOKENS: u32 = 4096;

/// Deterministic-ish sampling. Descriptions are a labelling task, not a
/// creative one, and a low temperature is what stops the same district getting
/// a different sentence every regeneration.
const TEMPERATURE: f32 = 0.2;

// ---------------------------------------------------------------------------
// OpenAI-compatible
// ---------------------------------------------------------------------------

/// Chat completions: Z.ai, Ollama, and anything else that speaks the shape.
#[derive(Debug, Clone, Copy)]
pub struct OpenAiCompatibleProvider(Provider);

impl OpenAiCompatibleProvider {
    /// A client for an endpoint with no shipped defaults.
    pub fn new() -> Self {
        Self(Provider::OpenAiCompatible)
    }
}

impl Default for OpenAiCompatibleProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatProvider for OpenAiCompatibleProvider {
    fn provider(&self) -> Provider {
        self.0
    }

    fn request(
        &self,
        config: &LlmConfig,
        key: Option<&Arc<Secret>>,
        system: &str,
        user: &str,
    ) -> Result<HttpRequest, LlmError> {
        config.validate()?;
        let mut body = serde_json::json!({
            "model": config.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "temperature": TEMPERATURE,
            "max_tokens": MAX_OUTPUT_TOKENS,
            "stream": false,
        });
        if config.request_json_object {
            body["response_format"] = serde_json::json!({"type": "json_object"});
        }
        // Only when the caller asked: an endpoint that does not know the field
        // rejects the whole request rather than ignoring it.
        if let Some(effort) = &config.reasoning_effort {
            body["reasoning_effort"] = serde_json::json!(effort);
        }
        let mut headers = Vec::new();
        match key {
            Some(key) => headers.push((
                "Authorization".to_owned(),
                HeaderValue::bearer(Arc::clone(key)),
            )),
            None if config.key_env.is_empty() => {}
            None => return Err(LlmError::NoKey(config.key_source())),
        }
        Ok(HttpRequest {
            url: config.endpoint(),
            headers,
            body: serde_json::to_string(&body)
                .map_err(|e| LlmError::Config(format!("could not serialise the request: {e}")))?,
            timeout: std::time::Duration::from_secs(config.timeout_secs),
        })
    }

    fn parse(&self, response: &HttpResponse) -> Result<ChatReply, LlmError> {
        let value: serde_json::Value = serde_json::from_str(&response.body).map_err(|e| {
            if response.is_success() {
                LlmError::Malformed(format!("{e}: {}", bound_message(&response.body, 200)))
            } else {
                // A non-JSON error body is common — a proxy's HTML 502 page.
                LlmError::Status {
                    status: response.status,
                    message: bound_message(&response.body, 200),
                }
            }
        })?;
        if !response.is_success() {
            return Err(LlmError::Status {
                status: response.status,
                message: bound_message(&error_message(&value), 200),
            });
        }
        // Some gateways answer 200 with an error object.
        if value.get("choices").is_none() {
            if value.get("error").is_some() {
                return Err(LlmError::Status {
                    status: response.status,
                    message: bound_message(&error_message(&value), 200),
                });
            }
            return Err(LlmError::Malformed("no choices in the response".to_owned()));
        }
        let choice = value
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| LlmError::Malformed("choices is empty".to_owned()))?;
        let text = choice
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| LlmError::Malformed("no message content".to_owned()))?
            .to_owned();
        let usage = value.get("usage").map_or(Usage::default(), |u| Usage {
            input_tokens: u
                .get("prompt_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            output_tokens: u
                .get("completion_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        });
        Ok(ChatReply {
            text,
            usage,
            stop_reason: choice
                .get("finish_reason")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        })
    }
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

/// Anthropic's Messages API: `POST /v1/messages`, `x-api-key`,
/// `anthropic-version`.
///
/// Shipped so that "Anthropic drops in as config, not as a rewrite" is a fact
/// rather than an intention. It is **not verified against the live API in this
/// round** — no `ANTHROPIC_API_KEY` was present — and the report says so. The
/// request and response shapes come from Anthropic's own raw-HTTP reference.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicProvider;

/// The Messages API version header. A pinned date, not "latest": that is the
/// entire point of the header.
const ANTHROPIC_VERSION: &str = "2023-06-01";

impl ChatProvider for AnthropicProvider {
    fn provider(&self) -> Provider {
        Provider::Anthropic
    }

    fn request(
        &self,
        config: &LlmConfig,
        key: Option<&Arc<Secret>>,
        system: &str,
        user: &str,
    ) -> Result<HttpRequest, LlmError> {
        config.validate()?;
        let body = serde_json::json!({
            "model": config.model,
            "max_tokens": MAX_OUTPUT_TOKENS,
            "system": system,
            "messages": [{"role": "user", "content": user}],
        });
        let mut headers = vec![(
            "anthropic-version".to_owned(),
            HeaderValue::Plain(ANTHROPIC_VERSION.to_owned()),
        )];
        match key {
            Some(key) => headers.push((
                "x-api-key".to_owned(),
                HeaderValue::raw_secret(Arc::clone(key)),
            )),
            None if config.key_env.is_empty() => {}
            None => return Err(LlmError::NoKey(config.key_source())),
        }
        Ok(HttpRequest {
            url: config.endpoint(),
            headers,
            body: serde_json::to_string(&body)
                .map_err(|e| LlmError::Config(format!("could not serialise the request: {e}")))?,
            timeout: std::time::Duration::from_secs(config.timeout_secs),
        })
    }

    fn parse(&self, response: &HttpResponse) -> Result<ChatReply, LlmError> {
        let value: serde_json::Value = serde_json::from_str(&response.body).map_err(|e| {
            if response.is_success() {
                LlmError::Malformed(format!("{e}: {}", bound_message(&response.body, 200)))
            } else {
                LlmError::Status {
                    status: response.status,
                    message: bound_message(&response.body, 200),
                }
            }
        })?;
        if !response.is_success() {
            return Err(LlmError::Status {
                status: response.status,
                message: bound_message(&error_message(&value), 200),
            });
        }
        let stop_reason = value
            .get("stop_reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        // A refusal is a 200 with no usable content. It is a *model* outcome,
        // so it degrades like one: the district keeps its derived description.
        if stop_reason.as_deref() == Some("refusal") {
            return Err(LlmError::Unusable(
                "the model declined the request".to_owned(),
            ));
        }
        let text: String = value
            .get("content")
            .and_then(serde_json::Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .ok_or_else(|| LlmError::Malformed("no content blocks".to_owned()))?;
        let usage = value.get("usage").map_or(Usage::default(), |u| Usage {
            input_tokens: u
                .get("input_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            output_tokens: u
                .get("output_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        });
        Ok(ChatReply {
            text,
            usage,
            stop_reason,
        })
    }
}

/// Digs the human-readable half out of whatever error shape arrived.
///
/// `OpenAI`-compatible endpoints use `{"error": {"message": …}}`, Anthropic uses
/// the same, and gateways use half a dozen variants. When none of them match,
/// the whole body is the message — bounded by the caller.
fn error_message(value: &serde_json::Value) -> String {
    for path in [
        &["error", "message"][..],
        &["error", "msg"],
        &["message"],
        &["msg"],
        &["detail"],
    ] {
        let mut cursor = value;
        let mut ok = true;
        for key in path {
            let Some(next) = cursor.get(*key) else {
                ok = false;
                break;
            };
            cursor = next;
        }
        if ok {
            if let Some(text) = cursor.as_str() {
                return text.to_owned();
            }
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::transport::HttpResponse;

    fn config(provider: Provider) -> LlmConfig {
        LlmConfig::default().with_provider(provider)
    }

    /// A key, built the only way `Secret` allows: through the environment.
    ///
    /// `name` must be unique per test. `set_var` is process-global and the test
    /// harness runs tests in parallel, so two tests sharing one variable name
    /// race — one removes it between the other's set and read, and the read
    /// comes back empty. That was an intermittent failure here before the name
    /// became a parameter.
    fn key(name: &str, value: &str) -> Arc<Secret> {
        std::env::set_var(name, value);
        let secret = Secret::from_env(&[name.to_owned()]).expect("just set it");
        std::env::remove_var(name);
        Arc::new(secret)
    }

    #[test]
    fn the_glm_request_is_the_verified_z_ai_shape() {
        let config = config(Provider::Glm);
        let request = Provider::Glm
            .client()
            .request(
                &config,
                Some(&key("POLIS_TEST_KEY_GLM", "k-1")),
                "SYS",
                "USER",
            )
            .expect("a request");
        assert_eq!(request.url, "https://api.z.ai/api/paas/v4/chat/completions");
        let body: serde_json::Value = serde_json::from_str(&request.body).expect("json");
        assert_eq!(body["model"], "glm-5.3-flash");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "SYS");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "USER");
        assert_eq!(body["stream"], false);
        assert_eq!(body["response_format"]["type"], "json_object");
        // The key is a Bearer header and it is not in the Debug.
        let debug = format!("{request:?}");
        assert!(!debug.contains("k-1"), "{debug}");
        assert!(!request.body.contains("k-1"), "never in the body");
    }

    #[test]
    fn structured_output_can_be_switched_off_without_a_rebuild() {
        let mut config = config(Provider::Glm);
        config.request_json_object = false;
        let request = Provider::Glm
            .client()
            .request(&config, Some(&key("POLIS_TEST_KEY_JSON", "k")), "s", "u")
            .expect("a request");
        let body: serde_json::Value = serde_json::from_str(&request.body).expect("json");
        assert!(body.get("response_format").is_none(), "{body}");
    }

    /// `reasoning_effort` is absent unless a caller asked for it, because an
    /// endpoint that has never heard of the field answers `400` rather than
    /// ignoring it — so the default must not send it, and a caller that has
    /// checked its endpoint must be able to.
    #[test]
    fn reasoning_effort_is_sent_only_when_it_is_configured() {
        let mut config = config(Provider::Glm);
        assert!(config.reasoning_effort.is_none(), "off by default");
        let bare = Provider::Glm
            .client()
            .request(&config, Some(&key("POLIS_TEST_KEY_EFFORT", "k")), "s", "u")
            .expect("a request");
        let body: serde_json::Value = serde_json::from_str(&bare.body).expect("json");
        assert!(body.get("reasoning_effort").is_none(), "{body}");

        config.reasoning_effort = Some("low".to_owned());
        let asked = Provider::Glm
            .client()
            .request(&config, Some(&key("POLIS_TEST_KEY_EFFORT", "k")), "s", "u")
            .expect("a request");
        let body: serde_json::Value = serde_json::from_str(&asked.body).expect("json");
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn a_local_endpoint_needs_no_key_and_a_hosted_one_says_so() {
        let ollama = config(Provider::Ollama);
        let request = Provider::Ollama
            .client()
            .request(&ollama, None, "s", "u")
            .expect("no key needed");
        assert!(
            request.url.starts_with("http://127.0.0.1:11434"),
            "{request:?}"
        );
        assert!(request.headers.is_empty(), "{request:?}");

        let glm = config(Provider::Glm);
        let error = Provider::Glm
            .client()
            .request(&glm, None, "s", "u")
            .expect_err("a hosted endpoint needs a key");
        assert!(matches!(error, LlmError::NoKey(_)), "{error:?}");
        assert!(!error.is_retryable(), "no amount of retrying finds a key");
    }

    #[test]
    fn a_glm_response_yields_text_and_the_real_token_counts() {
        let body = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "{\"districts\":[]}"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1234, "completion_tokens": 56, "total_tokens": 1290}
        })
        .to_string();
        let reply = Provider::Glm
            .client()
            .parse(&HttpResponse { status: 200, body })
            .expect("a reply");
        assert_eq!(reply.text, "{\"districts\":[]}");
        assert_eq!(reply.usage.input_tokens, 1234);
        assert_eq!(reply.usage.output_tokens, 56);
        assert_eq!(reply.stop_reason.as_deref(), Some("stop"));
        assert!(!reply.usage.is_zero());
    }

    #[test]
    fn every_broken_response_shape_is_an_error_and_never_a_panic() {
        let client = Provider::Glm.client();
        let cases = [
            (200, "not json at all"),
            (200, "{}"),
            (200, r#"{"choices":[]}"#),
            (200, r#"{"choices":[{"message":{}}]}"#),
            (200, r#"{"error":{"message":"quota"}}"#),
            (401, r#"{"error":{"message":"invalid api key"}}"#),
            (429, r#"{"error":{"message":"rate limited"}}"#),
            (500, "<html>bad gateway</html>"),
            (200, r#"{"choices":[{"message":{"content":null}}]}"#),
        ];
        for (status, body) in cases {
            let error = client
                .parse(&HttpResponse {
                    status,
                    body: body.to_owned(),
                })
                .expect_err(body);
            // Every one of them must be describable without leaking the body.
            assert!(!error.tag().is_empty());
            let _ = error.to_string();
        }
        // And the retry decision is made on the status, not the text.
        let rate = client
            .parse(&HttpResponse {
                status: 429,
                body: r#"{"error":{"message":"rate limited"}}"#.to_owned(),
            })
            .expect_err("429");
        assert!(rate.is_retryable(), "{rate:?}");
        let auth = client
            .parse(&HttpResponse {
                status: 401,
                body: r#"{"error":{"message":"invalid api key"}}"#.to_owned(),
            })
            .expect_err("401");
        assert!(!auth.is_retryable(), "{auth:?}");
        assert!(auth.to_string().contains("invalid api key"), "{auth}");
    }

    #[test]
    fn the_anthropic_request_carries_the_version_header_and_a_bare_key() {
        let config = config(Provider::Anthropic);
        let request = Provider::Anthropic
            .client()
            .request(
                &config,
                Some(&key("POLIS_TEST_KEY_ANTHROPIC", "sk-ant-test")),
                "SYS",
                "USER",
            )
            .expect("a request");
        assert_eq!(request.url, "https://api.anthropic.com/v1/messages");
        let names: Vec<&str> = request.headers.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"anthropic-version"), "{names:?}");
        assert!(names.contains(&"x-api-key"), "{names:?}");
        let body: serde_json::Value = serde_json::from_str(&request.body).expect("json");
        assert_eq!(body["system"], "SYS");
        assert_eq!(body["messages"][0]["content"], "USER");
        assert!(body["max_tokens"].is_number(), "max_tokens is required");
        assert!(!format!("{request:?}").contains("sk-ant-test"));
    }

    #[test]
    fn an_anthropic_response_is_the_text_blocks_and_its_own_usage_names() {
        let body = serde_json::json!({
            "content": [{"type": "thinking", "thinking": ""},
                        {"type": "text", "text": "{\"districts\":[]}"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 900, "output_tokens": 40}
        })
        .to_string();
        let reply = Provider::Anthropic
            .client()
            .parse(&HttpResponse { status: 200, body })
            .expect("a reply");
        assert_eq!(reply.text, "{\"districts\":[]}");
        assert_eq!(reply.usage.input_tokens, 900);
        assert_eq!(reply.usage.output_tokens, 40);
    }

    #[test]
    fn an_anthropic_refusal_is_unusable_rather_than_malformed() {
        let body = serde_json::json!({
            "content": [], "stop_reason": "refusal",
            "usage": {"input_tokens": 10, "output_tokens": 0}
        })
        .to_string();
        let error = Provider::Anthropic
            .client()
            .parse(&HttpResponse { status: 200, body })
            .expect_err("a refusal");
        assert!(matches!(error, LlmError::Unusable(_)), "{error:?}");
        assert!(
            !error.is_retryable(),
            "a refusal is not a transient failure"
        );
    }

    #[test]
    fn every_provider_has_a_complete_and_distinct_set_of_defaults() {
        let all = [
            Provider::Glm,
            Provider::Ollama,
            Provider::Anthropic,
            Provider::OpenAiCompatible,
        ];
        let mut names: Vec<&str> = all.iter().map(|p| p.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len(), "duplicate provider name");
        for provider in all {
            // Round-trips through the config file.
            let json = serde_json::to_string(&provider).expect("serialize");
            let back: Provider = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, provider);
            let config = config(provider);
            if provider == Provider::OpenAiCompatible {
                assert!(config.validate().is_err(), "no defaults to validate");
            } else {
                assert!(
                    config.validate().is_ok(),
                    "{provider} has incomplete defaults"
                );
            }
        }
    }
}
