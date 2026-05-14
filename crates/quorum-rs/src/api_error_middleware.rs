//! Reusable axum middleware that emits [`TelemetryEvent::ApiError`] on
//! every HTTP response with `status >= 400`. Mounts on any axum
//! router an agent crate exposes (status-server dashboard, MCP HTTP
//! front-end, etc.).
//!
//! Agent-side events omit `operator_principal` — agent dashboards
//! are typically loopback-bound and not authenticated as operators.
//! Orchestrators emitting the mirror event populate the principal
//! field from their own auth layer.

use std::sync::Arc;

use axum::{body::Body, extract::MatchedPath, http::Request, middleware::Next, response::Response};

use crate::telemetry::{ApiError, TelemetryContext, TelemetryEmitterMux, TelemetryEvent};

/// Bundle the emitter and a reusable [`TelemetryContext`] so the
/// middleware can build the event without per-request context
/// lookups. Status-server-style routes are non-task-scoped, so the
/// context's `job_id` / `round` / `phase` are `None`.
#[derive(Clone)]
pub struct ApiErrorTelemetry {
    pub emitter: TelemetryEmitterMux,
    pub ctx: TelemetryContext,
}

impl ApiErrorTelemetry {
    /// Build a bundle for an `agent_id`-scoped, non-task-bound
    /// telemetry context (the typical status-server case).
    pub fn new(emitter: TelemetryEmitterMux, agent_id: &str) -> Self {
        let ctx = TelemetryContext::new(agent_id, None, None, None);
        Self { emitter, ctx }
    }
}

/// Middleware entry point. Mount via
/// `axum::middleware::from_fn_with_state`. The state is a
/// `Option<Arc<ApiErrorTelemetry>>` so callers can pass `None` to
/// disable the layer at config time without a separate router shape.
pub async fn api_error_telemetry_middleware(
    axum::extract::State(state): axum::extract::State<Option<Arc<ApiErrorTelemetry>>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let start = std::time::Instant::now();
    let method = req.method().as_str().to_string();
    let endpoint = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());

    let resp = next.run(req).await;
    let status = resp.status().as_u16();

    if status >= 400
        && let Some(t) = state.as_ref()
    {
        t.emitter.emit(&TelemetryEvent::ApiError(ApiError {
            common: t.ctx.common(),
            http_status: status,
            error_code: None,
            endpoint,
            method,
            duration_ms: start.elapsed().as_millis() as u64,
        }));
    }

    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn endpoint_label_falls_back_to_raw_path() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/some/path?q=1")
            .body(Body::empty())
            .unwrap();
        let label = req
            .extensions()
            .get::<MatchedPath>()
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| req.uri().path().to_string());
        assert_eq!(label, "/some/path");
    }
}
