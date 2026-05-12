//! `LlmError` — the typed error returned by [`AiModel::chat_completion`].
//!
//! Lives in the `llms` module because it's the AiModel trait's error
//! type, not a telemetry concept. The complementary [`LlmErrorClass`]
//! enum (the snake_case variant tag that lands in the
//! `LlmRequestFailed.error_class` payload) stays in
//! [`crate::telemetry`] since it's part of the operator-facing event
//! schema. [`LlmError::classify`] bridges the two.
//!
//! Each provider impl maps its native error (reqwest, serde_json,
//! HTTP status codes, async-openai's `OpenAIError::ApiError.code`)
//! into this enum at the trait boundary so callers can pattern-match
//! variants instead of scraping formatted error strings.

use crate::telemetry::LlmErrorClass;

/// Typed error produced at the [`AiModel`](crate::llms::AiModel) boundary.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("rate limited")]
    RateLimit {
        retry_after_ms: Option<u64>,
        status: u16,
    },
    #[error("payment required")]
    PaymentRequired { status: u16 },
    #[error("server error (status {status})")]
    ServerError { status: u16 },
    /// vLLM/OpenAI-compatible context-window-exceeded error after
    /// reactive shrink retries are exhausted. Carries the server-
    /// reported `limit` (model max context tokens) and `tokens` (the
    /// request's input-token count) so dashboards can chart how often
    /// agents are crossing which model's window without re-parsing
    /// the error message.
    #[error("context overflow ({tokens} input tokens exceeded {limit}-token model limit)")]
    ContextOverflow { tokens: u32, limit: u32 },
    #[error("transport")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error("parse")]
    Parse(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error("other")]
    Other(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl LlmError {
    /// Map a typed error to the telemetry taxonomy.
    pub fn classify(&self) -> (LlmErrorClass, Option<u16>) {
        match self {
            LlmError::RateLimit {
                retry_after_ms: _,
                status,
            } => (LlmErrorClass::RateLimit, Some(*status)),
            LlmError::PaymentRequired { status } => (LlmErrorClass::PaymentRequired, Some(*status)),
            LlmError::ServerError { status } => (LlmErrorClass::ServerError, Some(*status)),
            LlmError::ContextOverflow { .. } => (LlmErrorClass::ContextOverflow, None),
            LlmError::Transport(_) => (LlmErrorClass::Transport, None),
            LlmError::Parse(_) => (LlmErrorClass::Parse, None),
            LlmError::Other(_) => (LlmErrorClass::Other, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant maps to the telemetry class with the expected
    /// http_status. Locks the bridge between AiModel error types and
    /// the operator-facing telemetry taxonomy so a future variant
    /// rename or class addition fails CI rather than silently
    /// misclassifies in dashboards.
    #[test]
    fn classify_covers_every_variant_with_correct_status() {
        let cases: Vec<(LlmError, LlmErrorClass, Option<u16>)> = vec![
            (
                LlmError::RateLimit {
                    retry_after_ms: Some(1500),
                    status: 429,
                },
                LlmErrorClass::RateLimit,
                Some(429),
            ),
            (
                LlmError::PaymentRequired { status: 402 },
                LlmErrorClass::PaymentRequired,
                Some(402),
            ),
            (
                LlmError::ServerError { status: 503 },
                LlmErrorClass::ServerError,
                Some(503),
            ),
            (
                LlmError::ContextOverflow {
                    tokens: 9000,
                    limit: 8192,
                },
                LlmErrorClass::ContextOverflow,
                None,
            ),
            (
                LlmError::Transport(Box::new(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "reset",
                ))),
                LlmErrorClass::Transport,
                None,
            ),
            (
                LlmError::Parse(Box::new(std::io::Error::other("bad json"))),
                LlmErrorClass::Parse,
                None,
            ),
            (
                LlmError::Other(Box::new(std::io::Error::other("misc"))),
                LlmErrorClass::Other,
                None,
            ),
        ];
        for (err, want_class, want_status) in cases {
            let (got_class, got_status) = err.classify();
            assert_eq!(
                got_class, want_class,
                "variant {err:?} classified wrong: got {got_class:?}, want {want_class:?}"
            );
            assert_eq!(
                got_status, want_status,
                "variant {err:?} status wrong: got {got_status:?}, want {want_status:?}"
            );
        }
    }

    /// `ContextOverflow` is the only variant that carries diagnostic
    /// numbers but suppresses them from `classify` (the telemetry
    /// schema doesn't have a tokens/limit field on `LlmRequestFailed`).
    /// Lock that the variant is constructible with the expected fields
    /// and that Display includes both numbers.
    #[test]
    fn context_overflow_carries_tokens_and_limit() {
        let err = LlmError::ContextOverflow {
            tokens: 12_500,
            limit: 8_192,
        };
        let display = err.to_string();
        assert!(display.contains("12500"), "display omits tokens: {display}");
        assert!(display.contains("8192"), "display omits limit: {display}");
        // The numeric fields stay readable for non-classify consumers.
        if let LlmError::ContextOverflow { tokens, limit } = err {
            assert_eq!(tokens, 12_500);
            assert_eq!(limit, 8_192);
        } else {
            unreachable!("matched variant must extract fields");
        }
    }

    /// `From<LlmError> for anyhow::Error` is the bridge used by the
    /// agent's retry classifier (`downcast_ref::<LlmError>()`).
    /// Verify the type survives the wrap.
    #[test]
    fn anyhow_wrap_preserves_typed_error() {
        let err: anyhow::Error = LlmError::ServerError { status: 502 }.into();
        let downcast = err.downcast_ref::<LlmError>();
        assert!(matches!(
            downcast,
            Some(LlmError::ServerError { status: 502 })
        ));
    }
}
