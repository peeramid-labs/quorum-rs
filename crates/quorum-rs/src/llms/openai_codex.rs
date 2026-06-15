//! ChatGPT/Codex subscription auth and Responses transport.
//!
//! This is intentionally separate from [`OpenAICompatibleModel`]: OpenAI API
//! keys use `api.openai.com`, while ChatGPT subscription auth uses OAuth
//! tokens against the Codex backend.

use crate::agents::config::AgentConfig;
use crate::llms::{AiModel, ChatCompletionResult, RequestConfig, TimingMetadata};
use crate::telemetry::LlmError;
use async_openai::types::{
    ChatChoice, ChatCompletionMessageToolCall, ChatCompletionToolType, CompletionUsage,
    CreateChatCompletionResponse, FinishReason, FunctionCall, Role,
};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Semaphore;

const OPENAI_AUTH_BASE_URL_DEFAULT: &str = "https://auth.openai.com";
const OPENAI_CODEX_CLIENT_ID_DEFAULT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_CODEX_DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEVICE_CODE_TIMEOUT_MS: u64 = 15 * 60_000;
const DEVICE_CODE_DEFAULT_INTERVAL_MS: u64 = 5_000;
const DEVICE_CODE_MIN_INTERVAL_MS: u64 = 1_000;
const REFRESH_SKEW_MS: i64 = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenAICodexTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenAICodexAuthFile {
    pub provider: String,
    pub tokens: OpenAICodexTokens,
}

#[derive(Debug, Clone)]
pub struct OpenAICodexAuthStore {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCodePrompt {
    pub verification_url: String,
    pub user_code: String,
    pub expires_in_ms: u64,
}

#[derive(Debug, Deserialize)]
struct UserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_i64")]
    expires_in: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct CodexCliAuthJson {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    tokens: Option<CodexCliTokens>,
    #[serde(default)]
    last_refresh: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct CodexCliTokens {
    access_token: Option<String>,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

impl OpenAICodexAuthStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> anyhow::Result<PathBuf> {
        Ok(home_dir()?.join(".nsed").join("openai-codex.json"))
    }

    pub fn default() -> anyhow::Result<Self> {
        Ok(Self::new(Self::default_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> anyhow::Result<Option<OpenAICodexAuthFile>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&self.path)?;
        let auth = serde_json::from_str(&text)?;
        Ok(Some(auth))
    }

    pub fn write(&self, tokens: OpenAICodexTokens) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
            set_private_dir_permissions(parent)?;
        }
        let auth = OpenAICodexAuthFile {
            provider: "openai-codex".to_string(),
            tokens,
        };
        let text = serde_json::to_string_pretty(&auth)?;
        atomic_write_private(&self.path, text.as_bytes())?;
        Ok(())
    }

    pub fn import_from_codex_cli(&self) -> anyhow::Result<bool> {
        let codex_home = std::env::var("CODEX_HOME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| home_dir().ok().map(|home| home.join(".codex")));
        let Some(codex_home) = codex_home else {
            return Ok(false);
        };
        self.import_from_codex_cli_home(&codex_home)
    }

    pub fn import_from_codex_cli_home(&self, codex_home: &Path) -> anyhow::Result<bool> {
        let auth_path = codex_home.join("auth.json");
        if !auth_path.exists() {
            return Ok(false);
        }
        let text = std::fs::read_to_string(auth_path)?;
        let cli: CodexCliAuthJson = serde_json::from_str(&text)?;
        let access_token = cli
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.access_token.clone())
            .or(cli.access_token);
        let refresh_token = cli
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.refresh_token.clone())
            .or(cli.refresh_token);
        let account_id = cli
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.account_id.clone());
        let Some(access_token) = access_token else {
            return Ok(false);
        };
        let Some(refresh_token) = refresh_token else {
            return Ok(false);
        };
        let expires_at_ms = resolve_codex_access_token_expiry_ms(&access_token)
            .or_else(|| fallback_expiry_from_codex_last_refresh(cli.last_refresh.as_ref()))
            .unwrap_or_else(|| now_ms() + 60 * 60_000);
        if expires_at_ms <= now_ms() {
            return Ok(false);
        }
        self.write(OpenAICodexTokens {
            account_id: account_id.or_else(|| resolve_chatgpt_account_id(&access_token)),
            access_token,
            refresh_token,
            expires_at_ms,
        })?;
        Ok(true)
    }
}

#[derive(Debug, Clone)]
pub struct OpenAICodexModel {
    client: reqwest::Client,
    base_url: String,
    auth_store: OpenAICodexAuthStore,
    semaphore: Option<Arc<Semaphore>>,
}

impl OpenAICodexModel {
    pub fn new(base_url: Option<String>, auth_store: OpenAICodexAuthStore) -> Self {
        let base_url = base_url
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("QUORUM_CODEX_BASE_URL").ok())
            .unwrap_or_else(|| OPENAI_CODEX_DEFAULT_BASE_URL.to_string());
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(7200))
                .build()
                .expect("failed to create HTTP client"),
            base_url,
            auth_store,
            semaphore: None,
        }
    }

    pub fn with_semaphore(mut self, semaphore: Arc<Semaphore>) -> Self {
        self.semaphore = Some(semaphore);
        self
    }

    async fn access_token(&self, force_refresh: bool) -> Result<OpenAICodexTokens, LlmError> {
        let mut auth = self
            .auth_store
            .read()
            .map_err(anyhow_error)?
            .ok_or_else(|| {
                other_msg("OpenAI Codex auth is missing; run `quorum auth openai-codex`")
            })?;

        if force_refresh || auth.tokens.expires_at_ms - now_ms() <= REFRESH_SKEW_MS {
            let _refresh_guard = refresh_lock().lock().await;
            let stale_refresh_token = auth.tokens.refresh_token.clone();
            if let Some(latest) = self.auth_store.read().map_err(anyhow_error)?
                && !force_refresh
                && latest.tokens.expires_at_ms - now_ms() > REFRESH_SKEW_MS
            {
                return Ok(latest.tokens);
            }
            if let Some(latest) = self.auth_store.read().map_err(anyhow_error)?
                && latest.tokens.refresh_token != stale_refresh_token
            {
                auth = latest;
            }
            let refreshed =
                match refresh_openai_codex_token(&self.client, &auth.tokens.refresh_token).await {
                    Ok(tokens) => tokens,
                    Err(err) if is_refresh_token_reused_error(&err) => {
                        let latest =
                            self.auth_store
                                .read()
                                .map_err(anyhow_error)?
                                .ok_or_else(|| {
                                    other_msg("OpenAI Codex auth disappeared during refresh")
                                })?;
                        if latest.tokens.refresh_token == auth.tokens.refresh_token {
                            return Err(anyhow_error(err));
                        }
                        refresh_openai_codex_token(&self.client, &latest.tokens.refresh_token)
                            .await
                            .map_err(anyhow_error)?
                    }
                    Err(err) => return Err(anyhow_error(err)),
                };
            self.auth_store
                .write(refreshed.clone())
                .map_err(anyhow_error)?;
            auth.tokens = refreshed;
        }
        Ok(auth.tokens)
    }
}

#[async_trait]
impl AiModel for OpenAICodexModel {
    async fn chat_completion(
        &self,
        agent: &AgentConfig,
        request_config: RequestConfig,
    ) -> Result<ChatCompletionResult, LlmError> {
        let _permit = if let Some(sem) = &self.semaphore {
            Some(sem.acquire().await.map_err(other_error)?)
        } else {
            None
        };

        let model_name = if agent.model_name.trim().is_empty() {
            "gpt-5.5"
        } else {
            agent.model_name.trim()
        };
        let request_json = build_responses_request(model_name, agent, &request_config)?;
        let request_body = serde_json::to_string(&request_json).map_err(parse_error)?;
        let endpoint = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let started = std::time::Instant::now();

        let mut tokens = self.access_token(false).await?;
        let mut response =
            send_codex_responses_request(&self.client, &endpoint, &tokens, request_body.clone())
                .await?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            tokens = self.access_token(true).await?;
            response = send_codex_responses_request(
                &self.client,
                &endpoint,
                &tokens,
                request_body.clone(),
            )
            .await?;
        }

        if !response.status().is_success() {
            let status = response.status();
            let retry_after_ms = retry_after_ms_from_headers(response.headers());
            let error_body = response.text().await.unwrap_or_default();
            if status.as_u16() == 429 {
                return Err(LlmError::RateLimit {
                    retry_after_ms,
                    status: 429,
                });
            }
            if status.is_server_error() {
                return Err(LlmError::ServerError {
                    status: status.as_u16(),
                });
            }
            return Err(other_msg(format!(
                "OpenAI Codex Responses request failed with status {status}: {error_body}"
            )));
        }

        let provider_backend = response
            .headers()
            .get("x-or-backend")
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        let body = response.text().await.map_err(transport_error)?;
        let chat_response = parse_responses_body(&body, model_name)?;
        Ok(ChatCompletionResult {
            response: chat_response,
            raw_request: request_body,
            timing: TimingMetadata {
                ttft_ms: None,
                generation_ms: Some(started.elapsed().as_millis() as u64),
            },
            provider_backend,
            shrink_info: None,
        })
    }
}

async fn send_codex_responses_request(
    client: &reqwest::Client,
    endpoint: &str,
    tokens: &OpenAICodexTokens,
    request_body: String,
) -> Result<reqwest::Response, LlmError> {
    client
        .post(endpoint)
        .headers(codex_responses_headers(tokens).map_err(anyhow_error)?)
        .body(request_body)
        .send()
        .await
        .map_err(transport_error)
}

pub async fn login_openai_codex_device_code<F, Fut>(
    client: &reqwest::Client,
    on_verification: F,
) -> anyhow::Result<OpenAICodexTokens>
where
    F: FnOnce(DeviceCodePrompt) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let user_code = request_device_code(client).await?;
    let auth_base_url = openai_auth_base_url();
    on_verification(DeviceCodePrompt {
        verification_url: format!("{auth_base_url}/codex/device"),
        user_code: user_code.user_code.clone(),
        expires_in_ms: DEVICE_CODE_TIMEOUT_MS,
    })
    .await;

    let authorization = poll_device_code(client, &user_code).await?;
    exchange_authorization_code(
        client,
        &authorization.authorization_code,
        &authorization.code_verifier,
    )
    .await
}

pub async fn login_and_store_openai_codex_device_code<F, Fut>(
    store: &OpenAICodexAuthStore,
    on_verification: F,
) -> anyhow::Result<OpenAICodexTokens>
where
    F: FnOnce(DeviceCodePrompt) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let client = reqwest::Client::new();
    let tokens = login_openai_codex_device_code(&client, on_verification).await?;
    store.write(tokens.clone())?;
    Ok(tokens)
}

async fn request_device_code(client: &reqwest::Client) -> anyhow::Result<UserCodeResponse> {
    let response = client
        .post(format!(
            "{}/api/accounts/deviceauth/usercode",
            openai_auth_base_url()
        ))
        .headers(openai_codex_auth_headers("application/json")?)
        .json(&json!({ "client_id": openai_codex_client_id() }))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "OpenAI device code request failed ({status}): {}",
            sanitize_error_text(&body)
        );
    }
    Ok(serde_json::from_str(&body)?)
}

async fn poll_device_code(
    client: &reqwest::Client,
    code: &UserCodeResponse,
) -> anyhow::Result<DeviceTokenResponse> {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(DEVICE_CODE_TIMEOUT_MS);
    let interval_ms = code
        .interval
        .unwrap_or(DEVICE_CODE_DEFAULT_INTERVAL_MS / 1000)
        .saturating_mul(1000)
        .max(DEVICE_CODE_MIN_INTERVAL_MS);

    while std::time::Instant::now() < deadline {
        let response = client
            .post(format!(
                "{}/api/accounts/deviceauth/token",
                openai_auth_base_url()
            ))
            .headers(openai_codex_auth_headers("application/json")?)
            .json(&json!({
                "device_auth_id": code.device_auth_id,
                "user_code": code.user_code,
            }))
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return Ok(serde_json::from_str(&body)?);
        }
        if status.as_u16() == 403 {
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
            continue;
        }
        anyhow::bail!(
            "OpenAI device authorization failed ({status}): {}",
            sanitize_error_text(&body)
        );
    }
    anyhow::bail!("OpenAI device authorization timed out after 15 minutes")
}

async fn exchange_authorization_code(
    client: &reqwest::Client,
    authorization_code: &str,
    code_verifier: &str,
) -> anyhow::Result<OpenAICodexTokens> {
    let redirect_uri = openai_codex_device_callback_url();
    let client_id = openai_codex_client_id();
    let response = client
        .post(format!("{}/oauth/token", openai_auth_base_url()))
        .headers(openai_codex_auth_headers(
            "application/x-www-form-urlencoded",
        )?)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", authorization_code),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await?;
    parse_oauth_token_response(response, "exchange", None).await
}

pub async fn refresh_openai_codex_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> anyhow::Result<OpenAICodexTokens> {
    let client_id = openai_codex_client_id();
    let response = client
        .post(format!("{}/oauth/token", openai_auth_base_url()))
        .headers(openai_codex_auth_headers(
            "application/x-www-form-urlencoded",
        )?)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id.as_str()),
        ])
        .send()
        .await?;
    parse_oauth_token_response(response, "refresh", Some(refresh_token)).await
}

async fn parse_oauth_token_response(
    response: reqwest::Response,
    operation: &str,
    previous_refresh_token: Option<&str>,
) -> anyhow::Result<OpenAICodexTokens> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "OpenAI Codex token {operation} failed ({status}): {}",
            sanitize_error_text(&body)
        );
    }
    let token_response: OAuthTokenResponse = serde_json::from_str(&body)?;
    let expires_at_ms = token_response
        .expires_in
        .map(|seconds| now_ms() + seconds.saturating_mul(1000))
        .or_else(|| resolve_codex_access_token_expiry_ms(&token_response.access_token))
        .unwrap_or_else(|| now_ms() + 5 * 60_000);
    Ok(OpenAICodexTokens {
        account_id: resolve_chatgpt_account_id(&token_response.access_token),
        access_token: token_response.access_token,
        refresh_token: token_response
            .refresh_token
            .or_else(|| previous_refresh_token.map(ToOwned::to_owned))
            .ok_or_else(|| {
                anyhow::anyhow!("OpenAI Codex token {operation} response missing refresh_token")
            })?,
        expires_at_ms,
    })
}

fn openai_auth_base_url() -> String {
    std::env::var("QUORUM_OPENAI_AUTH_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| OPENAI_AUTH_BASE_URL_DEFAULT.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn openai_codex_client_id() -> String {
    std::env::var("QUORUM_OPENAI_CODEX_CLIENT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| OPENAI_CODEX_CLIENT_ID_DEFAULT.to_string())
}

fn openai_codex_device_callback_url() -> String {
    std::env::var("QUORUM_OPENAI_CODEX_DEVICE_CALLBACK_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("{}/deviceauth/callback", openai_auth_base_url()))
}

fn openai_codex_auth_headers(content_type: &str) -> anyhow::Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::CONTENT_TYPE, content_type.parse()?);
    Ok(headers)
}

fn codex_responses_headers(
    tokens: &OpenAICodexTokens,
) -> anyhow::Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::CONTENT_TYPE, "application/json".parse()?);
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {}", tokens.access_token).parse()?,
    );
    headers.insert(
        reqwest::header::USER_AGENT,
        "codex_cli_rs/0.0.0 (quorum-rs)".parse()?,
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("originator"),
        "codex_cli_rs".parse()?,
    );
    if let Some(account_id) = tokens
        .account_id
        .clone()
        .or_else(|| resolve_chatgpt_account_id(&tokens.access_token))
    {
        headers.insert(
            reqwest::header::HeaderName::from_static("chatgpt-account-id"),
            account_id.parse()?,
        );
    }
    Ok(headers)
}

fn retry_after_ms_from_headers(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .and_then(|seconds| seconds.checked_mul(1000))
}

fn refresh_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn is_refresh_token_reused_error(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .to_ascii_lowercase()
        .contains("refresh_token_reused")
}

fn fallback_expiry_from_codex_last_refresh(value: Option<&Value>) -> Option<i64> {
    let millis = match value {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.timestamp_millis()),
        _ => None,
    }?;
    Some(millis.saturating_add(60 * 60_000))
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_integer(deserializer, "u64").and_then(|value| {
        value
            .map(|number| {
                u64::try_from(number).map_err(|_| {
                    serde::de::Error::custom(format!("expected u64-compatible value, got {number}"))
                })
            })
            .transpose()
    })
}

fn deserialize_optional_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_integer(deserializer, "i64")
}

fn deserialize_optional_integer<'de, D>(
    deserializer: D,
    expected: &'static str,
) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("expected {expected}"))),
        Some(Value::String(raw)) => raw.trim().parse::<i64>().map(Some).map_err(|_| {
            serde::de::Error::custom(format!("expected {expected}-compatible string"))
        }),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected {expected}, got {other}"
        ))),
    }
}

fn build_responses_request(
    model_name: &str,
    agent: &AgentConfig,
    request: &RequestConfig,
) -> Result<Value, LlmError> {
    let (instructions, input) = responses_input_from_messages(&request.messages)?;
    let mut body = Map::new();
    body.insert("model".to_string(), json!(model_name));
    body.insert("input".to_string(), Value::Array(input));
    body.insert("stream".to_string(), Value::Bool(true));
    body.insert(
        "instructions".to_string(),
        Value::String(if instructions.trim().is_empty() {
            "You are a helpful coding assistant.".to_string()
        } else {
            instructions
        }),
    );
    body.insert("store".to_string(), Value::Bool(false));
    if let Some(tools) = &request.tools {
        let converted = convert_tools(tools)?;
        if !converted.is_empty() {
            body.insert("tools".to_string(), Value::Array(converted));
        }
    }
    if let Some(choice) = &request.tool_choice {
        body.insert(
            "tool_choice".to_string(),
            serde_json::to_value(choice).map_err(parse_error)?,
        );
    }
    if let Some(effort) = agent
        .reasoning_effort
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        body.insert("reasoning".to_string(), json!({ "effort": effort }));
    }
    Ok(Value::Object(body))
}

fn responses_input_from_messages(
    messages: &[async_openai::types::ChatCompletionRequestMessage],
) -> Result<(String, Vec<Value>), LlmError> {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        let value = serde_json::to_value(message).map_err(parse_error)?;
        let role = value.get("role").and_then(Value::as_str).unwrap_or("user");
        let content_text = message_content_text(value.get("content"));
        match role {
            "system" | "developer" => {
                if !content_text.is_empty() {
                    instructions.push(content_text);
                }
            }
            "tool" => {
                let call_id = value
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": content_text,
                }));
            }
            "assistant" => {
                if !content_text.is_empty() {
                    input.push(json!({
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": content_text }],
                    }));
                }
                if let Some(calls) = value.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        if let Some(function) = call.get("function") {
                            input.push(json!({
                                "type": "function_call",
                                "call_id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
                                "name": function.get("name").and_then(Value::as_str).unwrap_or_default(),
                                "arguments": function.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
                            }));
                        }
                    }
                }
            }
            _ => {
                input.push(json!({
                    "role": "user",
                    "content": [{ "type": "input_text", "text": content_text }],
                }));
            }
        }
    }
    if input.is_empty() {
        input.push(json!({
            "role": "user",
            "content": [{ "type": "input_text", "text": "" }],
        }));
    }
    Ok((instructions.join("\n\n"), input))
}

fn message_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

fn convert_tools(
    tools: &[async_openai::types::ChatCompletionTool],
) -> Result<Vec<Value>, LlmError> {
    tools
        .iter()
        .map(|tool| {
            let value = serde_json::to_value(tool).map_err(parse_error)?;
            let function = value.get("function").cloned().unwrap_or_else(|| json!({}));
            let mut out = Map::new();
            out.insert("type".to_string(), json!("function"));
            if let Some(name) = function.get("name") {
                out.insert("name".to_string(), name.clone());
            }
            if let Some(description) = function.get("description") {
                out.insert("description".to_string(), description.clone());
            }
            if let Some(parameters) = function.get("parameters") {
                out.insert("parameters".to_string(), parameters.clone());
            }
            if let Some(strict) = function.get("strict") {
                out.insert("strict".to_string(), strict.clone());
            }
            Ok(Value::Object(out))
        })
        .collect()
}

fn parse_responses_response(
    body: &str,
    model_name: &str,
) -> Result<CreateChatCompletionResponse, LlmError> {
    let value: Value = serde_json::from_str(body).map_err(parse_error)?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("codex-response")
        .to_string();
    let mut content = String::new();
    let mut tool_calls = Vec::new();

    if let Some(output) = value.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if matches!(
                                part.get("type").and_then(Value::as_str),
                                Some("output_text" | "text")
                            ) {
                                if let Some(text) = part.get("text").and_then(Value::as_str) {
                                    content.push_str(text);
                                }
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let call_id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("call_codex")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}")
                        .to_string();
                    tool_calls.push(ChatCompletionMessageToolCall {
                        id: call_id,
                        r#type: ChatCompletionToolType::Function,
                        function: FunctionCall { name, arguments },
                    });
                }
                _ => {}
            }
        }
    } else if let Some(text) = value.get("output_text").and_then(Value::as_str) {
        content.push_str(text);
    }

    #[allow(deprecated)]
    let message = async_openai::types::ChatCompletionResponseMessage {
        role: Role::Assistant,
        content: if content.is_empty() {
            None
        } else {
            Some(content)
        },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        function_call: None,
        refusal: None,
        audio: None,
    };

    Ok(CreateChatCompletionResponse {
        id,
        object: "chat.completion".to_string(),
        created: (now_ms() / 1000) as u32,
        model: model_name.to_string(),
        choices: vec![ChatChoice {
            index: 0,
            message,
            finish_reason: Some(FinishReason::Stop),
            logprobs: None,
        }],
        usage: parse_usage(value.get("usage")),
        service_tier: None,
        system_fingerprint: None,
    })
}

fn parse_responses_body(
    body: &str,
    model_name: &str,
) -> Result<CreateChatCompletionResponse, LlmError> {
    let trimmed = body.trim_start();
    if !trimmed.starts_with("event:") && !trimmed.starts_with("data:") {
        return parse_responses_response(body, model_name);
    }

    let mut text_deltas = String::new();
    let mut output_items = Vec::new();
    let mut response_id = "resp_stream".to_string();
    let mut usage = None;

    for event in body.split("\n\n") {
        let mut event_name = "";
        let mut data_lines = Vec::new();
        for line in event.lines() {
            if let Some(value) = line.strip_prefix("event:") {
                event_name = value.trim();
            } else if let Some(value) = line.strip_prefix("data:") {
                let value = value.trim();
                if value != "[DONE]" {
                    data_lines.push(value);
                }
            }
        }
        if data_lines.is_empty() {
            continue;
        }

        let data = data_lines.join("\n");
        let value: Value = serde_json::from_str(&data).map_err(parse_error)?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event_name);
        match event_type {
            "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    output_items.push(item.clone());
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    text_deltas.push_str(delta);
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                if let Some(response) = value.get("response") {
                    if let Some(id) = response.get("id").and_then(Value::as_str) {
                        response_id = id.to_string();
                    }
                    usage = response.get("usage").cloned();
                }
            }
            _ => {}
        }
    }

    if output_items.is_empty() && !text_deltas.is_empty() {
        output_items.push(json!({
            "type": "message",
            "content": [{ "type": "output_text", "text": text_deltas }]
        }));
    }

    if !output_items.is_empty() {
        let mut response = Map::new();
        response.insert("id".to_string(), Value::String(response_id));
        response.insert("output".to_string(), Value::Array(output_items));
        if let Some(usage) = usage {
            response.insert("usage".to_string(), usage);
        }
        return parse_responses_response(&Value::Object(response).to_string(), model_name);
    }

    Err(other_msg(
        "OpenAI Codex stream did not include a completed response",
    ))
}

fn parse_usage(usage: Option<&Value>) -> Option<CompletionUsage> {
    let usage = usage?;
    let input = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let output = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    Some(CompletionUsage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input + output,
        prompt_tokens_details: None,
        completion_tokens_details: None,
    })
}

pub fn resolve_chatgpt_account_id(access_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(access_token)?;
    payload
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn resolve_codex_access_token_expiry_ms(access_token: &str) -> Option<i64> {
    let payload = decode_jwt_payload(access_token)?;
    payload
        .get("exp")
        .and_then(Value::as_i64)
        .filter(|exp| *exp > 0)
        .map(|exp| exp.saturating_mul(1000))
}

fn decode_jwt_payload(access_token: &str) -> Option<Value> {
    let mut parts = access_token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _sig = parts.next()?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn sanitize_error_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn atomic_write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    set_private_file_permissions(tmp.path())?;
    {
        use std::io::Write as _;
        tmp.write_all(data)?;
        tmp.flush()?;
    }
    tmp.persist(path)?;
    set_private_file_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn parse_error<E>(e: E) -> LlmError
where
    E: std::error::Error + Send + Sync + 'static,
{
    LlmError::Parse(Box::new(e))
}

fn transport_error<E>(e: E) -> LlmError
where
    E: std::error::Error + Send + Sync + 'static,
{
    LlmError::Transport(Box::new(e))
}

fn other_error<E>(e: E) -> LlmError
where
    E: std::error::Error + Send + Sync + 'static,
{
    LlmError::Other(Box::new(e))
}

fn anyhow_error(e: anyhow::Error) -> LlmError {
    other_msg(e.to_string())
}

fn other_msg(message: impl Into<String>) -> LlmError {
    LlmError::Other(Box::new(std::io::Error::other(message.into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent, ChatCompletionTool, ChatCompletionToolType,
        FunctionObject,
    };
    use std::ffi::OsString;

    fn jwt(payload: Value) -> String {
        format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let original = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, original }
        }

        fn unset(key: &'static str) -> Self {
            let original = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    struct CurrentDirGuard(PathBuf);

    impl CurrentDirGuard {
        fn change_to(path: &Path) -> Self {
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self(original)
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }

    #[test]
    fn extracts_account_id_from_codex_jwt() {
        let token = jwt(json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_123"
            }
        }));
        assert_eq!(
            resolve_chatgpt_account_id(&token).as_deref(),
            Some("acct_123")
        );
    }

    #[test]
    fn codex_headers_include_first_party_originator_and_account() {
        let token = jwt(json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_openai_workspace"
            }
        }));
        let tokens = OpenAICodexTokens {
            access_token: token,
            refresh_token: "refresh".into(),
            expires_at_ms: now_ms() + 60_000,
            account_id: None,
        };
        let headers = codex_responses_headers(&tokens).unwrap();
        assert_eq!(headers.get("originator").unwrap(), "codex_cli_rs");
        assert_eq!(
            headers.get("ChatGPT-Account-ID").unwrap(),
            "acct_openai_workspace"
        );
        assert_eq!(
            headers.get("User-Agent").unwrap(),
            "codex_cli_rs/0.0.0 (quorum-rs)"
        );
    }

    #[test]
    fn auth_headers_match_codex_device_auth_flow() {
        let headers = openai_codex_auth_headers("application/json").unwrap();
        assert_eq!(headers.get("Content-Type").unwrap(), "application/json");
        assert!(headers.get("originator").is_none());
        assert!(headers.get("User-Agent").is_none());
    }

    #[test]
    fn codex_last_refresh_fallback_expires_one_hour_later() {
        let ts = "2026-06-15T12:00:00Z";
        let expiry = fallback_expiry_from_codex_last_refresh(Some(&json!(ts))).unwrap();
        assert_eq!(
            expiry,
            chrono::DateTime::parse_from_rfc3339(ts)
                .unwrap()
                .timestamp_millis()
                + 60 * 60_000
        );
    }

    #[test]
    fn parses_device_code_numeric_strings() {
        let response: UserCodeResponse = serde_json::from_value(json!({
            "device_auth_id": "dev_123",
            "user_code": "ABCD-EFGH",
            "interval": "5"
        }))
        .unwrap();
        assert_eq!(response.interval, Some(5));
    }

    #[test]
    fn parses_oauth_expires_in_numeric_string() {
        let response: OAuthTokenResponse = serde_json::from_value(json!({
            "access_token": "access",
            "refresh_token": "refresh",
            "expires_in": "3600"
        }))
        .unwrap();
        assert_eq!(response.expires_in, Some(3600));
    }

    #[test]
    fn oauth_token_response_allows_missing_refresh_token() {
        let response: OAuthTokenResponse = serde_json::from_value(json!({
            "access_token": "access",
            "expires_in": 3600
        }))
        .unwrap();
        assert!(response.refresh_token.is_none());
    }

    #[test]
    #[serial_test::serial(openai_auth_env)]
    fn auth_env_overrides_are_trimmed_and_optional() {
        let _auth_base = EnvVarGuard::set(
            "QUORUM_OPENAI_AUTH_BASE_URL",
            "https://auth.example.test///",
        );
        let _client_id = EnvVarGuard::set("QUORUM_OPENAI_CODEX_CLIENT_ID", "client-test");
        let _callback = EnvVarGuard::unset("QUORUM_OPENAI_CODEX_DEVICE_CALLBACK_URL");

        assert_eq!(openai_auth_base_url(), "https://auth.example.test");
        assert_eq!(openai_codex_client_id(), "client-test");
        assert_eq!(
            openai_codex_device_callback_url(),
            "https://auth.example.test/deviceauth/callback"
        );
    }

    #[tokio::test]
    #[serial_test::serial(openai_auth_env)]
    async fn refresh_without_refresh_token_preserves_previous_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let _auth_base = EnvVarGuard::set("QUORUM_OPENAI_AUTH_BASE_URL", server.uri());
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "new-access",
                "expires_in": "3600"
            })))
            .mount(&server)
            .await;

        let tokens = refresh_openai_codex_token(&reqwest::Client::new(), "old-refresh")
            .await
            .unwrap();
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(tokens.refresh_token, "old-refresh");
    }

    #[tokio::test]
    #[serial_test::serial(openai_auth_env)]
    async fn authorization_exchange_requires_refresh_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let _auth_base = EnvVarGuard::set("QUORUM_OPENAI_AUTH_BASE_URL", server.uri());
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "new-access",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let err = exchange_authorization_code(&reqwest::Client::new(), "code", "verifier")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("missing refresh_token"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(openai_auth_env)]
    async fn poll_device_code_fails_fast_on_404() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let _auth_base = EnvVarGuard::set("QUORUM_OPENAI_AUTH_BASE_URL", server.uri());
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/token"))
            .respond_with(ResponseTemplate::new(404).set_body_string("unknown\n device\r session"))
            .mount(&server)
            .await;

        let err = poll_device_code(
            &reqwest::Client::new(),
            &UserCodeResponse {
                device_auth_id: "device-id".to_string(),
                user_code: "USER-CODE".to_string(),
                interval: Some(0),
            },
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("404"), "unexpected error: {message}");
        assert!(
            message.contains("unknown device session"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn builds_responses_request_with_instructions_and_tools() {
        let mut agent = AgentConfig::default();
        agent.model_name = "gpt-5.5".to_string();
        agent.max_tokens = 1024;
        agent.reasoning_effort = Some("high".to_string());

        let tool = ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: "read_file".to_string(),
                description: Some("Read a file".to_string()),
                parameters: Some(json!({"type": "object"})),
                strict: None,
            },
        };
        let request = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                    content: ChatCompletionRequestSystemMessageContent::Text("sys".into()),
                    name: None,
                }),
                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("hello".into()),
                    name: None,
                }),
            ],
            tools: Some(vec![tool]),
            tool_choice: None,
            presence_penalty: None,
        };
        let body = build_responses_request("gpt-5.5", &agent, &request).unwrap();
        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn builds_responses_request_with_default_instructions() {
        let agent = AgentConfig::default();
        let request = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("hello".into()),
                    name: None,
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
        };
        let body = build_responses_request("gpt-5.5", &agent, &request).unwrap();
        assert_eq!(body["instructions"], "You are a helpful coding assistant.");
    }

    #[test]
    fn parses_responses_text_and_tool_calls() {
        let body = json!({
            "id": "resp_1",
            "output": [
                {
                    "type": "message",
                    "content": [{ "type": "output_text", "text": "hi" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read_file",
                    "arguments": "{\"path\":\"README.md\"}"
                }
            ],
            "usage": { "input_tokens": 10, "output_tokens": 2 }
        })
        .to_string();
        let parsed = parse_responses_response(&body, "gpt-5.5").unwrap();
        let msg = &parsed.choices[0].message;
        assert_eq!(msg.content.as_deref(), Some("hi"));
        let call = &msg.tool_calls.as_ref().unwrap()[0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.function.name, "read_file");
        assert_eq!(parsed.usage.unwrap().prompt_tokens, 10);
    }

    #[test]
    fn parses_responses_sse_completed_event() {
        let done = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "content": [{ "type": "output_text", "text": "stream ok" }]
            }
        });
        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_2",
                "output": null
            }
        });
        let body = format!(
            "event: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            done, completed
        );

        let parsed = parse_responses_body(&body, "gpt-5.5").unwrap();
        assert_eq!(
            parsed.choices[0].message.content.as_deref(),
            Some("stream ok")
        );
    }

    #[test]
    fn auth_store_writes_private_json() {
        let dir = tempfile::tempdir().unwrap();
        let store = OpenAICodexAuthStore::new(dir.path().join("auth.json"));
        store
            .write(OpenAICodexTokens {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at_ms: 123,
                account_id: Some("acct".into()),
            })
            .unwrap();
        let read = store.read().unwrap().unwrap();
        assert_eq!(read.provider, "openai-codex");
        assert_eq!(read.tokens.account_id.as_deref(), Some("acct"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn imports_nested_codex_cli_auth_shape() {
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        let token = jwt(json!({
            "exp": (now_ms() / 1000) + 3600,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_nested"
            }
        }));
        std::fs::write(
            codex_home.join("auth.json"),
            serde_json::to_string(&json!({
                "tokens": {
                    "access_token": token,
                    "refresh_token": "refresh-nested",
                    "account_id": "acct_from_file"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let store = OpenAICodexAuthStore::new(dir.path().join("quorum-auth.json"));
        let imported = store.import_from_codex_cli_home(&codex_home).unwrap();
        assert!(imported);
        let auth = store.read().unwrap().unwrap();
        assert_eq!(auth.tokens.refresh_token, "refresh-nested");
        assert_eq!(auth.tokens.account_id.as_deref(), Some("acct_from_file"));
    }

    #[test]
    #[serial_test::serial(openai_auth_env)]
    fn import_from_codex_cli_does_not_fall_back_to_cwd_without_home() {
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join(".codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let token = jwt(json!({
            "exp": (now_ms() / 1000) + 3600,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_cwd"
            }
        }));
        std::fs::write(
            codex_home.join("auth.json"),
            serde_json::to_string(&json!({
                "tokens": {
                    "access_token": token,
                    "refresh_token": "refresh-cwd",
                    "account_id": "acct_cwd"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let _cwd_guard = CurrentDirGuard::change_to(dir.path());
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let _home = EnvVarGuard::unset("HOME");

        let store = OpenAICodexAuthStore::new(dir.path().join("quorum-auth.json"));
        assert!(!store.import_from_codex_cli().unwrap());
        assert!(store.read().unwrap().is_none());
    }
}
