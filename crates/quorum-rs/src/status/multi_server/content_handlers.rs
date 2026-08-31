//! Uploading files an operator can hand out a public link to.
//!
//! Bytes go straight from the request into the operator's object-store bucket:
//! the agent holds that bucket's credentials, which is why uploads are not
//! routed through anything else. Reading them back publicly is a different
//! process's job — this one returns the URL that process will serve.

use crate::content_blob::{
    Blob, NatsBlob, ObjectMeta, QuotaExceeded, Usage, Visibility, open_blob, parse_byte_range,
    stamped_now,
};
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::MultiAppState;

/// The largest upload accepted when nothing says otherwise.
const DEFAULT_MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;

/// How much of a file is read before its type is decided.
const SNIFF_BYTES: usize = 16;

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
    transcoder: crate::hls::Ffmpeg,
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

        let transcoder = crate::hls::Ffmpeg::default();
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
    hls: crate::hls::HlsState,
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

/// 503 rather than 404: "this deployment configured no uploads" and "no such
/// file" are different problems for whoever is debugging.
fn not_configured() -> Response {
    refuse(
        StatusCode::SERVICE_UNAVAILABLE,
        "uploads are not configured: this agent was given no NSED_FILES_BUCKET",
    )
}

/// Walk the multipart body: the file, spooled and hashed, and what visibility
/// it was asked for.
async fn read_form(
    form: &mut Multipart,
    max_upload_bytes: u64,
) -> Result<(Visibility, Spooled), Box<Response>> {
    let mut visibility = Visibility::Public;
    let mut file: Option<Spooled> = None;

    while let Some(field) = form.next_field().await.map_err(|e| {
        Box::new(refuse(
            StatusCode::BAD_REQUEST,
            format!("malformed upload: {e}"),
        ))
    })? {
        match field.name().unwrap_or_default() {
            "visibility" => visibility = read_visibility(field.text().await.unwrap_or_default())?,
            "file" => {
                let filename = field.file_name().unwrap_or("upload").to_string();
                let spooled = spool(field, max_upload_bytes).await?;
                file = Some(Spooled {
                    filename,
                    ..spooled
                });
            }
            // Ignored rather than refused: a browser form carries fields this
            // endpoint has no interest in, and rejecting the whole upload for
            // one of them helps nobody.
            _ => {}
        }
    }

    let file =
        file.ok_or_else(|| refuse(StatusCode::BAD_REQUEST, "no `file` part in the upload"))?;
    Ok((visibility, file))
}

/// Refused rather than defaulted: defaulting would publish a file whose
/// uploader asked for something else, and the mistake is invisible until the
/// link is out.
fn read_visibility(raw: String) -> Result<Visibility, Box<Response>> {
    match raw.trim() {
        "public" => Ok(Visibility::Public),
        "private" => Ok(Visibility::Private),
        other => Err(Box::new(refuse(
            StatusCode::BAD_REQUEST,
            format!("visibility {other:?} is neither public nor private"),
        ))),
    }
}

/// `POST /api/content` — store a file and return the address it can be read at.
///
/// The digest names the object, so it has to be known before the store is
/// written to, and a video cannot be held in memory to hash it. The body is
/// therefore spooled to a temporary file while being hashed, and the spool is
/// what gets streamed into the bucket.
pub(super) async fn upload(State(state): State<MultiAppState>, mut form: Multipart) -> Response {
    let Some(content) = state.content.clone() else {
        return not_configured();
    };

    let (visibility, mut spooled) = match read_form(&mut form, content.max_upload_bytes).await {
        Ok(parts) => parts,
        Err(refusal) => return *refusal,
    };

    let Some(mime) = sniff_mime(&spooled.head) else {
        return refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "this is not a media type this deployment serves",
        );
    };

    let meta = ObjectMeta {
        digest: spooled.digest.clone(),
        filename: spooled.filename.clone(),
        mime: mime.to_string(),
        bytes: spooled.bytes,
        visibility,
        uploaded_by: content.uploaded_by.clone(),
        created_at: stamped_now(),
    };

    if let Err(e) = spooled.file.rewind().await {
        return refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not re-read the upload: {e}"),
        );
    }

    match content
        .blob
        .put_stream(&spooled.digest, &mut spooled.file, &meta)
        .await
    {
        Ok(stored) => {
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
        Err(e) => match e.downcast_ref::<QuotaExceeded>() {
            // 507 with the numbers, not a 500 with a string: the caller can
            // act on "you are out of space", and cannot act on "something
            // went wrong".
            Some(full) => (
                StatusCode::INSUFFICIENT_STORAGE,
                Json(Refusal {
                    error: full.to_string(),
                    used_bytes: Some(full.used_bytes),
                    quota_bytes: Some(full.quota_bytes),
                }),
            )
                .into_response(),
            None => refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not store the upload: {e:#}"),
            ),
        },
    }
}

/// Where segmenting is recorded against the original.
const HLS_NOTE: &str = "hls";
/// Where the produced playlist's digest is recorded against the original.
const PLAYLIST_NOTE: &str = "playlist";

/// Kick off segmenting for a video, and say what state that leaves it in.
///
/// Returns without waiting: a transcode takes far longer than an HTTP request
/// should, and the original is already stored and servable. The uploader polls
/// for the playlist.
fn start_segmenting(
    content: &ContentUploads,
    stored: &ObjectMeta,
    spool: Spooled,
) -> crate::hls::HlsState {
    let (Some(hls), Some(public_base)) = (content.hls.clone(), content.public_base.clone()) else {
        // Nothing to segment with, or nowhere to point a playlist at: a
        // playlist of relative names nobody can resolve is worse than none.
        return crate::hls::HlsState::Skipped;
    };
    if !stored.mime.starts_with("video/") {
        return crate::hls::HlsState::Skipped;
    }

    // Taken before the spool is moved into a task, so a backlog is refused
    // rather than accumulating temporary files.
    let Ok(slot) = hls.waiting.clone().try_acquire_owned() else {
        tracing::warn!(
            digest = %stored.digest,
            "the segmenting queue is full, so this upload is stored whole"
        );
        return crate::hls::HlsState::Skipped;
    };

    let blob = content.blob.clone();
    let uploaded_by = content.uploaded_by.clone();
    let digest = stored.digest.clone();
    tokio::spawn(async move {
        let _slot = slot;
        // The spool is moved in and dropped here, at the end of the transcode,
        // rather than when the request returned — ffmpeg reads it.
        let spool = spool;
        if let Err(e) = blob.annotate(&digest, HLS_NOTE, "pending").await {
            tracing::warn!(digest, error = %format!("{e:#}"), "could not mark an upload pending");
        }

        let _turn = hls.running.acquire().await;
        let outcome = crate::hls::segment_and_store(
            blob.as_ref(),
            &hls.transcoder,
            spool.path(),
            &public_base,
            &uploaded_by,
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

    crate::hls::HlsState::Pending
}

/// An upload on disk, hashed on the way there.
struct Spooled {
    file: tokio::fs::File,
    /// Kept alive: dropping the handle deletes the file, and the spool has to
    /// outlive both the read that streams it into the bucket and any transcode
    /// that follows.
    guard: tempfile::TempPath,
    digest: String,
    bytes: u64,
    /// The first bytes, for deciding the media type without re-reading.
    head: Vec<u8>,
    filename: String,
}

impl Spooled {
    fn path(&self) -> &std::path::Path {
        &self.guard
    }
}

/// Write a field to a temporary file, hashing as it lands and stopping at the
/// ceiling.
///
/// The cap is enforced while reading, not after: reading a whole body to
/// discover it was too large is the denial of service the cap exists to
/// prevent.
async fn spool(
    mut field: axum::extract::multipart::Field<'_>,
    max: u64,
) -> Result<Spooled, Box<Response>> {
    let spool = tempfile::NamedTempFile::new().map_err(|e| {
        refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("no spool file: {e}"),
        )
    })?;
    let path = spool.into_temp_path();
    // Read *and* write: the same handle is rewound and streamed into the
    // bucket once the digest is known, and a write-only handle fails there
    // rather than here.
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .await
        .map_err(|e| {
            refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("no spool file: {e}"),
            )
        })?;

    let mut hasher = Sha256::new();
    let mut bytes: u64 = 0;
    let mut head = Vec::with_capacity(SNIFF_BYTES);

    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| boxed(StatusCode::BAD_REQUEST, format!("upload interrupted: {e}")))?
    {
        bytes += chunk.len() as u64;
        if bytes > max {
            return Err(boxed(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("upload exceeds the {max} byte limit"),
            ));
        }
        if head.len() < SNIFF_BYTES {
            let wanted = SNIFF_BYTES - head.len();
            head.extend_from_slice(&chunk[..wanted.min(chunk.len())]);
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| {
            boxed(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not spool the upload: {e}"),
            )
        })?;
    }
    file.flush().await.ok();

    Ok(Spooled {
        file,
        guard: path,
        digest: hex::encode(hasher.finalize()),
        bytes,
        head,
        filename: String::new(),
    })
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
    hls: crate::hls::HlsState,
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
    let Ok(meta) = content.blob.head(&digest).await else {
        return refuse(StatusCode::NOT_FOUND, "no such object");
    };
    let notes = content.blob.notes(&digest).await.unwrap_or_default();

    let hls = match notes.get(HLS_NOTE).map(String::as_str) {
        Some("pending") => crate::hls::HlsState::Pending,
        Some("ready") => crate::hls::HlsState::Ready,
        Some("failed") => crate::hls::HlsState::Failed,
        _ => crate::hls::HlsState::Skipped,
    };
    // Only once it is ready. A playlist note can outlive the segmentation that
    // wrote it — a later re-run that failed, say — and handing out a URL for a
    // playlist whose segments are gone fails in the player rather than here.
    let playlist_url = (hls == crate::hls::HlsState::Ready)
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
    match content.blob.delete(&digest).await {
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

/// Magic numbers that identify a type outright, longest first so a shorter
/// prefix cannot claim a file a longer one would have matched.
const MAGIC: [(&[u8], &str); 8] = [
    (b"\x89PNG\r\n\x1a\n", "image/png"),
    (b"GIF87a", "image/gif"),
    (b"GIF89a", "image/gif"),
    (b"%PDF-", "application/pdf"),
    (b"\x1a\x45\xdf\xa3", "video/webm"),
    (b"OggS", "audio/ogg"),
    (b"\xff\xd8\xff", "image/jpeg"),
    (b"ID3", "audio/mpeg"),
];

/// The media types an upload may be.
///
/// An allowlist, not a blocklist: the type is stored and later echoed back as
/// `Content-Type` by a public endpoint, so anything unrecognised here would be
/// a type this deployment never decided it was willing to serve.
fn sniff_mime(head: &[u8]) -> Option<&'static str> {
    if let Some((_, mime)) = MAGIC
        .iter()
        .find(|(magic, _)| head.len() >= magic.len() && &head[..magic.len()] == *magic)
    {
        return Some(mime);
    }
    sniff_container(head).or_else(|| sniff_mpeg_frame(head))
}

/// Formats whose first four bytes name a container, not a type.
///
/// RIFF and ISO base media both need a second look: `RIFF` alone would serve a
/// wav as a webp, and `ftyp` alone cannot tell mp4 from quicktime.
fn sniff_container(head: &[u8]) -> Option<&'static str> {
    if head.len() < 12 {
        return None;
    }
    if &head[..4] == b"RIFF" {
        return match &head[8..12] {
            b"WEBP" => Some("image/webp"),
            b"WAVE" => Some("audio/wav"),
            _ => None,
        };
    }
    if &head[4..8] == b"ftyp" {
        return match &head[8..11] {
            b"qt " => Some("video/quicktime"),
            _ => Some("video/mp4"),
        };
    }
    None
}

/// A bare MPEG audio frame, which carries no magic string — only a sync word.
fn sniff_mpeg_frame(head: &[u8]) -> Option<&'static str> {
    (head.len() >= 2 && head[0] == 0xff && (head[1] & 0xe0) == 0xe0).then_some("audio/mpeg")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::content_blob::{Usage, open_blob};
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
    async fn configured(max_upload_bytes: u64, quota: i64) -> Option<ContentUploads> {
        let js = context().await?;
        let bucket = format!("test_upload_{}", uuid::Uuid::new_v4().simple());
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
            // No transcoder in the tests that exercise the HTTP surface: the
            // pipeline has its own, and spawning one here would make every
            // upload test wait on a subprocess.
            hls: None,
        })
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
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
            require_broker("uploading needs a broker");
            return;
        };

        let video = fake_mp4(300_000);
        let response = send(
            Some(content.clone()),
            upload_request(multipart("clip.mp4", &video, None)),
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
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
            require_broker("uploading needs a broker");
            return;
        };
        let uploaded = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("clip.mp4", &fake_mp4(2_000), None)),
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
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
            require_broker("uploading needs a broker");
            return;
        };
        let png = b"\x89PNG\r\n\x1a\n and some pixels".to_vec();
        let uploaded = body_json(
            send(
                Some(content),
                upload_request(multipart("shot.png", &png, None)),
            )
            .await,
        )
        .await;
        assert_eq!(uploaded["mime"], "image/png");
        assert_eq!(uploaded["hls"], "skipped");
    }

    #[tokio::test]
    async fn status_reports_what_segmenting_recorded() {
        // The uploader's poll: segmenting outlives the request that started
        // it, so this is how they learn the video became seekable.
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
            require_broker("status needs a broker");
            return;
        };
        let uploaded = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("clip.mp4", &fake_mp4(2_000), None)),
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
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
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
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
            require_broker("range refusal needs a broker");
            return;
        };
        let video = fake_mp4(2_000);
        let uploaded = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("clip.mp4", &video, None)),
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
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
            require_broker("type refusal needs a broker");
            return;
        };
        let response = send(
            Some(content),
            upload_request(multipart("payload.html", b"<!DOCTYPE html><script>", None)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn an_upload_past_the_ceiling_is_refused_while_it_streams() {
        let Some(content) = configured(64 * 1024, 4 * 1024 * 1024).await else {
            require_broker("the size cap needs a broker");
            return;
        };
        let response = send(
            Some(content),
            upload_request(multipart("big.mp4", &fake_mp4(200_000), None)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_full_bucket_answers_with_the_numbers_not_a_five_hundred() {
        // A caller can act on "you are out of space" and cannot act on
        // "something went wrong".
        let Some(content) = configured(8 * 1024 * 1024, 64 * 1024).await else {
            require_broker("quota refusal needs a broker");
            return;
        };
        let first = send(
            Some(content.clone()),
            upload_request(multipart("a.mp4", &fake_mp4(40_000), None)),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);

        let mut second_bytes = fake_mp4(40_000);
        second_bytes[100] = 0x01; // different content, so a different digest
        let response = send(
            Some(content),
            upload_request(multipart("b.mp4", &second_bytes, None)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);
        let refusal = body_json(response).await;
        assert_eq!(refusal["quota_bytes"], 64 * 1024);
        assert!(refusal["used_bytes"].is_number());
    }

    #[tokio::test]
    async fn visibility_is_recorded_as_asked_for() {
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
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
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
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
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
            require_broker("deleting needs a broker");
            return;
        };
        let uploaded = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("clip.mp4", &fake_mp4(50_000), None)),
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
            upload_request(multipart("clip.mp4", &fake_mp4(100), None)),
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
    async fn an_upload_with_no_file_part_is_a_bad_request() {
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
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
        let Some(content) = configured(8 * 1024 * 1024, 4 * 1024 * 1024).await else {
            require_broker("idempotent upload needs a broker");
            return;
        };
        let video = fake_mp4(20_000);
        let first = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("clip.mp4", &video, None)),
            )
            .await,
        )
        .await;
        let second = body_json(
            send(
                Some(content.clone()),
                upload_request(multipart("same-again.mp4", &video, None)),
            )
            .await,
        )
        .await;
        assert_eq!(first["digest"], second["digest"], "the digest is the name");
        assert_eq!(content.blob.list().await.expect("list").len(), 1);
    }

    #[test]
    fn sniffing_recognises_the_types_this_deployment_serves() {
        assert_eq!(sniff_mime(b"\x89PNG\r\n\x1a\n....."), Some("image/png"));
        assert_eq!(sniff_mime(b"\xff\xd8\xff\xe0JFIF"), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a......"), Some("image/gif"));
        assert_eq!(sniff_mime(b"RIFF____WEBPVP8 "), Some("image/webp"));
        assert_eq!(
            sniff_mime(b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00"),
            Some("video/mp4")
        );
        assert_eq!(
            sniff_mime(b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00"),
            Some("video/mp4")
        );
        assert_eq!(
            sniff_mime(b"\x00\x00\x00\x1cftypqt  \x00\x00\x02\x00"),
            Some("video/quicktime")
        );
        assert_eq!(sniff_mime(b"\x1a\x45\xdf\xa3........"), Some("video/webm"));
        assert_eq!(sniff_mime(b"OggS\x00\x02\x00\x00"), Some("audio/ogg"));
        assert_eq!(sniff_mime(b"ID3\x03\x00\x00\x00"), Some("audio/mpeg"));
        assert_eq!(sniff_mime(b"RIFF____WAVEfmt "), Some("audio/wav"));
        assert_eq!(sniff_mime(b"%PDF-1.7\n"), Some("application/pdf"));
    }

    #[test]
    fn an_unrecognised_type_is_refused_rather_than_guessed() {
        // The stored type is echoed back as `Content-Type` by a public
        // endpoint, so "probably fine" is not a decision this can make.
        assert_eq!(sniff_mime(b"MZ\x90\x00"), None, "an executable");
        assert_eq!(sniff_mime(b"\x7fELF\x02\x01\x01"), None, "an executable");
        assert_eq!(sniff_mime(b"PK\x03\x04"), None, "a zip");
        assert_eq!(sniff_mime(b"<svg xmlns=\"http"), None, "svg is script");
        assert_eq!(sniff_mime(b"<!DOCTYPE html>"), None, "html is script");
        assert_eq!(sniff_mime(b""), None, "nothing at all");
        assert_eq!(sniff_mime(b"\x89PN"), None, "a truncated magic number");
    }

    #[test]
    fn riff_containers_are_told_apart_by_their_form_type() {
        // Both start `RIFF`. Reading only the first four bytes would serve a
        // wav as a webp.
        assert_eq!(sniff_mime(b"RIFF____WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_mime(b"RIFF____WAVEfmt "), Some("audio/wav"));
        assert_eq!(sniff_mime(b"RIFF____AVI LIST"), None, "avi is not served");
        assert_eq!(sniff_mime(b"RIFF___"), None, "too short to tell");
    }
}
