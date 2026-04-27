//! `OpenAI` Chat Completions API adapter (ADR-0025).
//!
//! Outbound adapter implementing [`crate::ports::LlmProvider`] over
//! HTTPS. POSTs the synthesis prompt to
//! `https://api.openai.com/v1/chat/completions` with a Bearer token
//! and parses the chat completion response.
//!
//! Hand-rolled, mirrors `anthropic_provider.rs` design — see ADR-0025
//! §"Crate / dependency choice" for the no-SDK rationale.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ports::{LlmError, LlmProvider, SynthesisRequest, SynthesisResponse};

const OPENAI_BASE_URL: &str = "https://api.openai.com";
const OPENAI_CHAT_PATH: &str = "/v1/chat/completions";

const HTTP_TIMEOUT_SECS: u64 = 60;

/// Default model used when `[llm].model` is unset. `gpt-5` is the
/// 2026 default per ADR-0025 §"Default model per provider"; if the
/// user wants a smaller/cheaper model they pin via `[llm].model`.
pub const DEFAULT_MODEL: &str = "gpt-5";

/// Default cap on output tokens for the `OpenAI` Chat Completions API.
///
/// ADR-0025 amendment 2026-04-27. Higher than Anthropic's 1024
/// because gpt-5 reasoning tokens are deducted from
/// `max_completion_tokens` — at 1024 the reasoning trace can eat
/// the entire budget and leave `choices[0].message.content` empty
/// (observed against `gpt-5-2025-08-07`). Operators on smaller
/// models (`gpt-4o`, `gpt-4.1`) may want to override down via
/// `[llm].max_tokens` or `SCHEMA_LLM_MAX_TOKENS` to reduce cost.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Default sampling temperature.
///
/// ADR-0025 amendment 2026-04-27. `gpt-5` rejects every value
/// other than `1.0` with HTTP 400 (`Unsupported value: 'temperature'
/// does not support 0.0 with this model`); older `OpenAI` chat
/// models accept the full `0.0..=2.0` range. The default is set
/// to `1.0` so the out-of-the-box experience works with the
/// default model; operators on `gpt-4o` / `gpt-4.1` can override
/// to `0.0` for deterministic-ish behaviour.
pub const DEFAULT_TEMPERATURE: f32 = 1.0;

/// Outbound adapter — `OpenAI` Chat Completions API.
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAiProvider {
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
            base_url: OPENAI_BASE_URL.to_string(),
        })
    }

    #[cfg(test)]
    fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn synthesize(&self, req: SynthesisRequest) -> Result<SynthesisResponse, LlmError> {
        let response = self.send(&req).await?;
        let response = Self::ensure_success(response).await?;
        parse_completion(response).await
    }
}

impl OpenAiProvider {
    async fn send(&self, req: &SynthesisRequest) -> Result<reqwest::Response, LlmError> {
        let payload = OpenAiChatRequest {
            model: &self.model,
            max_completion_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            temperature: req.temperature.unwrap_or(DEFAULT_TEMPERATURE),
            messages: vec![
                OpenAiMessage {
                    role: "system",
                    content: &req.system_prompt,
                },
                OpenAiMessage {
                    role: "user",
                    content: &req.user_prompt,
                },
            ],
        };
        let url = format!("{}{}", self.base_url, OPENAI_CHAT_PATH);
        self.client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|err| LlmError::Backend {
                provider: "openai",
                message: format!("HTTP error: {err}"),
            })
    }

    async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response, LlmError> {
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            warn!(
                provider = "openai",
                status = status.as_u16(),
                "openai credentials rejected"
            );
            return Err(LlmError::Unauthorized {
                provider: "openai",
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Backend {
                provider: "openai",
                message: format!("status {status}: {body}"),
            });
        }
        Ok(response)
    }
}

async fn parse_completion(response: reqwest::Response) -> Result<SynthesisResponse, LlmError> {
    let parsed: OpenAiChatResponse = response.json().await.map_err(|err| LlmError::Decode {
        provider: "openai",
        message: format!("decode response: {err}"),
    })?;
    log_usage(&parsed);
    let answer = first_choice_content(&parsed);
    if answer.is_empty() {
        return Err(LlmError::Decode {
            provider: "openai",
            message: "response contained no choices".to_string(),
        });
    }
    let (input_tokens, output_tokens) = parsed.usage.map_or((None, None), |u| {
        (Some(u.prompt_tokens), Some(u.completion_tokens))
    });
    Ok(SynthesisResponse {
        answer,
        model: parsed.model,
        input_tokens,
        output_tokens,
    })
}

fn log_usage(parsed: &OpenAiChatResponse) {
    debug!(
        provider = "openai",
        model = %parsed.model,
        input_tokens = parsed.usage.as_ref().map_or(0, |u| u.prompt_tokens),
        output_tokens = parsed.usage.as_ref().map_or(0, |u| u.completion_tokens),
        "openai synthesise completed"
    );
}

fn first_choice_content(parsed: &OpenAiChatResponse) -> String {
    parsed
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .unwrap_or_default()
}

#[derive(Debug, Serialize)]
struct OpenAiChatRequest<'a> {
    model: &'a str,
    max_completion_tokens: u32,
    temperature: f32,
    messages: Vec<OpenAiMessage<'a>>,
}

#[derive(Debug, Serialize)]
struct OpenAiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatResponse {
    model: String,
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiResponseMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponseMessage {
    #[serde(default)]
    content: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test fixtures may panic if the env is broken"
    )]

    use super::{
        DEFAULT_MAX_TOKENS, DEFAULT_MODEL, DEFAULT_TEMPERATURE, OpenAiChatRequest, OpenAiProvider,
    };
    use crate::ports::{LlmProvider, SynthesisRequest};

    /// ADR-0025 fitness — empty model from config falls through to
    /// the per-provider default at construction time.
    #[test]
    fn empty_model_uses_default() {
        let p = OpenAiProvider::new("test-key".into(), String::new()).unwrap();
        assert_eq!(p.model(), DEFAULT_MODEL);
    }

    /// ADR-0025 fitness — explicit model id from config wins.
    #[test]
    fn explicit_model_overrides_default() {
        let p = OpenAiProvider::new("test-key".into(), "gpt-4o".into()).unwrap();
        assert_eq!(p.model(), "gpt-4o");
    }

    /// ADR-0025 fitness — provider name is constant for the
    /// `LlmProvider::name` port contract.
    #[test]
    fn provider_name_is_constant() {
        let p = OpenAiProvider::new("test-key".into(), String::new()).unwrap();
        assert_eq!(p.name(), "openai");
    }

    /// ADR-0025 fitness — base URL override is exclusively a test
    /// hook and does not leak into the public surface.
    #[test]
    fn base_url_override_is_test_only() {
        let p = OpenAiProvider::new("test-key".into(), String::new())
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
        provider: &'a OpenAiProvider,
        req: &'a SynthesisRequest,
    ) -> OpenAiChatRequest<'a> {
        OpenAiChatRequest {
            model: &provider.model,
            max_completion_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            temperature: req.temperature.unwrap_or(DEFAULT_TEMPERATURE),
            messages: vec![
                super::OpenAiMessage {
                    role: "system",
                    content: &req.system_prompt,
                },
                super::OpenAiMessage {
                    role: "user",
                    content: &req.user_prompt,
                },
            ],
        }
    }

    /// ADR-0025 amendment 2026-04-27 — gpt-5 only accepts
    /// `temperature = 1.0` and reasoning tokens consume
    /// `max_completion_tokens`. The adapter's per-provider defaults
    /// (1.0, 8192) cover the gpt-5 happy path when the operator
    /// leaves `[llm].max_tokens` / `temperature` unset.
    #[test]
    fn openai_default_temperature_and_max_tokens() {
        let p = OpenAiProvider::new("test-key".into(), String::new()).unwrap();
        let req = fixture_request(None, None);
        let payload = build_payload(&p, &req);
        assert_eq!(payload.max_completion_tokens, 8192);
        assert!((payload.temperature - 1.0).abs() < f32::EPSILON);
    }

    /// ADR-0025 amendment 2026-04-27 — explicit operator overrides
    /// take precedence over the adapter default. Operators on
    /// non-reasoning models (`gpt-4o`) typically pin
    /// `temperature = 0.0` + a smaller `max_tokens`.
    #[test]
    fn request_with_explicit_overrides_takes_precedence_openai() {
        let p = OpenAiProvider::new("test-key".into(), "gpt-4o".into()).unwrap();
        let req = fixture_request(Some(512), Some(0.0));
        let payload = build_payload(&p, &req);
        assert_eq!(payload.max_completion_tokens, 512);
        assert!((payload.temperature - 0.0).abs() < f32::EPSILON);
    }
}
