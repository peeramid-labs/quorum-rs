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
    /// HTTP 400 that isn't a recognized context-overflow. The provider's
    /// response `body` carries the real reason (bad tool schema, token
    /// math, unsupported param). `Display` stays terse — the body can
    /// echo a fragment of the prompt — so logs/dumps don't leak it; the
    /// body is exposed only via [`LlmError::detail`], which operator-local
    /// surfaces (e.g. `quorum smoke-test`) opt into showing.
    #[error("bad request (status {status})")]
    BadRequest { status: u16, body: String },
    #[error("transport")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error("parse")]
    Parse(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error("other")]
    Other(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl LlmError {
    /// Render the full error chain (this error + every wrapped
    /// `source`) as a multi-line string, root cause last:
    ///
    /// ```text
    /// transport
    ///   caused by: hyper error
    ///   caused by: connection reset by peer (os error 104)
    /// ```
    ///
    /// `LlmError` only wraps a source on `Transport`, `Parse`, and
    /// `Other`; the structured variants (`RateLimit`,
    /// `PaymentRequired`, `ServerError`, `ContextOverflow`) render
    /// as a single line because they carry their detail inline in
    /// the `thiserror` format string.
    ///
    /// Operators see the chain in logs (`tracing::error!(error.chain
    /// = %err.display_chain(), ...)`) and dashboards (the JSON
    /// shape exposes one string field) without losing the root
    /// cause to a flattened one-liner.
    pub fn display_chain(&self) -> String {
        use std::error::Error as _;
        let mut out = self.to_string();
        let mut cursor: Option<&dyn std::error::Error> = self.source();
        while let Some(layer) = cursor {
            out.push_str("\n  caused by: ");
            out.push_str(&layer.to_string());
            cursor = layer.source();
        }
        out
    }

    /// Provider-supplied detail that `Display` deliberately withholds.
    /// Currently the captured HTTP 400 response body. Operator-local
    /// surfaces show this; logs/dumps keep using `Display`/`display_chain`
    /// so the body never leaks server-side.
    pub fn detail(&self) -> Option<&str> {
        match self {
            LlmError::BadRequest { body, .. } => Some(body),
            _ => None,
        }
    }

    /// Map a typed error to the telemetry taxonomy.
    pub fn classify(&self) -> (LlmErrorClass, Option<u16>) {
        match self {
            LlmError::BadRequest { status, .. } => (LlmErrorClass::Other, Some(*status)),
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
            (
                LlmError::BadRequest {
                    status: 400,
                    body: "{\"error\":\"bad schema\"}".to_string(),
                },
                LlmErrorClass::Other,
                Some(400),
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

    /// `BadRequest` keeps the provider body out of `Display`/`display_chain`
    /// (those reach logs + dumps and the body can echo the prompt) but
    /// exposes it via `detail()` for operator-local surfaces.
    #[test]
    fn bad_request_body_hidden_from_display_exposed_via_detail() {
        let body =
            r#"{"error":{"message":"'max_tokens' too large: 16000 > 21000 - 5395","code":400}}"#;
        let err = LlmError::BadRequest {
            status: 400,
            body: body.to_string(),
        };
        assert_eq!(err.to_string(), "bad request (status 400)");
        assert!(
            !err.display_chain().contains("max_tokens"),
            "display_chain must not leak the body: {}",
            err.display_chain()
        );
        assert_eq!(err.detail(), Some(body));
        // Non-BadRequest variants carry no detail.
        assert_eq!(LlmError::ServerError { status: 500 }.detail(), None);
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

    /// Single-layer variant (no `#[source]` wrapper) renders as the
    /// thiserror `Display` text alone.
    #[test]
    fn display_chain_single_layer_emits_one_line() {
        let chain = LlmError::ServerError { status: 503 }.display_chain();
        assert_eq!(chain, "server error (status 503)");
        assert!(
            !chain.contains("caused by"),
            "single-layer variant must not include a `caused by` line: {chain}"
        );
    }

    /// Wrapped variant walks one `source` level and renders the
    /// inner error on a `caused by` line.
    #[test]
    fn display_chain_two_layers_renders_caused_by() {
        let inner = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset by peer");
        let err = LlmError::Transport(Box::new(inner));
        let chain = err.display_chain();
        assert_eq!(chain, "transport\n  caused by: reset by peer");
    }

    /// Each `caused by` layer gets its own line; the root cause
    /// appears last. This locks the rendering convention that log
    /// shippers + dashboards depend on for stable splitting.
    #[test]
    fn display_chain_three_layers_walks_full_source_tree() {
        // Build a triple-nested chain by hand: inner io::Error,
        // wrapped in a SerdeJsonError-shaped layer, wrapped in
        // LlmError::Parse.
        #[derive(Debug)]
        struct MidLayer(Box<dyn std::error::Error + Send + Sync + 'static>);
        impl std::fmt::Display for MidLayer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "mid layer")
            }
        }
        impl std::error::Error for MidLayer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(self.0.as_ref())
            }
        }
        let root = std::io::Error::other("bad json byte");
        let mid = MidLayer(Box::new(root));
        let err = LlmError::Parse(Box::new(mid));
        let chain = err.display_chain();
        assert_eq!(
            chain,
            "parse\n  caused by: mid layer\n  caused by: bad json byte"
        );
    }

    /// Variants that carry detail in the format string (vs `#[source]`)
    /// still render their detail inline — the chain helper doesn't
    /// strip information for them.
    #[test]
    fn display_chain_preserves_inline_detail_on_structured_variants() {
        let chain = LlmError::ContextOverflow {
            tokens: 9000,
            limit: 8192,
        }
        .display_chain();
        assert!(chain.contains("9000"), "tokens missing: {chain}");
        assert!(chain.contains("8192"), "limit missing: {chain}");
    }
}
