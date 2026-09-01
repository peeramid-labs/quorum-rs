//! Uploading files an operator can hand out a public link to.
//!
//! Bytes go straight from the request into the operator's object-store bucket:
//! the agent holds that bucket's credentials, which is why uploads are not
//! routed through anything else. Reading them back publicly is a different
//! process's job — this one returns the URL that process will serve.

use crate::files::blob::{
    Blob, NatsBlob, ObjectMeta, Usage, Visibility, open_blob, parse_byte_range,
};
use crate::files::hls::{HLS_NOTE, PLAYLIST_NOTE};
use crate::files::upload::{Refusal as UploadRefusal, Spooled, read_upload_form, store_upload};
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::sync::Arc;

use super::MultiAppState;

/// The largest upload accepted when nothing says otherwise.
const DEFAULT_MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;

/// Bounded so a whole-video read is never assembled in memory before answering.
const READ_WINDOW: u64 = 512 * 1024;

/// Everything the upload routes need, or `None` where the agent was not given
/// a bucket to write to.
#[derive(Clone)]
pub struct ContentUploads {
    blob: Arc<NatsBlob>,
    max_upload_bytes: u64,
    /// What an uploader is handed as the file's public address. The serving
    /// side is another process, so this is configuration, not something that
    /// can be derived here.
    public_base: Option<String>,
    uploaded_by: String,
    /// Where an upload is spooled while it is hashed.
    ///
    /// `None` means the platform temporary directory, which in a container is
    /// routinely a small tmpfs — an upload needs as much room here as it has
    /// bytes, so a deployment that accepts video should point this at a volume.
    spool_dir: Option<std::path::PathBuf>,
    /// Segmenting for uploaded video. `None` where no transcoder was found on
    /// this host, which is not an error — a library user who never asked for a
    /// media pipeline gets their original stored and nothing else.
    hls: Option<Arc<Segmenting>>,
}

/// How many uploads may be waiting to segment.
///
/// Each one holds its spool on disk until its turn comes, so an unbounded
/// queue is an unbounded pile of temporary files the size of the uploads that
/// made them. Past this an upload is stored whole and reported `Skipped`,
/// which is honest and costs nothing.
const MAX_QUEUED_SEGMENTATIONS: usize = 4;

/// The transcoder, and the queue in front of it.
struct Segmenting {
    transcoder: crate::files::hls::Ffmpeg,
    /// Admission: bounds how many spools are being held for a turn.
    waiting: Arc<tokio::sync::Semaphore>,
    /// One transcode at a time. Parallel ffmpeg is how a host with several
    /// uploads in flight stops answering anything at all.
    running: tokio::sync::Semaphore,
}

impl ContentUploads {
    /// Read the configuration from this process's environment.
    ///
    /// `None` — uploads disabled, the routes answer 503 — unless
    /// `NSED_FILES_BUCKET` names a bucket. The agent is handed the name rather
    /// than deriving it, the same way a seat is handed the bucket it writes
    /// candidates to: naming is the deployment's, and the deployment is what
    /// provisioned the bucket and its ceiling.
    pub async fn from_env(
        js: &async_nats::jetstream::Context,
        uploaded_by: String,
    ) -> Option<Self> {
        let bucket = std::env::var("NSED_FILES_BUCKET")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())?;

        let blob = match open_blob(js, &bucket).await {
            Ok(blob) => blob,
            Err(e) => {
                tracing::warn!(
                    "NSED_FILES_BUCKET is {bucket:?} but it could not be opened, so uploads stay \
                     disabled: {e:#}"
                );
                return None;
            }
        };

        let transcoder = crate::files::hls::Ffmpeg::default();
        let hls = if transcoder.available().await {
            Some(Arc::new(Segmenting {
                transcoder,
                waiting: Arc::new(tokio::sync::Semaphore::new(MAX_QUEUED_SEGMENTATIONS)),
                running: tokio::sync::Semaphore::new(1),
            }))
        } else {
            tracing::info!(
                "no ffmpeg on this host, so uploaded video is stored whole and not segmented"
            );
            None
        };

        Some(Self {
            blob: Arc::new(blob),
            hls,
            max_upload_bytes: std::env::var("NSED_MAX_UPLOAD_BYTES")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(DEFAULT_MAX_UPLOAD_BYTES),
            public_base: std::env::var("NSED_PUBLIC_CONTENT_BASE")
                .ok()
                .map(|v| v.trim().trim_end_matches('/').to_string())
                .filter(|v| !v.is_empty()),
            spool_dir: std::env::var("NSED_UPLOAD_SPOOL_DIR")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(std::path::PathBuf::from),
            uploaded_by,
        })
    }

    /// The link an uploader can hand out, where one can be built.
    fn public_url(&self, digest: &str) -> Option<String> {
        self.public_base
            .as_ref()
            .map(|base| format!("{base}/{digest}"))
    }
}

/// What an upload answers with.
#[derive(Serialize)]
pub(super) struct Uploaded {
    /// `<scheme>://<bucket>/<digest>` — how the content plane names it.
    address: String,
    digest: String,
    /// The link to hand out. Absent where the deployment configured no public
    /// base, so a caller is never given a URL that resolves to nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    mime: String,
    bytes: u64,
    visibility: Visibility,
    /// Where segmenting stands for this upload. `Skipped` for anything that is
    /// not video, or on a host with no transcoder.
    hls: crate::files::hls::HlsState,
}

/// A refusal, as the caller sees it.
#[derive(Serialize)]
struct Refusal {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    used_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota_bytes: Option<u64>,
}

/// A refusal to return through `?`. Boxed because a `Response` is large and a
/// `Result` carrying one inline makes every success path pay for it.
fn boxed(status: StatusCode, message: impl Into<String>) -> Box<Response> {
    Box::new(refuse(status, message))
}

/// The shared upload layer's refusal, as this surface answers it.
fn render(refusal: UploadRefusal) -> Box<Response> {
    match refusal {
        UploadRefusal::TooLarge { .. } => boxed(StatusCode::PAYLOAD_TOO_LARGE, refusal.to_string()),
        UploadRefusal::UnsupportedType => {
            boxed(StatusCode::UNSUPPORTED_MEDIA_TYPE, refusal.to_string())
        }
        UploadRefusal::Malformed(_) => boxed(StatusCode::BAD_REQUEST, refusal.to_string()),
        // 507 with the numbers, not a 500 with a string: the caller can act on
        // "you are out of space", and cannot act on "something went wrong".
        UploadRefusal::OutOfSpace {
            used_bytes,
            quota_bytes,
        } => Box::new(
            (
                StatusCode::INSUFFICIENT_STORAGE,
                Json(Refusal {
                    error: refusal.to_string(),
                    used_bytes: Some(used_bytes),
                    quota_bytes: Some(quota_bytes),
                }),
            )
                .into_response(),
        ),
        // The server is out of room, not the operator. Logged with the path so
        // whoever runs it knows which filesystem filled; the caller is told
        // only that retrying later is worth it, since a server path is not
        // theirs to see.
        UploadRefusal::NoSpoolSpace { ref dir } => {
            tracing::error!(spool_dir = %dir, "out of spool space while receiving an upload");
            boxed(
                StatusCode::INSUFFICIENT_STORAGE,
                "the server is out of room to receive uploads right now",
            )
        }
        UploadRefusal::Io(_) => boxed(StatusCode::INTERNAL_SERVER_ERROR, refusal.to_string()),
    }
}

fn refuse(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(Refusal {
            error: message.into(),
            used_bytes: None,
            quota_bytes: None,
        }),
    )
        .into_response()
}

/// A stored object is named by the SHA-256 of its bytes, and nothing else.
///
/// Checked before the name reaches the store: annotation sidecars live in the
/// same bucket under a reserved prefix, so an unchecked name lets a caller
/// read and delete the bookkeeping of objects that are not theirs.
fn checked_digest(digest: &str) -> Result<(), Box<Response>> {
    let well_formed = digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if well_formed {
        Ok(())
    } else {
        Err(boxed(
            StatusCode::BAD_REQUEST,
            "not a SHA-256 digest".to_string(),
        ))
    }
}

/// 503 rather than 404: "this deployment configured no uploads" and "no such
/// file" are different problems for whoever is debugging.
fn not_configured() -> Response {
    refuse(
        StatusCode::SERVICE_UNAVAILABLE,
        "uploads are not configured: this agent was given no NSED_FILES_BUCKET",
    )
}

/// `POST /api/content` — store a file and return the address it can be read at.
///
/// The digest names the object, so it has to be known before the store is
/// written to, and a video cannot be held in memory to hash it. The body is
/// therefore spooled to a temporary file while being hashed, and the spool is
/// what gets streamed into the bucket.
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
    if let Some(refusal) =
        crate::files::upload::declared_too_large(&headers, content.max_upload_bytes)
    {
        return *render(refusal);
    }

    let (visibility, mut spooled) = match read_upload_form(
        &mut form,
        content.max_upload_bytes,
        content.spool_dir.as_deref(),
    )
    .await
    {
        Ok(parts) => parts,
        Err(refusal) => return *render(refusal),
    };

    let stored = match store_upload(
        content.blob.as_ref(),
        &mut spooled,
        visibility,
        &content.uploaded_by,
    )
    .await
    {
        Ok(stored) => stored,
        Err(refusal) => return *render(refusal),
    };

    let hls = start_segmenting(&content, &stored, spooled);
    Json(Uploaded {
        address: format!("nats://{}/{}", content.blob.bucket(), stored.digest),
        url: content.public_url(&stored.digest),
        digest: stored.digest,
        mime: stored.mime,
        bytes: stored.bytes,
        visibility: stored.visibility,
        hls,
    })
    .into_response()
}

/// Kick off segmenting for a video, and say what state that leaves it in.
///
/// Returns without waiting: a transcode takes far longer than an HTTP request
/// should, and the original is already stored and servable. The uploader polls
/// for the playlist.
fn start_segmenting(
    content: &ContentUploads,
    stored: &ObjectMeta,
    spool: Spooled,
) -> crate::files::hls::HlsState {
    let (Some(hls), Some(public_base)) = (content.hls.clone(), content.public_base.clone()) else {
        // Nothing to segment with, or nowhere to point a playlist at: a
        // playlist of relative names nobody can resolve is worse than none.
        return crate::files::hls::HlsState::Skipped;
    };
    if !stored.mime.starts_with("video/") {
        return crate::files::hls::HlsState::Skipped;
    }
    // Segments are the video. Publishing them for a private upload would
    // publish the upload, whatever the original object is marked.
    if stored.visibility != Visibility::Public {
        return crate::files::hls::HlsState::Skipped;
    }

    // Taken before the spool is moved into a task, so a backlog is refused
    // rather than accumulating temporary files.
    let Ok(slot) = hls.waiting.clone().try_acquire_owned() else {
        tracing::warn!(
            digest = %stored.digest,
            "the segmenting queue is full, so this upload is stored whole"
        );
        return crate::files::hls::HlsState::Skipped;
    };

    let blob = content.blob.clone();
    let uploaded_by = content.uploaded_by.clone();
    let digest = stored.digest.clone();
    let visibility = stored.visibility;
    tokio::spawn(async move {
        let _slot = slot;
        // The spool is moved in and dropped here, at the end of the transcode,
        // rather than when the request returned — ffmpeg reads it.
        let spool = spool;
        if let Err(e) = blob.annotate(&digest, HLS_NOTE, "pending").await {
            tracing::warn!(digest, error = %format!("{e:#}"), "could not mark an upload pending");
        }

        let _turn = hls.running.acquire().await;
        let outcome = crate::files::hls::segment_and_store(
            blob.as_ref(),
            &hls.transcoder,
            &digest,
            spool.path(),
            &public_base,
            &uploaded_by,
            visibility,
        )
        .await;

        let state = match outcome {
            Ok(segmented) => {
                let _ = blob
                    .annotate(&digest, PLAYLIST_NOTE, &segmented.playlist)
                    .await;
                tracing::info!(digest, playlist = %segmented.playlist, "segmented an upload");
                "ready"
            }
            Err(e) => {
                // The original is untouched and still servable whole; only the
                // seekable form is missing.
                tracing::warn!(digest, error = %format!("{e:#}"), "could not segment an upload");
                "failed"
            }
        };
        if let Err(e) = blob.annotate(&digest, HLS_NOTE, state).await {
            tracing::warn!(digest, error = %format!("{e:#}"), "could not record the segmenting outcome");
        }
    });

    crate::files::hls::HlsState::Pending
}

/// A body that reads windows as the client consumes them.
///
/// Streamed, not assembled: a whole-object GET of a video would otherwise hold
/// the entire file in memory before the first byte reached the client.
fn stream_object(blob: Arc<NatsBlob>, digest: String, first: u64, wanted: u64) -> Body {
    Body::from_stream(futures::stream::unfold(
        (blob, digest, first, wanted),
        |(blob, digest, offset, remaining)| async move {
            if remaining == 0 {
                return None;
            }
            match blob
                .get_range(&digest, offset, remaining.min(READ_WINDOW))
                .await
            {
                Ok((bytes, _)) if bytes.is_empty() => None,
                Ok((bytes, _)) => {
                    let read = bytes.len() as u64;
                    Some((
                        Ok(bytes),
                        (blob, digest, offset + read, remaining.saturating_sub(read)),
                    ))
                }
                Err(e) => Some((
                    Err(std::io::Error::other(format!("{e:#}"))),
                    (blob, digest, offset, 0),
                )),
            }
        },
    ))
}

/// The slice to answer with, and the status that describes it.
///
/// A range header that cannot be satisfied is an error, not a silent
/// whole-object answer: the caller asked for a slice and would otherwise
/// splice the whole file where the slice belonged.
fn resolve_range(headers: &HeaderMap, total: u64) -> Result<(u64, u64, StatusCode), Box<Response>> {
    let Some(raw) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return Ok((0, total.saturating_sub(1), StatusCode::OK));
    };
    match parse_byte_range(raw, total) {
        Some((first, last)) => Ok((first, last, StatusCode::PARTIAL_CONTENT)),
        None => Err(Box::new(
            (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{total}"))],
            )
                .into_response(),
        )),
    }
}

/// `GET /api/content/{digest}` — the agent's own read-back, with `Range`.
pub(super) async fn fetch(
    State(state): State<MultiAppState>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(content) = state.content.as_ref() else {
        return not_configured();
    };
    if let Err(refusal) = checked_digest(&digest) {
        return *refusal;
    }

    let meta = match content.blob.head(&digest).await {
        Ok(meta) => meta,
        Err(_) => return refuse(StatusCode::NOT_FOUND, "no such object"),
    };

    let (first, last, status) = match resolve_range(&headers, meta.bytes) {
        Ok(slice) => slice,
        Err(refusal) => return *refusal,
    };

    if meta.bytes == 0 {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, meta.mime)],
            Body::empty(),
        )
            .into_response();
    }

    let wanted = last - first + 1;
    let body = stream_object(content.blob.clone(), digest.clone(), first, wanted);

    let mut response_headers = HeaderMap::new();
    let set = |headers: &mut HeaderMap, name: header::HeaderName, value: String| {
        if let Ok(value) = value.parse() {
            headers.insert(name, value);
        }
    };
    set(
        &mut response_headers,
        header::CONTENT_TYPE,
        meta.mime.clone(),
    );
    set(
        &mut response_headers,
        header::ACCEPT_RANGES,
        "bytes".to_string(),
    );
    set(
        &mut response_headers,
        header::ETAG,
        format!("\"{}\"", meta.digest),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        set(
            &mut response_headers,
            header::CONTENT_RANGE,
            format!("bytes {first}-{last}/{}", meta.bytes),
        );
    }
    set(
        &mut response_headers,
        header::CONTENT_LENGTH,
        wanted.to_string(),
    );
    (status, response_headers, body).into_response()
}

/// What is known about one stored file, including where segmenting got to.
#[derive(Serialize)]
struct FileStatus {
    #[serde(flatten)]
    meta: ObjectMeta,
    hls: crate::files::hls::HlsState,
    /// The playlist to hand a player, once there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    playlist_url: Option<String>,
}

/// `GET /api/content/{digest}/status` — the uploader's poll.
///
/// Segmenting outlives the request that started it, so this is how a caller
/// learns the video became seekable.
pub(super) async fn status(
    State(state): State<MultiAppState>,
    Path(digest): Path<String>,
) -> Response {
    let Some(content) = state.content.as_ref() else {
        return not_configured();
    };
    if let Err(refusal) = checked_digest(&digest) {
        return *refusal;
    }
    let Ok(meta) = content.blob.head(&digest).await else {
        return refuse(StatusCode::NOT_FOUND, "no such object");
    };
    let notes = content.blob.notes(&digest).await.unwrap_or_default();

    let hls = crate::files::hls::HlsState::from_note(notes.get(HLS_NOTE).map(String::as_str));
    // Only once it is ready. A playlist note can outlive the segmentation that
    // wrote it — a later re-run that failed, say — and handing out a URL for a
    // playlist whose segments are gone fails in the player rather than here.
    let playlist_url = (hls == crate::files::hls::HlsState::Ready)
        .then(|| notes.get(PLAYLIST_NOTE).and_then(|d| content.public_url(d)))
        .flatten();

    Json(FileStatus {
        meta,
        hls,
        playlist_url,
    })
    .into_response()
}

/// `DELETE /api/content/{digest}` — remove an object and free its space.
pub(super) async fn remove(
    State(state): State<MultiAppState>,
    Path(digest): Path<String>,
) -> Response {
    let Some(content) = state.content.as_ref() else {
        return not_configured();
    };
    if let Err(refusal) = checked_digest(&digest) {
        return *refusal;
    }
    // Segments too, or deleting a video would not stop it being watchable.
    match crate::files::hls::delete_with_derived(content.blob.as_ref(), &digest).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => refuse(StatusCode::NOT_FOUND, "no such object"),
    }
}

/// `GET /api/content/usage` — how much of the ceiling is spent.
pub(super) async fn usage(State(state): State<MultiAppState>) -> Response {
    let Some(content) = state.content.as_ref() else {
        return not_configured();
    };
    match content.blob.usage().await {
        Ok(usage) => Json::<Usage>(usage).into_response(),
        Err(e) => refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not read usage: {e:#}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::files::blob::{Usage, open_blob};
    use crate::nats_utils::ensure_object_bucket;
    use async_nats::jetstream::{self, object_store};
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    const BOUNDARY: &str = "----quorumtestboundary";

    fn multipart(filename: &str, bytes: &[u8], visibility: Option<&str>) -> Vec<u8> {
        let mut body = Vec::new();
        if let Some(visibility) = visibility {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data;                      name=\"visibility\"\r\n\r\n{visibility}\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\";                  filename=\"{filename}\"\r\nContent-Type:                  application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    /// Bytes that sniff as an mp4, of the requested length.
    fn fake_mp4(len: usize) -> Vec<u8> {
        let mut bytes = b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00".to_vec();
        bytes.resize(len.max(bytes.len()), 0x42);
        bytes
    }

    async fn context() -> Option<jetstream::Context> {
        let url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".into());
        async_nats::connect(&url).await.ok().map(jetstream::new)
    }

    fn require_broker(reason: &str) {
        let required = std::env::var("REQUIRE_NATS")
            .map(|v| {
                let v = v.trim().to_string();
                !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
            })
            .unwrap_or(false);
        assert!(
            !required,
            "REQUIRE_NATS is set, so this test may not skip: {reason}"
        );
        eprintln!("Skipping: {reason}");
    }

    /// Uploads configured over a throwaway bucket. Small on purpose:
    /// JetStream reserves `max_bytes`, so provisioning at a production ceiling
    /// exhausts the broker's store for every later test.
    async fn configured(tag: &str, max_upload_bytes: u64, quota: i64) -> Option<ContentUploads> {
        let js = context().await?;
        // One per test — they run concurrently within a binary.
        let bucket = format!("test_upload_{tag}");
        // Deleted first: the bucket count stays bounded by the number of tests
        // rather than growing with each run, and every test starts clean
        // without teardown a panic would skip.
        let _ = js.delete_object_store(&bucket).await;
        ensure_object_bucket(
            &js,
            object_store::Config {
                bucket: bucket.clone(),
                max_bytes: quota,
                storage: jetstream::stream::StorageType::File,
                num_replicas: 1,
                ..Default::default()
            },
        )
        .await
        .expect("open the test bucket");
        Some(ContentUploads {
            blob: Arc::new(open_blob(&js, &bucket).await.expect("open blob")),
            max_upload_bytes,
            public_base: Some("https://example.test/content/acme".to_string()),
            uploaded_by: "agent-7".to_string(),
            spool_dir: None,
            // No transcoder in the tests that exercise the HTTP surface: the
            // pipeline has its own, and spawning one here would make every
            // upload test wait on a subprocess.
            hls: None,
        })
    }

    /// The same configuration with a transcoder attached.
    ///
    /// Without one `start_segmenting` returns at its first guard, so a test
    /// that means to exercise a later gate — the media type, the visibility —
    /// would pass for any input at all.
    fn with_transcoder(content: ContentUploads) -> ContentUploads {
        ContentUploads {
            hls: Some(Arc::new(Segmenting {
                transcoder: crate::files::hls::Ffmpeg::default(),
                waiting: Arc::new(tokio::sync::Semaphore::new(4)),
                running: tokio::sync::Semaphore::new(1),
            })),
            ..content
        }
    }

    async fn send(content: Option<ContentUploads>, request: Request<Body>) -> Response {
        super::super::build_router(MultiAppState::for_content(content))
            .oneshot(request)
            .await
            .expect("the router answers")
    }

    fn upload_request(body: Vec<u8>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/content")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .expect("request")
    }

    async fn body_bytes(response: Response) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec()
    }

    async fn body_json(response: Response) -> serde_json::Value {
        serde_json::from_slice(&body_bytes(response).await).expect("json body")
    }

    #[tokio::test]
    async fn an_uploaded_video_comes_back_whole_and_by_range() {
        let Some(content) = configured(
            "an_uploaded_video_comes_back_whole_and_by_range",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("uploading needs a broker");
            return;
        };

        let video = fake_mp4(300_000);
        let response = send(
            Some(content.clone()),
            upload_request(multipart("clip.mp4", &video, Some("public"))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let uploaded = body_json(response).await;

        assert_eq!(
            uploaded["mime"], "video/mp4",
            "the type is sniffed, not taken from the part"
        );
        assert_eq!(uploaded["bytes"], video.len());
        assert_eq!(uploaded["visibility"], "public");
        let digest = uploaded["digest"].as_str().expect("a digest").to_string();
        assert_eq!(
            uploaded["url"],
            format!("https://example.test/content/acme/{digest}"),
            "the uploader is handed the link to share"
        );

        // Whole.
        let response = send(
            Some(content.clone()),
            Request::builder()
                .uri(format!("/api/content/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "video/mp4"
        );
        assert_eq!(
            response.headers().get(header::ACCEPT_RANGES).unwrap(),
            "bytes"
        );
        assert_eq!(body_bytes(response).await, video);

        // A slice — what a player asks for when someone scrubs.
        let response = send(
            Some(content),
            Request::builder()
                .uri(format!("/api/content/{digest}"))
                .header(header::RANGE, "bytes=1000-1999")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(header::CONTENT_RANGE).unwrap(),
            &format!("bytes 1000-1999/{}", video.len())
        );
        assert_eq!(body_bytes(response).await, video[1000..2000]);
    }

    #[tokio::test]
    async fn a_host_with_no_transcoder_stores_the_video_and_says_it_skipped() {
        // Not an error, and not a lie about being pending: a library user who
        // never asked for a media pipeline gets their original and a state
        // that says why there is no playlist.
        let Some(content) = configured(
            "a_host_with_no_transcoder_stores_the_video_and_says_it_skipped",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("uploading needs a broker");
            return;
        };
        let uploaded = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("clip.mp4", &fake_mp4(2_000), Some("public"))),
            )
            .await,
        )
        .await;
        assert_eq!(uploaded["mime"], "video/mp4");
        assert_eq!(uploaded["hls"], "skipped");

        // And the original is servable whole, which is the point of not
        // failing the upload.
        let digest = uploaded["digest"].as_str().unwrap();
        let response = send(
            Some(content),
            Request::builder()
                .uri(format!("/api/content/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_still_image_is_never_queued_for_segmenting() {
        let Some(content) = configured(
            "a_still_image_is_never_queued_for_segmenting",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("uploading needs a broker");
            return;
        };
        let png = b"\x89PNG\r\n\x1a\n and some pixels".to_vec();
        let uploaded = body_json(
            send(
                Some(with_transcoder(content)),
                upload_request(multipart("shot.png", &png, Some("public"))),
            )
            .await,
        )
        .await;
        assert_eq!(uploaded["mime"], "image/png");
        assert_eq!(uploaded["hls"], "skipped");
    }

    #[tokio::test]
    async fn a_private_video_is_never_queued_for_segmenting() {
        // Segments are the video, so the gate belongs before the queue.
        let Some(content) = configured(
            "a_private_video_is_never_queued_for_segmenting",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("the private-video gate needs a broker");
            return;
        };
        let uploaded = body_json(
            send(
                Some(with_transcoder(content.clone())),
                upload_request(multipart("clip.mp4", &fake_mp4(2_000), Some("private"))),
            )
            .await,
        )
        .await;
        assert_eq!(uploaded["visibility"], "private");
        assert_eq!(
            uploaded["hls"], "skipped",
            "a private video must not be segmented into public pieces"
        );
        assert_eq!(
            content.blob.list().await.expect("list").len(),
            1,
            "nothing beyond the original may be stored"
        );
    }

    #[tokio::test]
    async fn status_reports_what_segmenting_recorded() {
        // The uploader's poll: segmenting outlives the request that started
        // it, so this is how they learn the video became seekable.
        let Some(content) = configured(
            "status_reports_what_segmenting_recorded",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("status needs a broker");
            return;
        };
        let uploaded = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("clip.mp4", &fake_mp4(2_000), Some("public"))),
            )
            .await,
        )
        .await;
        let digest = uploaded["digest"].as_str().unwrap().to_string();

        let status = |content: Option<ContentUploads>, digest: String| async move {
            body_json(
                send(
                    content,
                    Request::builder()
                        .uri(format!("/api/content/{digest}/status"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await,
            )
            .await
        };

        let before = status(Some(content.clone()), digest.clone()).await;
        assert_eq!(before["hls"], "skipped", "nothing has segmented it");
        assert_eq!(before["mime"], "video/mp4");
        assert!(
            before["playlist_url"].is_null(),
            "no playlist has been produced"
        );

        // Simulate what the background transcode records.
        let playlist = "d".repeat(64);
        content
            .blob
            .annotate(&digest, PLAYLIST_NOTE, &playlist)
            .await
            .expect("annotate");
        content
            .blob
            .annotate(&digest, HLS_NOTE, "ready")
            .await
            .expect("annotate");

        let after = status(Some(content.clone()), digest.clone()).await;
        assert_eq!(after["hls"], "ready");
        assert_eq!(
            after["playlist_url"],
            format!("https://example.test/content/acme/{playlist}"),
            "the uploader is handed the link a player needs"
        );

        // A note left by a segmentation that later failed must not be handed
        // out: its segments are gone, so the URL would fail in the player.
        content
            .blob
            .annotate(&digest, HLS_NOTE, "failed")
            .await
            .expect("annotate");
        let stale = status(Some(content), digest).await;
        assert_eq!(stale["hls"], "failed");
        assert!(
            stale["playlist_url"].is_null(),
            "a playlist is only offered once it is ready: {stale}"
        );
    }

    #[tokio::test]
    async fn status_for_a_file_that_does_not_exist_is_a_404() {
        let Some(content) = configured(
            "status_for_a_file_that_does_not_exist_is_a_404",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("status needs a broker");
            return;
        };
        let response = send(
            Some(content),
            Request::builder()
                .uri(format!("/api/content/{}/status", "e".repeat(64)))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_range_that_cannot_be_satisfied_is_refused_not_widened() {
        // Answering the whole object would have the caller splice all of it
        // where a slice belonged.
        let Some(content) = configured(
            "a_range_that_cannot_be_satisfied_is_refused_not_widened",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("range refusal needs a broker");
            return;
        };
        let video = fake_mp4(2_000);
        let uploaded = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("clip.mp4", &video, Some("public"))),
            )
            .await,
        )
        .await;
        let digest = uploaded["digest"].as_str().unwrap().to_string();

        let response = send(
            Some(content),
            Request::builder()
                .uri(format!("/api/content/{digest}"))
                .header(header::RANGE, "bytes=9000-9999")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            response.headers().get(header::CONTENT_RANGE).unwrap(),
            &format!("bytes */{}", video.len())
        );
    }

    #[tokio::test]
    async fn a_type_this_deployment_does_not_serve_is_refused() {
        let Some(content) = configured(
            "a_type_this_deployment_does_not_serve_is_refused",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("type refusal needs a broker");
            return;
        };
        let response = send(
            Some(content),
            upload_request(multipart(
                "payload.html",
                b"<!DOCTYPE html><script>",
                Some("public"),
            )),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn an_upload_that_declares_itself_too_large_never_reaches_the_spool() {
        // Rejecting mid-body reaches the client as a reset connection, so a
        // long upload ends in "the connection dropped" and says nothing about
        // the ceiling. A declared length can be answered before a byte lands.
        let Some(content) = configured(
            "an_upload_that_declares_itself_too_large_never_reaches_the_spool",
            64 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("the declared-size check needs a broker");
            return;
        };
        let claimed = Request::builder()
            .method("POST")
            .uri("/api/content")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .header(header::CONTENT_LENGTH, (100 * 1024 * 1024).to_string())
            .body(Body::empty())
            .expect("request");

        let response = send(Some(content.clone()), claimed).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            content.blob.list().await.expect("list").is_empty(),
            "nothing may be stored for an upload that was never read"
        );
    }

    #[tokio::test]
    async fn an_upload_past_the_ceiling_is_refused_while_it_streams() {
        let Some(content) = configured(
            "an_upload_past_the_ceiling_is_refused_while_it_streams",
            64 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("the size cap needs a broker");
            return;
        };
        let response = send(
            Some(content),
            upload_request(multipart("big.mp4", &fake_mp4(200_000), Some("public"))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_full_bucket_answers_with_the_numbers_not_a_five_hundred() {
        // A caller can act on "you are out of space" and cannot act on
        // "something went wrong".
        let Some(content) = configured(
            "a_full_bucket_answers_with_the_numbers_not_a_five_hundred",
            8 * 1024 * 1024,
            64 * 1024,
        )
        .await
        else {
            require_broker("quota refusal needs a broker");
            return;
        };
        let first = send(
            Some(content.clone()),
            upload_request(multipart("a.mp4", &fake_mp4(40_000), Some("public"))),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);

        let mut second_bytes = fake_mp4(40_000);
        second_bytes[100] = 0x01; // different content, so a different digest
        let response = send(
            Some(content),
            upload_request(multipart("b.mp4", &second_bytes, Some("public"))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);
        let refusal = body_json(response).await;
        assert_eq!(refusal["quota_bytes"], 64 * 1024);
        assert!(refusal["used_bytes"].is_number());
    }

    #[tokio::test]
    async fn visibility_is_recorded_as_asked_for() {
        let Some(content) = configured(
            "visibility_is_recorded_as_asked_for",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("visibility needs a broker");
            return;
        };
        let uploaded = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("clip.mp4", &fake_mp4(1_000), Some("private"))),
            )
            .await,
        )
        .await;
        assert_eq!(uploaded["visibility"], "private");

        let digest = uploaded["digest"].as_str().unwrap();
        let stored = content.blob.head(digest).await.expect("head");
        assert_eq!(stored.visibility, Visibility::Private);
        assert_eq!(stored.filename, "clip.mp4");
        assert_eq!(stored.uploaded_by, "agent-7");
    }

    #[tokio::test]
    async fn an_unknown_visibility_is_refused_rather_than_defaulted_to_public() {
        // Defaulting would publish a file whose uploader asked for something
        // else, and the mistake is invisible until the link is out.
        let Some(content) = configured(
            "an_unknown_visibility_is_refused_rather_than_defaulted_to_public",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("visibility refusal needs a broker");
            return;
        };
        let response = send(
            Some(content),
            upload_request(multipart("clip.mp4", &fake_mp4(1_000), Some("unlisted"))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn deleting_frees_the_space_and_the_object_is_gone() {
        let Some(content) = configured(
            "deleting_frees_the_space_and_the_object_is_gone",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("deleting needs a broker");
            return;
        };
        let uploaded = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("clip.mp4", &fake_mp4(50_000), Some("public"))),
            )
            .await,
        )
        .await;
        let digest = uploaded["digest"].as_str().unwrap().to_string();

        let before: Usage = serde_json::from_value(
            body_json(
                send(
                    Some(content.clone()),
                    Request::builder()
                        .uri("/api/content/usage")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await,
            )
            .await,
        )
        .expect("usage");
        assert!(before.used_bytes >= 50_000);
        assert_eq!(before.quota_bytes, 4 * 1024 * 1024);

        let response = send(
            Some(content.clone()),
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/content/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let response = send(
            Some(content),
            Request::builder()
                .uri(format!("/api/content/{digest}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_agent_with_no_bucket_says_so_rather_than_reporting_nothing_found() {
        // 503 and 404 mean different things to whoever is debugging: one is a
        // deployment that forgot to configure uploads, the other is a file
        // that was never there.
        let response = send(
            None,
            upload_request(multipart("clip.mp4", &fake_mp4(100), Some("public"))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let response = send(
            None,
            Request::builder()
                .uri("/api/content/usage")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn an_upload_that_does_not_say_public_or_private_is_refused() {
        // Neither default is safe. Defaulting to public publishes a file whose
        // uploader may have meant otherwise, and the mistake is invisible
        // until the link is out; defaulting to private silently breaks the
        // sharing this endpoint exists for.
        let Some(content) = configured(
            "an_upload_that_does_not_say_public_or_private_is_refused",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("the visibility requirement needs a broker");
            return;
        };
        let response = send(
            Some(content.clone()),
            upload_request(multipart("clip.mp4", &fake_mp4(1_000), None)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            body_json(response).await["error"]
                .as_str()
                .unwrap_or_default()
                .contains("visibility"),
            "the refusal must name the missing part"
        );

        // And nothing was stored despite the bytes having been read.
        assert!(content.blob.list().await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn an_oversize_form_field_is_refused_before_it_is_buffered() {
        // `Field::text()` has no ceiling of its own and this route disables the
        // request-level limit so a video can through — an unbounded non-file
        // part would otherwise be an unbounded allocation.
        let Some(content) = configured(
            "an_oversize_form_field_is_refused_before_it_is_buffered",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("the field cap needs a broker");
            return;
        };
        let huge = "p".repeat(crate::files::upload::MAX_FIELD_BYTES * 4);
        let response = send(
            Some(content),
            upload_request(multipart("clip.mp4", &fake_mp4(1_000), Some(&huge))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_digest_shaped_probe_never_reaches_the_store() {
        // Annotation sidecars share the bucket under a reserved prefix, so an
        // unchecked name would let a caller read and delete the bookkeeping of
        // objects that are not theirs.
        let Some(content) = configured(
            "a_digest_shaped_probe_never_reaches_the_store",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("digest validation needs a broker");
            return;
        };
        for probe in [
            "notes.abc",
            "../secrets",
            "",
            &"A".repeat(64),
            &"a".repeat(63),
        ] {
            for (method, suffix) in [("GET", ""), ("DELETE", ""), ("GET", "/status")] {
                let response = send(
                    Some(content.clone()),
                    Request::builder()
                        .method(method)
                        .uri(format!("/api/content/{probe}{suffix}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await;
                assert!(
                    matches!(
                        response.status(),
                        StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND
                    ),
                    "{method} {probe:?}{suffix} answered {}",
                    response.status()
                );
            }
        }
    }

    #[tokio::test]
    async fn an_upload_with_no_file_part_is_a_bad_request() {
        let Some(content) = configured(
            "an_upload_with_no_file_part_is_a_bad_request",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("the empty-upload check needs a broker");
            return;
        };
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data;              name=\"visibility\"\r\n\r\npublic\r\n--{BOUNDARY}--\r\n"
        );
        let response = send(Some(content), upload_request(body.into_bytes())).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn uploading_the_same_video_twice_yields_one_object() {
        let Some(content) = configured(
            "uploading_the_same_video_twice_yields_one_object",
            8 * 1024 * 1024,
            4 * 1024 * 1024,
        )
        .await
        else {
            require_broker("idempotent upload needs a broker");
            return;
        };
        let video = fake_mp4(20_000);
        let first = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("clip.mp4", &video, Some("public"))),
            )
            .await,
        )
        .await;
        let second = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("same-again.mp4", &video, Some("public"))),
            )
            .await,
        )
        .await;
        assert_eq!(first["digest"], second["digest"], "the digest is the name");
        assert_eq!(content.blob.list().await.expect("list").len(), 1);
    }
}
