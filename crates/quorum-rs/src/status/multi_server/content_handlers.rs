//! The operator's files, from the agent dashboard.
//!
//! An upload is staged in the files pipeline for the agent's operator
//! ([`crate::pipeline::stage`]): the agent's NATS credentials allow announcing
//! uploads only for that operator, and the node admits, processes and stores
//! them. Everything else (status, read-back, usage, delete) is the node's
//! answer, asked with the operator's bearer: the files live in the node, so
//! this surface keeps no copy and no bookkeeping of its own.

use crate::files::blob::{ObjectMeta, Visibility, stamped_now};
use crate::files::upload::{
    Refusal as UploadRefusal, declared_too_large, read_upload_form, sniff_mime,
};
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tokio::io::AsyncReadExt as _;

use super::MultiAppState;

/// The largest upload accepted when nothing says otherwise.
const DEFAULT_MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;

/// Bytes read from an upload's start to decide its media type.
const SNIFF_BYTES: u64 = 512;

/// Headers of the node's answer worth passing on to the dashboard.
const PASSED_ON: [header::HeaderName; 7] = [
    header::CONTENT_TYPE,
    header::CONTENT_LENGTH,
    header::CONTENT_RANGE,
    header::ACCEPT_RANGES,
    header::ETAG,
    header::CACHE_CONTROL,
    header::CONTENT_DISPOSITION,
];

/// Everything the file routes need, or `None` where the deployment configured
/// no files for this agent.
#[derive(Clone)]
pub struct ContentUploads {
    js: async_nats::jetstream::Context,
    /// The operator principal uploads are staged for: the one the agent's NATS
    /// credentials let it announce for.
    owner: String,
    node: NodeFiles,
    max_upload_bytes: u64,
    /// Where an upload is spooled while it is hashed. `None` is the platform
    /// temporary directory, routinely a small tmpfs in a container.
    spool_dir: Option<std::path::PathBuf>,
    uploaded_by: String,
}

/// The node's owner routes, asked as the operator.
#[derive(Clone)]
struct NodeFiles {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl ContentUploads {
    /// Read the configuration from this process's environment.
    ///
    /// `None` (the routes answer 503) unless the deployment names all of
    /// `NSED_FILES_OWNER` (the operator principal), `NSED_FILES_NODE` (the
    /// node's URL) and `NSED_FILES_TOKEN` (a bearer the node accepts for that
    /// operator). The agent is handed these rather than deriving them: the
    /// deployment provisioned the operator and its credentials.
    pub async fn from_env(
        js: &async_nats::jetstream::Context,
        uploaded_by: String,
    ) -> Option<Self> {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let (owner, base, token) = match (
            var("NSED_FILES_OWNER"),
            var("NSED_FILES_NODE"),
            var("NSED_FILES_TOKEN"),
        ) {
            (Some(owner), Some(base), Some(token)) => (owner, base, token),
            (None, None, None) => return None,
            _ => {
                tracing::warn!(
                    "files need NSED_FILES_OWNER, NSED_FILES_NODE and NSED_FILES_TOKEN together; \
                     uploads stay disabled"
                );
                return None;
            }
        };
        Some(Self {
            js: js.clone(),
            owner,
            node: NodeFiles {
                http: reqwest::Client::new(),
                base: base.trim_end_matches('/').to_string(),
                token,
            },
            max_upload_bytes: var("NSED_MAX_UPLOAD_BYTES")
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_MAX_UPLOAD_BYTES),
            spool_dir: var("NSED_UPLOAD_SPOOL_DIR").map(std::path::PathBuf::from),
            uploaded_by,
        })
    }
}

impl NodeFiles {
    /// The node's answer to `method path`, passed through: status, the
    /// headers in [`PASSED_ON`], and the body streamed. `Range` and
    /// `If-None-Match` are forwarded from `headers`.
    async fn forward(&self, method: Method, path: &str, headers: &HeaderMap) -> Response {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        for name in [header::RANGE, header::IF_NONE_MATCH] {
            if let Some(value) = headers.get(&name) {
                request = request.header(name, value);
            }
        }
        let answer = match request.send().await {
            Ok(answer) => answer,
            Err(e) => {
                tracing::warn!(error = %e, "the node holding this operator's files did not answer");
                return refuse(StatusCode::BAD_GATEWAY, "the file store is unavailable");
            }
        };
        let status =
            StatusCode::from_u16(answer.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut passed = HeaderMap::new();
        for name in PASSED_ON {
            if let Some(value) = answer.headers().get(name.as_str())
                && let Ok(value) = header::HeaderValue::from_bytes(value.as_bytes())
            {
                passed.insert(name, value);
            }
        }
        (status, passed, Body::from_stream(answer.bytes_stream())).into_response()
    }
}

/// What a staged upload answers with. The file is the node's once committed;
/// its link, segmenting and final state come from `/api/content/{digest}/status`.
#[derive(Serialize)]
pub(super) struct Staged {
    upload_id: String,
    digest: String,
    mime: String,
    bytes: u64,
    visibility: Visibility,
    /// Always `staged`: admission, processing and commit happen at the node.
    state: &'static str,
}

/// A refusal, as the caller sees it.
#[derive(Serialize)]
struct Refusal {
    error: String,
}

fn refuse(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(Refusal {
            error: message.into(),
        }),
    )
        .into_response()
}

/// The shared upload layer's refusal, as this surface answers it.
fn render(refusal: UploadRefusal) -> Response {
    match refusal {
        UploadRefusal::TooLarge { .. } => {
            refuse(StatusCode::PAYLOAD_TOO_LARGE, refusal.to_string())
        }
        UploadRefusal::UnsupportedType => {
            refuse(StatusCode::UNSUPPORTED_MEDIA_TYPE, refusal.to_string())
        }
        UploadRefusal::Malformed(_) => refuse(StatusCode::BAD_REQUEST, refusal.to_string()),
        // The spool is this host's disk, not the operator's room: the path goes
        // to the log, the caller learns only that a retry later is worth it.
        UploadRefusal::NoSpoolSpace { ref dir } => {
            tracing::error!(spool_dir = %dir, "out of spool space while receiving an upload");
            refuse(
                StatusCode::INSUFFICIENT_STORAGE,
                "the server is out of room to receive uploads right now",
            )
        }
        // The operator's room is judged at the node, when the upload is admitted.
        UploadRefusal::OutOfSpace { .. } | UploadRefusal::Io(_) => {
            refuse(StatusCode::INTERNAL_SERVER_ERROR, refusal.to_string())
        }
    }
}

/// A file is named by the SHA-256 of its bytes and nothing else, checked
/// before the name reaches the node's URL.
fn checked_digest(digest: &str) -> Result<(), Box<Response>> {
    let well_formed = digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if well_formed {
        Ok(())
    } else {
        Err(Box::new(refuse(
            StatusCode::BAD_REQUEST,
            "not a SHA-256 digest",
        )))
    }
}

/// 503 rather than 404: "this deployment configured no files" and "no such
/// file" are different problems for whoever is debugging.
fn not_configured() -> Response {
    refuse(
        StatusCode::SERVICE_UNAVAILABLE,
        "files are not configured: this agent was given no NSED_FILES_OWNER, NSED_FILES_NODE \
         and NSED_FILES_TOKEN",
    )
}

/// `POST /api/content` — stage a file for the operator in the files pipeline.
///
/// The digest names the file and the pipeline checks it before anything
/// lands, so the body is spooled while it is hashed and the spool is what
/// gets staged.
pub(super) async fn upload(
    State(state): State<MultiAppState>,
    headers: HeaderMap,
    mut form: Multipart,
) -> Response {
    let Some(content) = state.content.clone() else {
        return not_configured();
    };
    // Refused from the declared length before the body is read, so an oversize
    // upload does not end as a reset connection minutes in.
    if let Some(refusal) = declared_too_large(&headers, content.max_upload_bytes) {
        return render(refusal);
    }
    let (visibility, spooled) = match read_upload_form(
        &mut form,
        content.max_upload_bytes,
        content.spool_dir.as_deref(),
    )
    .await
    {
        Ok(parts) => parts,
        Err(refusal) => return render(refusal),
    };
    let meta = match described(&spooled, visibility, &content.uploaded_by).await {
        Ok(meta) => meta,
        Err(refusal) => return render(refusal),
    };
    let staged = async {
        let mut bytes = tokio::fs::File::open(spooled.path()).await?;
        crate::pipeline::stage(&content.js, &content.owner, &mut bytes, &meta, None).await
    }
    .await;
    match staged {
        Ok(upload_id) => Json(Staged {
            upload_id,
            digest: meta.digest,
            mime: meta.mime,
            bytes: meta.bytes,
            visibility: meta.visibility,
            state: "staged",
        })
        .into_response(),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "could not stage an upload");
            refuse(StatusCode::BAD_GATEWAY, "could not stage the upload")
        }
    }
}

/// The type is sniffed from the bytes, never taken from the client's claim.
async fn described(
    spooled: &crate::files::upload::Spooled,
    visibility: Visibility,
    uploaded_by: &str,
) -> Result<ObjectMeta, UploadRefusal> {
    let mut head = Vec::new();
    tokio::fs::File::open(spooled.path())
        .await
        .map_err(|e| UploadRefusal::Io(format!("could not re-read the upload: {e}")))?
        .take(SNIFF_BYTES)
        .read_to_end(&mut head)
        .await
        .map_err(|e| UploadRefusal::Io(format!("could not re-read the upload: {e}")))?;
    let mime = sniff_mime(&head).ok_or(UploadRefusal::UnsupportedType)?;
    Ok(ObjectMeta {
        digest: spooled.digest.clone(),
        filename: spooled.filename.clone(),
        mime: mime.to_string(),
        bytes: spooled.bytes,
        visibility,
        uploaded_by: uploaded_by.to_string(),
        created_at: stamped_now(),
    })
}

/// `GET /api/content/{digest}` — the operator's file read back, with `Range`.
pub(super) async fn fetch(
    State(state): State<MultiAppState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Response {
    owned(state, Method::GET, &digest, "", &headers).await
}

/// `GET /api/content/{digest}/status` — where an upload or file stands.
pub(super) async fn status(
    State(state): State<MultiAppState>,
    Path(digest): Path<String>,
) -> Response {
    owned(state, Method::GET, &digest, "/status", &HeaderMap::new()).await
}

/// `DELETE /api/content/{digest}` — the file and everything derived from it.
pub(super) async fn remove(
    State(state): State<MultiAppState>,
    Path(digest): Path<String>,
) -> Response {
    owned(state, Method::DELETE, &digest, "", &HeaderMap::new()).await
}

/// `GET /api/content/usage` — the operator's room and what is spent of it.
pub(super) async fn usage(State(state): State<MultiAppState>) -> Response {
    let Some(content) = state.content.as_ref() else {
        return not_configured();
    };
    content
        .node
        .forward(Method::GET, "/api/operators/storage", &HeaderMap::new())
        .await
}

async fn owned(
    state: MultiAppState,
    method: Method,
    digest: &str,
    suffix: &str,
    headers: &HeaderMap,
) -> Response {
    let Some(content) = state.content.as_ref() else {
        return not_configured();
    };
    if let Err(refusal) = checked_digest(digest) {
        return *refusal;
    }
    content
        .node
        .forward(method, &format!("/api/content/{digest}{suffix}"), headers)
        .await
}

#[cfg(test)]
mod tests;
