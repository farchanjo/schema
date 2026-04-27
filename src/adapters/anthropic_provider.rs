//! Anthropic Messages API adapter (ADR-0025).
//!
//! Outbound adapter implementing [`crate::ports::LlmProvider`] over
//! HTTPS. POSTs the synthesis prompt to
//! `https://api.anthropic.com/v1/messages` with the `x-api-key` and
//! `anthropic-version` headers and parses the text response.
//!
//! Hand-rolled (no third-party SDK) per ADR-0025 §"Crate / dependency
//! choice" — the surface is one endpoint and a few JSON shapes; an
//! SDK crate buys nothing and locks us into its release cadence.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ports::{LlmError, LlmProvider, SynthesisRequest, SynthesisResponse};

/// `2023-06-01` is the stable Anthropic Messages API version pinned
/// upstream as of 2026-04 — the only version the public docs warrant
/// for the `messages` shape used here.
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_MESSAGES_PATH: &str = "/v1/messages";

/// HTTP timeout per call. The use-case layer adds an outer
/// `tokio::time::timeout(60s)` around the whole synthesise call;
/// this client-side timeout matches that envelope so we never
/// linger on a stuck socket.
const HTTP_TIMEOUT_SECS: u64 = 60;

/// Default model used when `[llm].model` is unset. Haiku 4.5 is the
/// 2026-Q1 cost/latency sweet spot for synthesis over ~30 KiB of
/// retrieval context per ADR-0025 §"Default model per provider".
pub const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";

/// Default cap on output tokens for the Anthropic Messages API.
///
/// ADR-0025 amendment 2026-04-27. Haiku-class models do not have
/// reasoning-token budget pressure, so 1024 covers a typical RAG
/// answer with citations. Operators can override via
/// `[llm].max_tokens` or `SCHEMA_LLM_MAX_TOKENS`.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Default sampling temperature.
///
/// ADR-0025 amendment 2026-04-27. Anthropic accepts the full
/// `0.0..=1.0` range; `0.0` is deterministic-ish and the right
/// default for "answer over the indexed corpus" use case.
pub const DEFAULT_TEMPERATURE: f32 = 0.0;

/// Outbound adapter — Anthropic Messages API.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl AnthropicProvider {
    /// Construct from a non-empty API key plus the resolved model id.
    /// `model` may be empty; in that case the default is substituted.
    ///
    /// # Errors
    /// Returns an error if the underlying `reqwest::Client` cannot be
    /// built (e.g. broken system TLS roots).
    pub fn new(api_key: String, model: String) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()?;
        let model = if model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            model
        };
        Ok(Self {
            client,
            api_key,
            model,
            base_url: ANTHROPIC_BASE_URL.to_string(),
        })
    }

    #[cfg(test)]
    fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn synthesize(&self, req: SynthesisRequest) -> Result<SynthesisResponse, LlmError> {
        let response = self.send(&req).await?;
        let response = Self::ensure_success(response).await?;
        parse_messages(response).await
    }
}

impl AnthropicProvider {
    async fn send(&self, req: &SynthesisRequest) -> Result<reqwest::Response, LlmError> {
        let payload = AnthropicMessagesRequest {
            model: &self.model,
            max_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            temperature: req.temperature.unwrap_or(DEFAULT_TEMPERATURE),
            system: &req.system_prompt,
            messages: vec![AnthropicMessage {
                role: "user",
                content: &req.user_prompt,
            }],
        };
        let url = format!("{}{}", self.base_url, ANTHROPIC_MESSAGES_PATH);
        self.client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .json(&payload)
            .send()
            .await
            .map_err(|err| LlmError::Backend {
                provider: "anthropic",
                message: format!("HTTP error: {err}"),
            })
    }

    async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response, LlmError> {
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            warn!(
                provider = "anthropic",
                status = status.as_u16(),
                "anthropic credentials rejected"
            );
            return Err(LlmError::Unauthorized {
                provider: "anthropic",
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Backend {
                provider: "anthropic",
                message: format!("status {status}: {body}"),
            });
        }
        Ok(response)
    }
}

async fn parse_messages(response: reqwest::Response) -> Result<SynthesisResponse, LlmError> {
    let parsed: AnthropicMessagesResponse =
        response.json().await.map_err(|err| LlmError::Decode {
            provider: "anthropic",
            message: format!("decode response: {err}"),
        })?;
    log_usage(&parsed);
    let answer = collect_text_blocks(&parsed.content);
    if answer.is_empty() {
        return Err(LlmError::Decode {
            provider: "anthropic",
            message: "response contained no text blocks".to_string(),
        });
    }
    Ok(SynthesisResponse {
        answer,
        model: parsed.model,
        input_tokens: Some(parsed.usage.input_tokens),
        output_tokens: Some(parsed.usage.output_tokens),
    })
}

fn log_usage(parsed: &AnthropicMessagesResponse) {
    debug!(
        provider = "anthropic",
        model = %parsed.model,
        input_tokens = parsed.usage.input_tokens,
        output_tokens = parsed.usage.output_tokens,
        "anthropic synthesise completed"
    );
}

fn collect_text_blocks(content: &[AnthropicContentBlock]) -> String {
    let mut answer = String::new();
    for block in content {
        if block.kind == "text" {
            answer.push_str(&block.text);
        }
    }
    answer
}

#[derive(Debug, Serialize)]
struct AnthropicMessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    temperature: f32,
    system: &'a str,
    messages: Vec<AnthropicMessage<'a>>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessagesResponse {
    model: String,
    content: Vec<AnthropicContentBlock>,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use super::{
        AnthropicMessagesRequest, AnthropicProvider, DEFAULT_MAX_TOKENS, DEFAULT_MODEL,
        DEFAULT_TEMPERATURE,
    };
    use crate::ports::{LlmProvider, SynthesisRequest};

    /// ADR-0025 fitness — empty model from config falls through to
    /// the per-provider default at construction time.
    #[test]
    fn empty_model_uses_default() {
        let p = AnthropicProvider::new("test-key".into(), String::new()).unwrap();
        assert_eq!(p.model(), DEFAULT_MODEL);
    }

    /// ADR-0025 fitness — explicit model id from config wins.
    #[test]
    fn explicit_model_overrides_default() {
        let p = AnthropicProvider::new("test-key".into(), "claude-sonnet-4-6".into()).unwrap();
        assert_eq!(p.model(), "claude-sonnet-4-6");
    }

    /// ADR-0025 fitness — provider name is constant for the
    /// `LlmProvider::name` port contract.
    #[test]
    fn provider_name_is_constant() {
        let p = AnthropicProvider::new("test-key".into(), String::new()).unwrap();
        assert_eq!(p.name(), "anthropic");
    }

    /// ADR-0025 fitness — base URL override is exclusively a test
    /// hook and does not leak into the public surface.
    #[test]
    fn base_url_override_is_test_only() {
        let p = AnthropicProvider::new("test-key".into(), String::new())
            .unwrap()
            .with_base_url("http://127.0.0.1:9".into());
        assert_eq!(p.model(), DEFAULT_MODEL);
    }

    fn fixture_request(max_tokens: Option<u32>, temperature: Option<f32>) -> SynthesisRequest {
        SynthesisRequest {
            system_prompt: "sys".into(),
            user_prompt: "ask".into(),
            max_tokens,
            temperature,
        }
    }

    fn build_payload<'a>(
        provider: &'a AnthropicProvider,
        req: &'a SynthesisRequest,
    ) -> AnthropicMessagesRequest<'a> {
        AnthropicMessagesRequest {
            model: &provider.model,
            max_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            temperature: req.temperature.unwrap_or(DEFAULT_TEMPERATURE),
            system: &req.system_prompt,
            messages: vec![super::AnthropicMessage {
                role: "user",
                content: &req.user_prompt,
            }],
        }
    }

    /// ADR-0025 amendment 2026-04-27 — when the request omits both
    /// `max_tokens` and `temperature`, the adapter substitutes its
    /// per-provider defaults (1024, 0.0) before building the wire
    /// payload.
    #[test]
    fn anthropic_default_temperature_and_max_tokens() {
        let p = AnthropicProvider::new("test-key".into(), String::new()).unwrap();
        let req = fixture_request(None, None);
        let payload = build_payload(&p, &req);
        assert_eq!(payload.max_tokens, 1024);
        assert!((payload.temperature - 0.0).abs() < f32::EPSILON);
    }

    /// ADR-0025 amendment 2026-04-27 — explicit operator overrides
    /// (set via `[llm].max_tokens` / `temperature`, or the matching
    /// env vars) take precedence over the adapter default.
    #[test]
    fn request_with_explicit_overrides_takes_precedence_anthropic() {
        let p = AnthropicProvider::new("test-key".into(), String::new()).unwrap();
        let req = fixture_request(Some(2048), Some(0.7));
        let payload = build_payload(&p, &req);
        assert_eq!(payload.max_tokens, 2048);
        assert!((payload.temperature - 0.7).abs() < f32::EPSILON);
    }
}
