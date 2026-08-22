//! Model-availability probe over a provider's model catalog.
//!
//! A roster pins per-agent `model_name`s on a provider. Providers retire and
//! rename models, and a dead id then fails *every* request. This probe fetches
//! the provider's model catalog — the standard OpenAI-compatible `/models` list
//! (`{ "data": [ { "id": ... }, ... ] }`) — and answers
//! [`ModelAvailability::is_available`] so serving/selection surfaces can
//! auto-exclude an agent whose model is gone, and restore it when the model
//! returns, without a human editing config.
//!
//! Both the catalog URL and the provider the catalog covers are supplied by the
//! caller (config-driven) — nothing here is tied to a specific vendor.
//!
//! Fail-open is the invariant: only a *successful* catalog that omits an id
//! reports [`Availability::Unavailable`]. A fetch failure, an agent on a
//! different provider, or a not-yet-refreshed cache reports
//! [`Availability::Unknown`] so a network blip can never mass-deactivate the
//! fleet.

use std::collections::HashSet;
use std::sync::RwLock;

/// Availability verdict for a `(provider_id, model_name)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// The model is present in the last successfully fetched catalog.
    Available,
    /// The catalog was fetched successfully and does NOT contain the model.
    Unavailable,
    /// No verdict: the agent is on a provider this probe doesn't cover, the
    /// catalog was never fetched, or the last fetch failed. Callers MUST treat
    /// this as "keep serving" (fail-open).
    Unknown,
}

/// Fetches and caches a provider's model catalog to answer availability queries.
///
/// The cache holds the id set from the last *successful* fetch; a failed
/// [`ModelAvailability::refresh`] leaves the previous set intact (fail-open) and
/// surfaces the error. A background poller drives `refresh` on an interval;
/// `is_available` is a cheap synchronous read.
#[derive(Debug)]
pub struct ModelAvailability {
    client: reqwest::Client,
    /// The provider's model-catalog endpoint (OpenAI-compatible `/models`).
    catalog_url: String,
    /// The provider id whose agents this catalog covers. Agents on any other
    /// provider resolve to [`Availability::Unknown`] (fail-open).
    provider_id: String,
    /// `None` until the first successful fetch; then the set of catalog model ids.
    ids: RwLock<Option<HashSet<String>>>,
    /// Optional endpoint for confirming the provider will actually serve a
    /// model, not merely list it. `None` = catalog-only.
    serving: Option<ServingProbe>,
}

/// Where to send a minimal completion to confirm a model is really served.
#[derive(Debug)]
struct ServingProbe {
    chat_url: String,
    api_key: String,
}

impl ModelAvailability {
    /// Also confirm the provider will serve a model, not only list it.
    ///
    /// A catalog is an advertisement. A provider can list an id whose endpoint
    /// answers 404, and then a catalog-only verdict reports an agent healthy
    /// while every task it is given fails — which is exactly what hid a dead
    /// seat for two days.
    pub fn with_serving_probe(mut self, chat_url: String, api_key: String) -> Self {
        self.serving = Some(ServingProbe { chat_url, api_key });
        self
    }

    /// Ask the endpoint for one token and read the answer as a verdict.
    ///
    /// [`Availability::Unavailable`] ONLY when the endpoint says this model is
    /// not there (404). Every other outcome — a rate limit, an auth failure, a
    /// provider 500, a transport error, or no probe configured — is
    /// [`Availability::Unknown`], because the alternative is a provider hiccup
    /// benching the whole fleet at once.
    pub async fn probe_serving(&self, model_name: &str) -> Availability {
        let Some(serving) = self.serving.as_ref() else {
            return Availability::Unknown;
        };
        let body = serde_json::json!({
            "model": model_name,
            "messages": [{ "role": "user", "content": "ping" }],
            "max_tokens": 1,
        });
        let response = self
            .client
            .post(&serving.chat_url)
            .bearer_auth(&serving.api_key)
            .json(&body)
            .send()
            .await;
        match response {
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => Availability::Unavailable,
            Ok(r) if r.status().is_success() => Availability::Available,
            _ => Availability::Unknown,
        }
    }

    /// `catalog_url` is the provider's OpenAI-compatible `/models` endpoint;
    /// `provider_id` is the provider whose agents this catalog governs. Both are
    /// caller-supplied so the probe stays provider-agnostic.
    pub fn new(catalog_url: String, provider_id: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            catalog_url,
            provider_id,
            ids: RwLock::new(None),
            serving: None,
        }
    }

    /// Fetch the catalog and replace the cached id set. On any failure the
    /// previous set is kept (fail-open) and the error is returned.
    pub async fn refresh(&self) -> Result<usize, String> {
        let body = self
            .client
            .get(&self.catalog_url)
            .send()
            .await
            .map_err(|e| format!("catalog request failed: {e}"))?;
        if !body.status().is_success() {
            return Err(format!("catalog returned status {}", body.status()));
        }
        let text = body.text().await.map_err(|e| e.to_string())?;
        let ids = parse_catalog_ids(&text)?;
        let count = ids.len();
        *self
            .ids
            .write()
            .map_err(|_| "model-availability lock poisoned".to_string())? = Some(ids);
        Ok(count)
    }

    /// Availability verdict for an agent's `(provider_id, model_name)`.
    ///
    /// Only agents on the provider this catalog covers are checked; everything
    /// else — and any state before a successful refresh — is
    /// [`Availability::Unknown`] (fail-open).
    pub fn is_available(&self, provider_id: &str, model_name: &str) -> Availability {
        if provider_id != self.provider_id {
            return Availability::Unknown;
        }
        // A poisoned lock is unrecoverable — fail open to Unknown rather than
        // panic, consistent with the never-mass-deactivate invariant.
        let Ok(guard) = self.ids.read() else {
            return Availability::Unknown;
        };
        match &*guard {
            None => Availability::Unknown,
            Some(ids) if ids.contains(model_name) => Availability::Available,
            Some(_) => Availability::Unavailable,
        }
    }
}

/// Extract `data[].id` from an OpenAI-compatible `/models` response.
fn parse_catalog_ids(body: &str) -> Result<HashSet<String>, String> {
    let catalog: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("catalog json parse: {e}"))?;
    let entries = catalog
        .get("data")
        .and_then(|data| data.as_array())
        .ok_or("catalog json missing `data` array")?;
    Ok(entries
        .iter()
        .filter_map(|model| {
            model
                .get("id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PROVIDER: &str = "prov";

    const CATALOG: &str = r#"{"data":[
        {"id":"vendor/model-a"},
        {"id":"vendor/model-b"},
        {"id":"vendor/model-c"}
    ]}"#;

    async fn serve(status: u16, body: &str) -> (MockServer, String) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body.to_string()))
            .mount(&server)
            .await;
        let url = format!("{}/v1/models", server.uri());
        (server, url)
    }

    #[test]
    fn before_refresh_everything_is_unknown() {
        // Fail-open: never deactivate an agent just because we haven't polled yet.
        let probe = ModelAvailability::new("http://unused".to_string(), PROVIDER.to_string());
        assert_eq!(
            probe.is_available(PROVIDER, "vendor/model-a"),
            Availability::Unknown
        );
    }

    #[tokio::test]
    async fn refresh_marks_present_available_and_absent_unavailable() {
        let (_server, url) = serve(200, CATALOG).await;
        let probe = ModelAvailability::new(url, PROVIDER.to_string());
        let n = probe.refresh().await.unwrap();
        assert_eq!(n, 3);

        assert_eq!(
            probe.is_available(PROVIDER, "vendor/model-b"),
            Availability::Available,
            "a live id is Available"
        );
        // A withdrawn id → Unavailable → agent auto-excluded.
        assert_eq!(
            probe.is_available(PROVIDER, "vendor/withdrawn"),
            Availability::Unavailable,
            "a withdrawn id is Unavailable"
        );
    }

    #[tokio::test]
    async fn failed_first_fetch_stays_unknown_never_unavailable() {
        // A 5xx on the very first fetch must NOT flip agents to Unavailable — a
        // network blip can't mass-deactivate the fleet.
        let (_server, url) = serve(500, "upstream down").await;
        let probe = ModelAvailability::new(url, PROVIDER.to_string());
        assert!(probe.refresh().await.is_err());
        assert_eq!(
            probe.is_available(PROVIDER, "vendor/some-model"),
            Availability::Unknown,
            "a failed first fetch stays Unknown (fail-open), never Unavailable"
        );
    }

    #[tokio::test]
    async fn failed_refresh_retains_last_good_catalog() {
        // Good fetch, then a failing refresh: the previous good set survives so a
        // transient outage doesn't drop a live agent.
        let (_ok_server, ok_url) = serve(200, CATALOG).await;
        let probe = ModelAvailability::new(ok_url, PROVIDER.to_string());
        probe.refresh().await.unwrap();

        let (_bad_server, bad_url) = serve(500, "upstream down").await;
        let probe = ModelAvailability::new(bad_url, PROVIDER.to_string());
        {
            // Simulate a poller that already holds a good catalog.
            *probe.ids.write().unwrap() = parse_catalog_ids(CATALOG).ok();
        }
        assert!(probe.refresh().await.is_err());
        assert_eq!(
            probe.is_available(PROVIDER, "vendor/model-b"),
            Availability::Available,
            "a failed refresh retains the previous good catalog"
        );
    }

    #[tokio::test]
    async fn other_provider_is_unknown() {
        let (_server, url) = serve(200, CATALOG).await;
        let probe = ModelAvailability::new(url, PROVIDER.to_string());
        probe.refresh().await.unwrap();
        // Only the covered provider is checked; others fail-open.
        assert_eq!(
            probe.is_available("some-other-provider", "vendor/model-a"),
            Availability::Unknown
        );
    }

    #[test]
    fn parse_catalog_ids_extracts_ids_and_rejects_garbage() {
        let ids = parse_catalog_ids(CATALOG).unwrap();
        assert!(ids.contains("vendor/model-b"));
        assert_eq!(ids.len(), 3);
        assert!(parse_catalog_ids("not json").is_err());
        assert!(parse_catalog_ids(r#"{"no_data":1}"#).is_err());
    }

    /// A catalog listing is not proof of serving: a provider can advertise an id
    /// its endpoint answers 404 for, and then the probe reports the agent
    /// healthy while every task it is given fails. Observed in production on a
    /// seat whose model was listed and unusable for two days.
    #[tokio::test]
    async fn a_listed_model_the_endpoint_will_not_serve_is_unavailable() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let probe = ModelAvailability::new("http://unused".to_string(), PROVIDER.to_string())
            .with_serving_probe(format!("{}/v1/chat/completions", server.uri()), "k".into());

        assert_eq!(
            probe.probe_serving("vendor/model-a").await,
            Availability::Unavailable
        );
    }

    #[tokio::test]
    async fn a_model_the_endpoint_answers_is_available() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(r#"{"choices":[]}"#))
            .mount(&server)
            .await;
        let probe = ModelAvailability::new("http://unused".to_string(), PROVIDER.to_string())
            .with_serving_probe(format!("{}/v1/chat/completions", server.uri()), "k".into());

        assert_eq!(
            probe.probe_serving("vendor/model-a").await,
            Availability::Available
        );
    }

    /// Fail-open, and this is the case that matters most: a rate limit, an auth
    /// blip or a provider 500 must never bench the fleet. Only "this model is
    /// not here" does.
    #[tokio::test]
    async fn every_other_failure_leaves_the_verdict_unknown() {
        for status in [400u16, 401, 429, 500, 503] {
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .respond_with(wiremock::ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let probe = ModelAvailability::new("http://unused".to_string(), PROVIDER.to_string())
                .with_serving_probe(format!("{}/v1/chat/completions", server.uri()), "k".into());

            assert_eq!(
                probe.probe_serving("vendor/model-a").await,
                Availability::Unknown,
                "status {status} must not bench an agent"
            );
        }
    }

    /// No probe configured is the default, and must stay a no-op.
    #[tokio::test]
    async fn without_a_serving_probe_the_verdict_is_unknown() {
        let probe = ModelAvailability::new("http://unused".to_string(), PROVIDER.to_string());
        assert_eq!(
            probe.probe_serving("vendor/model-a").await,
            Availability::Unknown
        );
    }
}
