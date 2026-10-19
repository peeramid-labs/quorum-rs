//! Client for the key manager (noolog-node `kms/`), and the wire both sides
//! speak. A service signs every request with its own nkey over a one-time
//! nonce, the method, the path and the body; the KMS answers secrets derived
//! from its root, its account's public key, and user JWTs it signs.
//!
//! The KMS listens on the internal network only; this client never reaches it
//! through a public URL.

use anyhow::{Context as _, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Header carrying the caller's nkey public key.
pub const KEY: &str = "x-kms-key";
/// Header carrying `<unix millis>.<random>`, used once.
pub const NONCE: &str = "x-kms-nonce";
/// Header carrying the hex signature over [`canonical`].
pub const SIGNATURE: &str = "x-kms-signature";

/// Binding the method and path stops a signed `/derive` being replayed
/// against `/user-jwt`; the body hash stops a label being swapped.
pub fn canonical(nonce: &str, method: &str, path: &str, body: &[u8]) -> String {
    format!(
        "{nonce}:{method}:{path}:{}",
        hex::encode(Sha256::digest(body))
    )
}

/// What a user JWT grants, as `POST /user-jwt` takes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserJwt {
    pub user_public_key: String,
    /// Shown in the broker's logs, e.g. `agent-1@acme`.
    pub name: String,
    #[serde(default)]
    pub publish: Vec<String>,
    #[serde(default)]
    pub subscribe: Vec<String>,
}

/// A service's client: every call signed with the service's nkey.
#[derive(Clone)]
pub struct KmsClient {
    http: reqwest::Client,
    base: String,
    key: std::sync::Arc<nkeys::KeyPair>,
}

impl KmsClient {
    /// `base` is the KMS's internal address; `seed` the service's nkey seed.
    pub fn new(base: &str, seed: &str) -> Result<Self> {
        let key = nkeys::KeyPair::from_seed(seed.trim())
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("the service's KMS nkey seed")?;
        Ok(Self {
            http: reqwest::Client::new(),
            base: base.trim_end_matches('/').to_string(),
            key: std::sync::Arc::new(key),
        })
    }

    /// `hex(HMAC-SHA256(root, label))`; refused unless `label` is under this
    /// service's prefixes in the KMS's clients file.
    pub async fn derive(&self, label: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Derived {
            secret: String,
        }
        let body = serde_json::to_vec(&serde_json::json!({ "label": label }))?;
        let derived: Derived = self.call(reqwest::Method::POST, "/derive", body).await?;
        Ok(derived.secret)
    }

    /// Agents sign their registration challenge against this key, so an issuer
    /// asks it once at startup rather than per registration.
    pub async fn account_public_key(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct Account {
            public_key: String,
        }
        let account: Account = self
            .call(reqwest::Method::GET, "/account", Vec::new())
            .await?;
        Ok(account.public_key)
    }

    /// A user JWT signed with the account key, which never leaves the KMS;
    /// refused unless this service may have user JWTs signed.
    pub async fn user_jwt(&self, asked: &UserJwt) -> Result<String> {
        #[derive(Deserialize)]
        struct Signed {
            jwt: String,
        }
        let signed: Signed = self
            .call(
                reqwest::Method::POST,
                "/user-jwt",
                serde_json::to_vec(asked)?,
            )
            .await?;
        Ok(signed.jwt)
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Vec<u8>,
    ) -> Result<T> {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let nonce = format!("{millis}.{}", uuid::Uuid::new_v4().simple());
        let signature = self
            .key
            .sign(canonical(&nonce, method.as_str(), path, &body).as_bytes())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let response = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header(KEY, self.key.public_key())
            .header(NONCE, nonce)
            .header(SIGNATURE, hex::encode(signature))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .with_context(|| format!("ask the KMS {path}"))?;
        let status = response.status();
        anyhow::ensure!(
            status.is_success(),
            "the KMS refused {path}: {status} {}",
            response.text().await.unwrap_or_default()
        );
        response.json().await.context("read the KMS's answer")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};

    /// Answers what was signed, after checking the signature as the KMS does.
    async fn echo(client_key: String) -> String {
        let route = move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let client_key = client_key.clone();
            async move {
                let header = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).unwrap_or("");
                let signed = canonical(header(NONCE), method.as_str(), uri.path(), &body);
                let verified = header(KEY) == client_key
                    && hex::decode(header(SIGNATURE)).is_ok_and(|sig| {
                        nkeys::KeyPair::from_public_key(&client_key)
                            .is_ok_and(|k| k.verify(signed.as_bytes(), &sig).is_ok())
                    });
                if !verified {
                    return (StatusCode::UNAUTHORIZED, "{}".to_string());
                }
                let answer = match uri.path() {
                    "/derive" => serde_json::json!({ "secret": String::from_utf8_lossy(&body) }),
                    "/account" => serde_json::json!({ "public_key": "AXYZ" }),
                    _ => {
                        let asked: UserJwt = serde_json::from_slice(&body).unwrap();
                        serde_json::json!({ "jwt": asked.name })
                    }
                };
                (StatusCode::OK, answer.to_string())
            }
        };
        let app = axum::Router::new()
            .route("/derive", axum::routing::post(route.clone()))
            .route("/account", axum::routing::get(route.clone()))
            .route("/user-jwt", axum::routing::post(route));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    #[test]
    fn the_canonical_binds_nonce_method_path_and_body() {
        let a = canonical("1.x", "POST", "/derive", b"{}");
        assert_eq!(
            a,
            "1.x:POST:/derive:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        assert_ne!(a, canonical("1.x", "GET", "/derive", b"{}"));
        assert_ne!(a, canonical("1.x", "POST", "/account", b"{}"));
        assert_ne!(a, canonical("1.x", "POST", "/derive", b"{ }"));
        assert_ne!(a, canonical("2.x", "POST", "/derive", b"{}"));
    }

    #[tokio::test]
    async fn every_call_is_signed_and_answered() {
        let key = nkeys::KeyPair::new_user();
        let base = echo(key.public_key()).await;
        let kms = KmsClient::new(&format!("{base}/"), &key.seed().unwrap()).unwrap();

        assert_eq!(kms.derive("node/x").await.unwrap(), r#"{"label":"node/x"}"#);
        assert_eq!(kms.account_public_key().await.unwrap(), "AXYZ");
        let asked = UserJwt {
            user_public_key: "UABC".into(),
            name: "agent-1@acme".into(),
            publish: vec![],
            subscribe: vec![],
        };
        assert_eq!(kms.user_jwt(&asked).await.unwrap(), "agent-1@acme");
    }

    #[tokio::test]
    async fn a_refusal_names_the_status() {
        let base = echo(nkeys::KeyPair::new_user().public_key()).await;
        let stranger = nkeys::KeyPair::new_user();
        let kms = KmsClient::new(&base, &stranger.seed().unwrap()).unwrap();
        let refused = kms.derive("node/x").await.unwrap_err().to_string();
        assert!(refused.contains("401"), "{refused}");
        assert!(KmsClient::new(&base, "not a seed").is_err());
    }
}
