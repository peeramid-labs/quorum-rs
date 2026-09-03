//! A named byte store with a size ceiling, and the one trait the serving
//! layers are written against.
//!
//! Uploaded files differ from deliberation content in three ways: they carry a
//! MIME type that has to survive all the way to an HTTP response, a visibility
//! flag, and a byte ceiling per owner.
//!
//! This module holds the *mechanism* only — how named bytes and their metadata
//! are stored, read back and bounded. What a bucket is called and who is
//! entitled to how much are deployment policy and live with the side that owns
//! storage, the same split [`crate::nats_utils::put_content_addressed`]
//! already follows.
//!
//! The metadata keys written here are a contract: whoever uploads and whoever
//! serves must agree on them, and they are on opposite sides of a process
//! boundary. That is why this is in the SDK rather than beside either one.
//!
//! [`Blob`] exists so the substrate can change without the HTTP layers
//! changing with it. [`Blob::get_range`] is in the signature even though the
//! NATS implementation reaches an offset by skipping forward: writing the
//! serving side against whole-object reads is what would have to be undone
//! when object storage that ranges natively replaces this one.

use anyhow::{Context as _, Result};
use async_nats::jetstream::{self, object_store, object_store::ObjectStore};
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::io::AsyncRead;

/// How many part-read objects one blob keeps open.
///
/// Bounds file handles and broker subscriptions on a store that many viewers
/// read at once. Evicting costs one reopen, never an error.
const MAX_OPEN_CURSORS: usize = 32;

/// A JetStream byte ceiling as a quota.
///
/// JetStream spells "no ceiling" as a non-positive `max_bytes`, `-1` being the
/// sentinel it stores for unlimited. Clamping that to zero would read an
/// unlimited bucket as a full one and refuse every upload, so it becomes the
/// largest quota instead — which is what the bucket actually enforces.
pub fn quota_from_max_bytes(max_bytes: i64) -> u64 {
    if max_bytes <= 0 {
        u64::MAX
    } else {
        max_bytes as u64
    }
}

/// Who may read an object.
///
/// Only [`Visibility::Public`] is servable today. The variant exists now
/// because visibility is recorded per object at upload time: adding it later
/// would leave every already-stored object with no answer to the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Servable by anyone holding the address.
    Public,
    /// Not servable by the public endpoint. No authenticated read path exists
    /// yet, so this currently means "reachable only by the operator's own
    /// agents".
    Private,
}

impl Visibility {
    fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
        }
    }

    /// Anything not recognised reads as private: an object whose visibility
    /// cannot be established must not be served.
    fn from_str(raw: &str) -> Self {
        if raw == "public" {
            Self::Public
        } else {
            Self::Private
        }
    }
}

/// Where an object's annotations live.
///
/// A separate object rather than the store's own metadata map, because
/// `update_metadata` in `async-nats` 0.47 writes back only the name and
/// description and drops the map — an annotation made that way is accepted
/// and then silently absent. A sidecar also keeps annotations off the serving
/// path: reading a file's type and size stays one round trip whether or not
/// anything has annotated it.
const NOTES_PREFIX: &str = "notes.";

/// What is known about a stored object without reading it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ObjectMeta {
    /// SHA-256 hex of the content. Also the object's name.
    pub digest: String,
    /// The name the uploader gave it, for `Content-Disposition`. Never used to
    /// address the object.
    pub filename: String,
    pub mime: String,
    pub bytes: u64,
    pub visibility: Visibility,
    /// Identity that uploaded it, for attribution and later revocation.
    pub uploaded_by: String,
    /// RFC 3339. Written by the store, not the caller.
    pub created_at: String,
}

/// Now, as the `created_at` an [`ObjectMeta`] carries.
///
/// Callers build `ObjectMeta` as a struct literal rather than through a
/// constructor: it has two `String` fields that mean different things
/// (`filename` and `mime`) and swapping them positionally would compile.
pub fn stamped_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl ObjectMeta {
    fn to_map(&self) -> HashMap<String, String> {
        HashMap::from([
            ("filename".to_string(), self.filename.clone()),
            ("mime".to_string(), self.mime.clone()),
            (
                "visibility".to_string(),
                self.visibility.as_str().to_string(),
            ),
            ("uploaded_by".to_string(), self.uploaded_by.clone()),
            ("created_at".to_string(), self.created_at.clone()),
        ])
    }

    /// Rebuild from what the store kept. Missing fields fall back rather than
    /// failing: an object written by an older build is still servable, and the
    /// fallback for visibility is the closed one.
    fn from_info(info: &object_store::ObjectInfo) -> Self {
        let field = |key: &str| info.metadata.get(key).cloned().unwrap_or_default();
        Self {
            digest: info.name.clone(),
            filename: field("filename"),
            mime: info
                .metadata
                .get("mime")
                .cloned()
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            bytes: info.size as u64,
            visibility: Visibility::from_str(&field("visibility")),
            uploaded_by: field("uploaded_by"),
            created_at: field("created_at"),
        }
    }
}

/// How much of the ceiling is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub used_bytes: u64,
    pub quota_bytes: u64,
}

impl Usage {
    /// Whether `bytes` more would fit.
    ///
    /// Checked rather than saturating: saturation clamps an overflowing total
    /// down to `u64::MAX`, which compares as fitting against a large quota and
    /// turns an impossible request into an accepted one.
    pub fn admits(&self, bytes: u64) -> bool {
        self.used_bytes
            .checked_add(bytes)
            .is_some_and(|total| total <= self.quota_bytes)
    }
}

/// A put refused because the operator's space is full.
///
/// A distinct type rather than a message, because the HTTP layer has to answer
/// `507` with the numbers rather than `500` with a string, and matching on
/// broker error text to decide that would break the first time the broker
/// rewords it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaExceeded {
    pub used_bytes: u64,
    pub quota_bytes: u64,
    pub requested_bytes: u64,
}

impl std::fmt::Display for QuotaExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "storage quota exceeded: {} of {} bytes used, {} more requested",
            self.used_bytes, self.quota_bytes, self.requested_bytes
        )
    }
}

impl std::error::Error for QuotaExceeded {}

/// Store and serve named bytes.
#[async_trait]
pub trait Blob: Send + Sync {
    /// Store `reader` under `digest`, which is the content's own hash and
    /// therefore makes storing the same bytes twice idempotent.
    ///
    /// Refuses with [`QuotaExceeded`] rather than filling the space.
    async fn put_stream(
        &self,
        digest: &str,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        meta: &ObjectMeta,
    ) -> Result<ObjectMeta>;

    /// `(window, total_size)`. A `len` running past the end yields a short
    /// window, not an error; an `offset` past the end is a caller bug.
    async fn get_range(&self, digest: &str, offset: u64, len: u64) -> Result<(Vec<u8>, u64)>;

    /// Metadata without the bytes.
    async fn head(&self, digest: &str) -> Result<ObjectMeta>;

    /// Record `value` against `key` on an object already stored.
    ///
    /// Additive: the object's own fields and its other annotations are left
    /// as they are.
    async fn annotate(&self, digest: &str, key: &str, value: &str) -> Result<()>;

    /// Everything annotated against `digest`. Empty when nothing has been.
    async fn notes(&self, digest: &str) -> Result<HashMap<String, String>>;

    async fn delete(&self, digest: &str) -> Result<()>;

    async fn usage(&self) -> Result<Usage>;

    async fn list(&self) -> Result<Vec<ObjectMeta>>;
}

/// [`Blob`] over one NATS object-store bucket.
///
/// The bucket's `max_bytes` is the quota, which is why exhaustion is
/// fail-closed for free: object-store streams are created `discard: New` with
/// `deny_delete`, so a put past the ceiling is refused and nothing already
/// stored is evicted to make room.
pub struct NatsBlob {
    /// Kept alongside the store because the bucket's byte count lives on the
    /// backing stream, and `ObjectStore` does not expose it.
    js: jetstream::Context,
    bucket: String,
    store: ObjectStore,
    quota_bytes: u64,
    /// Where part-read objects left off, so a paged fetch continues one open
    /// stream instead of re-skipping the prefix per window — which costs bytes
    /// quadratic in the object's size.
    ///
    /// A pool rather than a single slot: one blob serves every reader of an
    /// operator's files, and two viewers streaming at once would otherwise
    /// evict each other's position on every window and drive both back to the
    /// quadratic path.
    cursors: tokio::sync::Mutex<CursorPool<object_store::Object>>,
    /// Serialises the read-modify-write in [`Blob::annotate`].
    ///
    /// One lock for the whole blob rather than one per object: annotations are
    /// a handful per upload, so contention is not a concern, and two callers
    /// annotating the same digest at once would otherwise each write back a
    /// map missing the other's note. Two uploads of identical bytes share a
    /// digest, so that is reachable rather than theoretical.
    annotating: tokio::sync::Mutex<()>,
}

/// Part-read objects, keyed by where each one stopped.
///
/// Generic over the object so the bookkeeping — which is where the bound and
/// the matching live — can be exercised without a broker.
struct CursorPool<V> {
    open: std::collections::VecDeque<(String, u64, V)>,
    capacity: usize,
}

impl<V> CursorPool<V> {
    fn new(capacity: usize) -> Self {
        Self {
            open: std::collections::VecDeque::new(),
            capacity,
        }
    }

    /// The object left at exactly `(digest, position)`, removed from the pool.
    ///
    /// Exactly: a cursor is a position in a stream that cannot seek, so a
    /// reader asking for anything else has to open fresh.
    fn take(&mut self, digest: &str, position: u64) -> Option<V> {
        let at = self
            .open
            .iter()
            .position(|(held, stopped, _)| held == digest && *stopped == position)?;
        self.open.remove(at).map(|(_, _, object)| object)
    }

    /// Hand an object back for the next window, evicting the least recently
    /// returned when the pool is full.
    fn put(&mut self, digest: String, position: u64, object: V) {
        self.open.push_back((digest, position, object));
        while self.open.len() > self.capacity {
            self.open.pop_front();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.open.len()
    }
}

impl NatsBlob {
    pub fn new(
        js: jetstream::Context,
        bucket: impl Into<String>,
        store: ObjectStore,
        quota_bytes: u64,
    ) -> Self {
        Self {
            js,
            bucket: bucket.into(),
            store,
            quota_bytes,
            cursors: tokio::sync::Mutex::new(CursorPool::new(MAX_OPEN_CURSORS)),
            annotating: tokio::sync::Mutex::new(()),
        }
    }

    /// The bucket this serves, for callers that need to name it.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }
}

#[async_trait]
impl Blob for NatsBlob {
    async fn put_stream(
        &self,
        digest: &str,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        meta: &ObjectMeta,
    ) -> Result<ObjectMeta> {
        // Checked before writing rather than caught after. The broker would
        // refuse the put anyway, but only once the chunks are already on the
        // wire — and a part-written object leaves chunks behind that nothing
        // names and no delete reaches. Two concurrent puts can still both pass
        // this and let the broker refuse the loser; that race is rare and the
        // broker's refusal is the backstop, not the plan.
        let usage = self.usage().await?;
        if !usage.admits(meta.bytes) {
            return Err(anyhow::Error::new(QuotaExceeded {
                used_bytes: usage.used_bytes,
                quota_bytes: usage.quota_bytes,
                requested_bytes: meta.bytes,
            }));
        }

        let stored = ObjectMeta {
            digest: digest.to_string(),
            ..meta.clone()
        };
        let object_meta = object_store::ObjectMetadata {
            name: digest.to_string(),
            metadata: stored.to_map(),
            ..Default::default()
        };

        // `put` wants a sized reader; `&mut &mut dyn AsyncRead` is sized and
        // reads through to the caller's stream.
        let mut reader = reader;
        let written = self
            .store
            .put(object_meta, &mut reader)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context(format!("store object {digest}"))?;

        Ok(ObjectMeta {
            bytes: written.size as u64,
            ..stored
        })
    }

    async fn get_range(&self, digest: &str, offset: u64, len: u64) -> Result<(Vec<u8>, u64)> {
        use tokio::io::AsyncReadExt as _;

        // Taken and released before anything is awaited on the broker: a match
        // scrutinee's temporaries live to the end of the match, so holding the
        // guard here would serialise every read in the bucket behind one
        // reader's offset skip — the opposite of what the pool is for.
        let resumed = {
            let mut cursors = self.cursors.lock().await;
            cursors.take(digest, offset)
        };
        let mut object = match resumed {
            Some(open) => open,
            None => {
                let mut fresh = self
                    .store
                    .get(digest)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .context(format!("read object {digest}"))?;
                let total = fresh.info().size as u64;
                if offset > total {
                    anyhow::bail!("offset {offset} is past the end of {total} bytes");
                }
                tokio::io::copy(&mut (&mut fresh).take(offset), &mut tokio::io::sink())
                    .await
                    .context("skip to the requested offset")?;
                fresh
            }
        };
        let total = object.info().size as u64;

        let mut window = Vec::new();
        (&mut object)
            .take(len)
            .read_to_end(&mut window)
            .await
            .context("read the window")?;

        let after = offset + window.len() as u64;
        if after < total {
            self.cursors
                .lock()
                .await
                .put(digest.to_string(), after, object);
        }

        Ok((window, total))
    }

    async fn head(&self, digest: &str) -> Result<ObjectMeta> {
        let stored = self
            .store
            .info(digest)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context(format!("read metadata for {digest}"))?;
        // A delete leaves a tombstone that `info` still returns. Serving from
        // it would answer with the metadata of bytes that are no longer there.
        if stored.deleted {
            anyhow::bail!("object {digest} was deleted");
        }
        Ok(ObjectMeta::from_info(&stored))
    }

    async fn annotate(&self, digest: &str, key: &str, value: &str) -> Result<()> {
        let _serialised = self.annotating.lock().await;
        let mut notes = self.notes(digest).await?;
        notes.insert(key.to_string(), value.to_string());
        let encoded = serde_json::to_vec(&notes).context("encode annotations")?;
        let mut reader = encoded.as_slice();
        self.store
            .put(
                object_store::ObjectMetadata {
                    name: format!("{NOTES_PREFIX}{digest}"),
                    ..Default::default()
                },
                &mut reader,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context(format!("annotate {digest}"))?;
        Ok(())
    }

    async fn notes(&self, digest: &str) -> Result<HashMap<String, String>> {
        use tokio::io::AsyncReadExt as _;

        // Absent is empty, not an error: most objects are never annotated.
        let Ok(mut object) = self.store.get(format!("{NOTES_PREFIX}{digest}")).await else {
            return Ok(HashMap::new());
        };
        let mut raw = Vec::new();
        object
            .read_to_end(&mut raw)
            .await
            .context("read annotations")?;
        serde_json::from_slice(&raw).context("decode annotations")
    }

    async fn delete(&self, digest: &str) -> Result<()> {
        // The sidecar goes with it, or it would outlive what it describes and
        // reattach itself to the next object stored under the same digest.
        let _ = self.store.delete(format!("{NOTES_PREFIX}{digest}")).await;
        self.store
            .delete(digest)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context(format!("delete object {digest}"))
    }

    async fn usage(&self) -> Result<Usage> {
        let used_bytes = self
            .js
            .get_stream(format!("OBJ_{}", self.bucket))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("open bucket stream")?
            .info()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("read bucket usage")?
            .state
            .bytes as u64;
        Ok(Usage {
            used_bytes,
            quota_bytes: self.quota_bytes,
        })
    }

    async fn list(&self) -> Result<Vec<ObjectMeta>> {
        use futures::TryStreamExt as _;

        let listing = self
            .store
            .list()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("list objects")?;
        let infos: Vec<_> = listing
            .try_collect()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("collect object listing")?;
        Ok(infos
            .iter()
            .filter(|info| !info.deleted && !info.name.starts_with(NOTES_PREFIX))
            .map(ObjectMeta::from_info)
            .collect())
    }
}

/// Open a [`NatsBlob`] over an existing bucket, at the ceiling that bucket
/// already has.
///
/// The ceiling comes from the bucket rather than the caller: the broker is
/// what enforces it, and a caller guessing a different number would pre-check
/// against a limit that does not exist.
pub async fn open_blob(js: &jetstream::Context, bucket: &str) -> Result<NatsBlob> {
    let store = js
        .get_object_store(bucket)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context(format!("open object bucket {bucket}"))?;
    let quota_bytes = js
        .get_stream(format!("OBJ_{bucket}"))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("open the bucket's stream")?
        .info()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("read the bucket's ceiling")?
        .config
        .max_bytes;
    Ok(NatsBlob::new(
        js.clone(),
        bucket,
        store,
        quota_from_max_bytes(quota_bytes),
    ))
}

/// `Range: bytes=a-b` against a known total, as an inclusive `(first, last)`.
///
/// `None` when the header is absent-shaped, malformed, or unsatisfiable — the
/// caller answers 200 or 416 accordingly.
pub fn parse_byte_range(header: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = header.strip_prefix("bytes=")?.trim();
    // Only a single range is served. Reading the first part of a multi-range
    // request and answering 206 would claim to have satisfied all of it.
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let last_byte = total - 1;

    match (first.trim(), last.trim()) {
        ("", suffix) => suffix_range(suffix, total),
        (start, "") => open_range(start, last_byte),
        (start, end) => closed_range(start, end, last_byte),
    }
}

/// `-N`: the final N bytes.
fn suffix_range(suffix: &str, total: u64) -> Option<(u64, u64)> {
    let len: u64 = suffix.parse().ok()?;
    (len > 0).then(|| (total.saturating_sub(len), total - 1))
}

/// `N-`: from N to the end.
fn open_range(start: &str, last_byte: u64) -> Option<(u64, u64)> {
    let start: u64 = start.parse().ok()?;
    (start <= last_byte).then_some((start, last_byte))
}

/// `A-B`.
///
/// An end past the last byte is clamped rather than refused: the range is
/// still satisfiable, just shorter than asked for.
fn closed_range(start: &str, end: &str, last_byte: u64) -> Option<(u64, u64)> {
    let start: u64 = start.parse().ok()?;
    let end: u64 = end.parse().ok()?;
    (start <= end && start <= last_byte).then_some((start, end.min(last_byte)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nats_utils::sha256_hex_bytes;

    #[test]
    fn a_range_is_read_as_an_inclusive_pair() {
        assert_eq!(parse_byte_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_byte_range("bytes=100-", 1000), Some((100, 999)));
        assert_eq!(
            parse_byte_range("bytes=-100", 1000),
            Some((900, 999)),
            "a suffix range counts back from the end"
        );
        assert_eq!(
            parse_byte_range("bytes=0-", 1000),
            Some((0, 999)),
            "an open range is the whole object"
        );
        assert_eq!(
            parse_byte_range("bytes=990-2000", 1000),
            Some((990, 999)),
            "an end past the last byte is clamped, not refused"
        );
        assert_eq!(parse_byte_range("bytes=999-999", 1000), Some((999, 999)));
    }

    #[test]
    fn an_unsatisfiable_or_malformed_range_yields_nothing() {
        assert_eq!(
            parse_byte_range("bytes=1000-", 1000),
            None,
            "starts past the end"
        );
        assert_eq!(parse_byte_range("bytes=500-400", 1000), None, "backwards");
        assert_eq!(parse_byte_range("items=0-99", 1000), None, "not bytes");
        assert_eq!(parse_byte_range("bytes=abc-def", 1000), None);
        assert_eq!(parse_byte_range("bytes=", 1000), None);
        assert_eq!(parse_byte_range("", 1000), None);
        assert_eq!(parse_byte_range("bytes=0-99", 0), None, "an empty object");
        assert_eq!(
            parse_byte_range("bytes=0-99, 200-299", 1000),
            None,
            "multi-range is not served, and must not be read as its first part"
        );
        assert_eq!(
            parse_byte_range("bytes=-0", 1000),
            None,
            "an empty suffix range"
        );
    }

    #[test]
    fn an_unrecognised_visibility_reads_as_private() {
        // The fallback has to be the closed one: an object whose visibility
        // cannot be established must not be served to the public.
        assert_eq!(Visibility::from_str("public"), Visibility::Public);
        assert_eq!(Visibility::from_str("private"), Visibility::Private);
        assert_eq!(Visibility::from_str(""), Visibility::Private);
        assert_eq!(Visibility::from_str("PUBLIC"), Visibility::Private);
        assert_eq!(Visibility::from_str("public "), Visibility::Private);
    }

    #[test]
    fn usage_admits_up_to_the_ceiling_and_no_further() {
        let usage = Usage {
            used_bytes: 900,
            quota_bytes: 1000,
        };
        assert!(usage.admits(100), "exactly filling the quota is allowed");
        assert!(!usage.admits(101));
        assert!(usage.admits(0));
    }

    #[test]
    fn an_overflowing_request_is_refused_rather_than_clamped() {
        let usage = Usage {
            used_bytes: u64::MAX - 10,
            quota_bytes: u64::MAX,
        };
        assert!(!usage.admits(100));
        assert!(usage.admits(10), "exactly reaching the ceiling still fits");
    }

    #[test]
    fn an_unlimited_bucket_is_not_read_as_a_full_one() {
        // JetStream stores "no ceiling" as -1. Clamping that to zero would
        // make every upload to an unlimited bucket fail the pre-check.
        assert_eq!(quota_from_max_bytes(-1), u64::MAX);
        assert_eq!(quota_from_max_bytes(0), u64::MAX);
        assert_eq!(quota_from_max_bytes(1024), 1024);
        assert!(
            Usage {
                used_bytes: 1 << 40,
                quota_bytes: quota_from_max_bytes(-1),
            }
            .admits(1 << 30)
        );
    }

    #[test]
    fn a_cursor_is_reused_only_at_the_exact_position_it_stopped() {
        // A cursor is a position in a stream that cannot seek, so anything but
        // an exact match has to open fresh — handing back a cursor for a
        // different offset would serve the wrong bytes.
        let mut pool = CursorPool::new(4);
        pool.put("aa".to_string(), 100, 1_u32);
        assert_eq!(pool.take("aa", 99), None, "a rewind must not match");
        assert_eq!(pool.take("bb", 100), None, "another object must not match");
        assert_eq!(pool.take("aa", 100), Some(1));
        assert_eq!(pool.take("aa", 100), None, "taking removes it");
    }

    #[test]
    fn concurrent_readers_each_keep_their_own_place() {
        // One blob serves every reader of a bucket. A single slot would have
        // two viewers evict each other every window and drive both back to
        // re-skipping from byte zero.
        let mut pool = CursorPool::new(4);
        pool.put("aa".to_string(), 512, 1_u32);
        pool.put("bb".to_string(), 1024, 2_u32);
        assert_eq!(pool.take("bb", 1024), Some(2));
        assert_eq!(
            pool.take("aa", 512),
            Some(1),
            "the other reader's place must survive"
        );
    }

    #[test]
    fn the_cursor_pool_is_bounded() {
        let mut pool = CursorPool::new(2);
        for i in 0..5_u32 {
            pool.put(format!("obj-{i}"), 0, i);
        }
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.take("obj-0", 0), None, "the oldest is evicted first");
        assert_eq!(pool.take("obj-4", 0), Some(4), "the newest is kept");
    }

    #[test]
    fn metadata_survives_the_map_round_trip() {
        let meta = public_meta(&"a".repeat(64), 4096);
        let map = meta.to_map();
        assert_eq!(map.get("mime").map(String::as_str), Some("video/mp4"));
        assert_eq!(map.get("visibility").map(String::as_str), Some("public"));
        assert_eq!(map.get("filename").map(String::as_str), Some("clip.mp4"));
        assert!(
            !meta.created_at.is_empty(),
            "the store stamps the time, so it must not be empty"
        );
    }

    /// Test buckets are small on purpose. JetStream *reserves* `max_bytes` for
    /// a file-backed stream, so a suite that provisions at a production
    /// ceiling exhausts the account's store and every later test fails with
    /// "insufficient storage resources available".
    const TEST_QUOTA_BYTES: i64 = 2 * 1024 * 1024;

    fn public_meta(digest: &str, bytes: u64) -> ObjectMeta {
        ObjectMeta {
            digest: digest.to_string(),
            filename: "clip.mp4".to_string(),
            mime: "video/mp4".to_string(),
            bytes,
            visibility: Visibility::Public,
            uploaded_by: "agent-7".to_string(),
            created_at: stamped_now(),
        }
    }

    async fn context() -> Option<jetstream::Context> {
        let url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".into());
        match async_nats::connect(&url).await {
            Ok(client) => Some(jetstream::new(client)),
            Err(e) => {
                eprintln!("connect to {url} failed: {e:?}");
                None
            }
        }
    }

    /// `None` when the broker is absent, so tests skip rather than fail — and
    /// `REQUIRE_NATS=1` turns that skip back into a failure.
    async fn blob_for(tag: &str, quota: i64) -> Option<NatsBlob> {
        let js = context().await?;
        // Named after the test rather than randomised, and deleted first, so
        // the number of test buckets stays bounded by the number of tests
        // instead of growing with every run — JetStream *reserves* a bucket's
        // max_bytes, so leaked buckets exhaust the account's store.
        let bucket = format!("test_blob_{tag}");
        let _ = js.delete_object_store(&bucket).await;
        let store = crate::nats_utils::ensure_object_bucket(
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
        // Surfaced rather than swallowed: an `.ok()` here would turn a broken
        // bucket into a silent skip, which reads as "no broker".
        .expect("open the test bucket");
        Some(NatsBlob::new(
            js,
            bucket,
            store,
            quota_from_max_bytes(quota),
        ))
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

    async fn put(blob: &NatsBlob, content: &[u8]) -> Result<String> {
        let digest = sha256_hex_bytes(content);
        let mut reader = content;
        blob.put_stream(
            &digest,
            &mut reader,
            &public_meta(&digest, content.len() as u64),
        )
        .await?;
        Ok(digest)
    }

    #[tokio::test]
    async fn an_uploaded_object_comes_back_byte_for_byte_with_its_type() -> Result<()> {
        let Some(blob) = blob_for("roundtrip", TEST_QUOTA_BYTES).await else {
            require_broker("blob round-trip needs a broker");
            return Ok(());
        };

        let content: Vec<u8> = (0..9_000_usize).map(|i| (i % 251) as u8).collect();
        let digest = put(&blob, &content).await?;

        let head = blob.head(&digest).await?;
        assert_eq!(head.digest, digest);
        assert_eq!(head.mime, "video/mp4");
        assert_eq!(head.filename, "clip.mp4");
        assert_eq!(head.visibility, Visibility::Public);
        assert_eq!(head.uploaded_by, "agent-7");
        assert_eq!(head.bytes, content.len() as u64);

        let (whole, total) = blob.get_range(&digest, 0, content.len() as u64).await?;
        assert_eq!(total, content.len() as u64);
        assert_eq!(whole, content);
        assert_eq!(
            sha256_hex_bytes(&whole),
            digest,
            "what came back must hash to the name it was asked for"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_range_past_the_end_is_short_and_an_offset_past_it_is_an_error() -> Result<()> {
        let Some(blob) = blob_for("range", TEST_QUOTA_BYTES).await else {
            require_broker("range reads need a broker");
            return Ok(());
        };
        let content: Vec<u8> = (0..5_000_usize).map(|i| (i % 251) as u8).collect();
        let digest = put(&blob, &content).await?;

        let (middle, _) = blob.get_range(&digest, 1_000, 500).await?;
        assert_eq!(middle, &content[1000..1500]);

        let (tail, _) = blob.get_range(&digest, 4_900, 500).await?;
        assert_eq!(tail, &content[4900..], "a short window is not an error");

        assert!(
            blob.get_range(&digest, content.len() as u64 + 1, 10)
                .await
                .is_err(),
            "an offset past the end is a caller bug"
        );
        Ok(())
    }

    #[tokio::test]
    async fn consecutive_windows_reassemble_and_a_rewind_starts_over() -> Result<()> {
        let Some(blob) = blob_for("windows", TEST_QUOTA_BYTES).await else {
            require_broker("windowed reads need a broker");
            return Ok(());
        };
        let content: Vec<u8> = (0..20_000_usize).map(|i| (i % 251) as u8).collect();
        let digest = put(&blob, &content).await?;
        let other = put(&blob, b"a different object entirely").await?;

        let mut rebuilt = Vec::new();
        loop {
            let (window, total) = blob.get_range(&digest, rebuilt.len() as u64, 3_000).await?;
            if window.is_empty() {
                break;
            }
            rebuilt.extend_from_slice(&window);
            if rebuilt.len() as u64 >= total {
                break;
            }
        }
        assert_eq!(rebuilt, content);

        let (rewound, _) = blob.get_range(&digest, 0, 512).await?;
        assert_eq!(rewound, content[..512], "a rewind starts over, correctly");

        let (elsewhere, _) = blob.get_range(&other, 0, 64).await?;
        assert_eq!(elsewhere, b"a different object entirely");

        let (resumed, _) = blob.get_range(&digest, 512, 512).await?;
        assert_eq!(resumed, content[512..1024], "and the first read can go on");
        Ok(())
    }

    #[tokio::test]
    async fn the_quota_refuses_the_put_and_evicts_nothing() -> Result<()> {
        // The whole reason the substrate was chosen: a full bucket rejects new
        // bytes instead of making room by dropping someone's file.
        let quota = 64 * 1024;
        let Some(blob) = blob_for("quota", quota).await else {
            require_broker("quota enforcement needs a broker");
            return Ok(());
        };

        let first = vec![b'a'; 40_000];
        let kept = put(&blob, &first).await?;

        let second = vec![b'b'; 40_000];
        let digest = sha256_hex_bytes(&second);
        let mut reader = second.as_slice();
        let refusal = blob
            .put_stream(
                &digest,
                &mut reader,
                &public_meta(&digest, second.len() as u64),
            )
            .await
            .expect_err("a put past the ceiling must be refused");

        let quota_error = refusal
            .downcast_ref::<QuotaExceeded>()
            .expect("the refusal must be typed, so the HTTP layer can answer 507");
        assert_eq!(quota_error.quota_bytes, quota as u64);
        assert_eq!(quota_error.requested_bytes, second.len() as u64);

        let (survivor, _) = blob.get_range(&kept, 0, first.len() as u64).await?;
        assert_eq!(survivor, first, "nothing already stored may be evicted");
        assert!(
            blob.head(&digest).await.is_err(),
            "the refused put stored nothing"
        );
        Ok(())
    }

    #[tokio::test]
    async fn the_broker_refuses_a_put_the_pre_check_let_through() -> Result<()> {
        // Two concurrent uploads can both pass the pre-check. The bucket's own
        // max_bytes is the backstop, and it must refuse rather than evict —
        // this test is what fails if a bucket is ever created with a discard
        // policy that makes room.
        let Some(js) = context().await else {
            require_broker("the broker backstop needs a broker");
            return Ok(());
        };
        let bucket = "test_blob_backstop".to_string();
        let _ = js.delete_object_store(&bucket).await;
        let store = crate::nats_utils::ensure_object_bucket(
            &js,
            object_store::Config {
                bucket: bucket.clone(),
                max_bytes: 64 * 1024,
                storage: jetstream::stream::StorageType::File,
                num_replicas: 1,
                ..Default::default()
            },
        )
        .await?;
        // Told a ceiling far above the bucket's own, so the pre-check always
        // passes and the broker is the only thing left saying no.
        let blob = NatsBlob::new(js, bucket, store, u64::MAX);

        let first = vec![b'a'; 40_000];
        let kept = put(&blob, &first).await?;

        let second = vec![b'b'; 40_000];
        let digest = sha256_hex_bytes(&second);
        let mut reader = second.as_slice();
        assert!(
            blob.put_stream(
                &digest,
                &mut reader,
                &public_meta(&digest, second.len() as u64)
            )
            .await
            .is_err(),
            "the bucket ceiling must refuse the put"
        );

        let (survivor, _) = blob.get_range(&kept, 0, first.len() as u64).await?;
        assert_eq!(survivor, first, "the backstop must not evict to make room");
        Ok(())
    }

    #[tokio::test]
    async fn usage_tracks_what_is_stored_and_a_delete_frees_it() -> Result<()> {
        let Some(blob) = blob_for("usage", TEST_QUOTA_BYTES).await else {
            require_broker("usage needs a broker");
            return Ok(());
        };

        let empty = blob.usage().await?;
        assert_eq!(empty.quota_bytes, TEST_QUOTA_BYTES as u64);

        let content = vec![b'x'; 30_000];
        let digest = put(&blob, &content).await?;
        let filled = blob.usage().await?;
        assert!(
            filled.used_bytes >= content.len() as u64,
            "usage must count the bytes stored: {filled:?}"
        );

        let listed = blob.list().await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].digest, digest);
        assert_eq!(listed[0].mime, "video/mp4");

        blob.delete(&digest).await?;
        assert!(
            blob.head(&digest).await.is_err(),
            "a deleted object is gone"
        );
        let freed = blob.usage().await?;
        assert!(
            freed.used_bytes < filled.used_bytes,
            "deleting must return the space: {filled:?} -> {freed:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_annotation_survives_and_leaves_the_object_alone() -> Result<()> {
        // A pipeline records its result against an object already written. It
        // must not be able to change what the object *is* while doing so — an
        // annotation named `mime` must not become the media type.
        let Some(blob) = blob_for("annotate", TEST_QUOTA_BYTES).await else {
            require_broker("annotation needs a broker");
            return Ok(());
        };
        let digest = put(&blob, b"a video").await?;

        blob.annotate(&digest, "hls", "ready").await?;
        blob.annotate(&digest, "playlist", &"b".repeat(64)).await?;
        blob.annotate(&digest, "mime", "text/html").await?;

        let notes = blob.notes(&digest).await?;
        assert_eq!(notes.get("hls").map(String::as_str), Some("ready"));
        assert_eq!(
            notes.get("playlist").map(String::as_str),
            Some("b".repeat(64).as_str())
        );

        let meta = blob.head(&digest).await?;
        assert_eq!(
            meta.mime, "video/mp4",
            "an annotation must not overwrite a field the store owns"
        );
        assert_eq!(meta.filename, "clip.mp4");
        assert_eq!(meta.bytes, b"a video".len() as u64);
        assert_eq!(
            blob.list().await?.len(),
            1,
            "the sidecar is not a file of the operator's"
        );

        // Re-annotating replaces that one note and keeps the others.
        blob.annotate(&digest, "hls", "failed").await?;
        let notes = blob.notes(&digest).await?;
        assert_eq!(notes.get("hls").map(String::as_str), Some("failed"));
        assert!(notes.contains_key("playlist"));

        // An object nobody annotated has no notes, and that is not an error.
        assert!(blob.notes(&"c".repeat(64)).await?.is_empty());

        // And the bytes are still readable afterwards.
        let (bytes, _) = blob.get_range(&digest, 0, 64).await?;
        assert_eq!(bytes, b"a video");
        Ok(())
    }

    #[tokio::test]
    async fn storing_the_same_bytes_twice_stores_one_object() -> Result<()> {
        let Some(blob) = blob_for("idempotent", TEST_QUOTA_BYTES).await else {
            require_broker("idempotent put needs a broker");
            return Ok(());
        };
        let content = b"the same bytes".to_vec();
        let first = put(&blob, &content).await?;
        let second = put(&blob, &content).await?;
        assert_eq!(first, second, "the digest is the name");
        assert_eq!(blob.list().await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn open_blob_reads_the_ceiling_from_the_bucket() -> Result<()> {
        // A caller guessing the ceiling would pre-check against a limit the
        // broker is not enforcing.
        let Some(js) = context().await else {
            require_broker("opening a bucket needs a broker");
            return Ok(());
        };
        let bucket = "test_blob_open".to_string();
        let _ = js.delete_object_store(&bucket).await;
        crate::nats_utils::ensure_object_bucket(
            &js,
            object_store::Config {
                bucket: bucket.clone(),
                max_bytes: 128 * 1024,
                storage: jetstream::stream::StorageType::File,
                num_replicas: 1,
                ..Default::default()
            },
        )
        .await?;

        let blob = open_blob(&js, &bucket).await?;
        assert_eq!(blob.usage().await?.quota_bytes, 128 * 1024);
        assert_eq!(blob.bucket(), bucket);

        assert!(
            open_blob(&js, "test_blob_absent_bucket").await.is_err(),
            "a bucket that does not exist is an error, not an empty one"
        );
        Ok(())
    }
}
