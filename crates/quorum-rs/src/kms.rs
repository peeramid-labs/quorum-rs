//! Client for the key manager (mist-os `kms/`), and the wire both sides
//! speak. A service's one credential is its NATS `.creds`: a user JWT the
//! account signed, whose tags carry what the KMS lets it do, and the user nkey
//! seed. Every request carries the JWT and is signed with that seed over a
//! one-time nonce, the method, the path and the body; the KMS checks the JWT
//! against its own account key, so it keeps no list of clients.
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
/// Header for the caller's account-signed JWT, whose tags are its grants.
pub const JWT: &str = "x-kms-jwt";

/// JWT tag letting its holder derive labels starting with what follows.
pub const TAG_DERIVE: &str = "kms-derive:";
/// JWT tag letting its holder have user JWTs signed.
pub const TAG_SIGN_USERS: &str = "kms-sign-users";

/// The user JWT and nkey seed of a `.creds` file.
pub fn parse_creds(creds: &str) -> Result<(String, String)> {
    let block = |begin: &str| {
        creds
            .lines()
            .skip_while(|l| !l.contains(begin))
            .skip(1)
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("---"))
            .map(str::to_string)
    };
    let jwt = block("BEGIN NATS USER JWT").context("no user JWT in the creds")?;
    let seed = block("BEGIN USER NKEY SEED").context("no nkey seed in the creds")?;
    Ok((jwt, seed))
}

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

/// A service's client: every call carries its JWT and is signed with its seed.
#[derive(Clone)]
pub struct KmsClient {
    http: reqwest::Client,
    base: String,
    jwt: std::sync::Arc<String>,
    key: std::sync::Arc<nkeys::KeyPair>,
}

impl KmsClient {
    /// `base` is the KMS's internal address; `creds` the text of the service's
    /// NATS `.creds` file.
    pub fn from_creds(base: &str, creds: &str) -> Result<Self> {
        let (jwt, seed) = parse_creds(creds)?;
        let key = nkeys::KeyPair::from_seed(&seed)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("the nkey seed in the creds")?;
        Ok(Self {
            http: reqwest::Client::new(),
            base: base.trim_end_matches('/').to_string(),
            jwt: std::sync::Arc::new(jwt),
            key: std::sync::Arc::new(key),
        })
    }

    /// [`Self::from_creds`], reading the `.creds` file at `path`.
    pub fn from_creds_file(base: &str, path: &str) -> Result<Self> {
        let creds = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
        Self::from_creds(base, &creds)
    }

    /// `hex(HMAC-SHA256(root, label))`; refused unless the JWT carries a
    /// [`TAG_DERIVE`] tag `label` starts with.
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
            .header(JWT, self.jwt.as_str())
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
                    && header(JWT) == "the.user.jwt"
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
        let kms = KmsClient::from_creds(&format!("{base}/"), &creds_of(&key)).unwrap();

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
        let kms = KmsClient::from_creds(&base, &creds_of(&stranger)).unwrap();
        let refused = kms.derive("node/x").await.unwrap_err().to_string();
        assert!(refused.contains("401"), "{refused}");
        assert!(KmsClient::from_creds(&base, "not creds").is_err());
    }

    /// A `.creds` file for `key`, its JWT a stand-in the echo server expects.
    fn creds_of(key: &nkeys::KeyPair) -> String {
        format!(
            "-----BEGIN NATS USER JWT-----\nthe.user.jwt\n------END NATS USER JWT------\n\n\
             ************************* IMPORTANT *************************\n\n\
             -----BEGIN USER NKEY SEED-----\n{}\n------END USER NKEY SEED------\n",
            key.seed().unwrap()
        )
    }

    #[test]
    fn creds_give_their_jwt_and_seed() {
        let key = nkeys::KeyPair::new_user();
        let (jwt, seed) = parse_creds(&creds_of(&key)).unwrap();
        assert_eq!((jwt.as_str(), seed), ("the.user.jwt", key.seed().unwrap()));
        assert!(parse_creds("-----BEGIN NATS USER JWT-----\nx\n").is_err());
        assert!(parse_creds("").is_err());
    }

    #[test]
    fn a_creds_file_is_read_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.creds");
        std::fs::write(&path, creds_of(&nkeys::KeyPair::new_user())).unwrap();
        assert!(KmsClient::from_creds_file("http://kms", path.to_str().unwrap()).is_ok());
        assert!(KmsClient::from_creds_file("http://kms", "/nonexistent").is_err());
    }
}
