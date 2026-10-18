//! The files pipeline: how an upload travels from whoever made it to the
//! store that keeps it.
//!
//! A producer (an agent through the SDK, or the node for a client that
//! uploaded through it) stages the bytes in the shared object store [`INBOX`]
//! and announces them on [`received_subject`]`(owner)`. The node takes the
//! owner from that subject, which each producer's NATS credentials allow only
//! for its own operator, plans the steps the upload goes through, and passes a
//! [`Staged`] message along [`step_subject`]s on the work-queue stream
//! [`STREAM`]. Messages carry ids and metadata, never bytes. The last step
//! commits the upload to the owner's store and deletes what was staged.
//!
//! Staged objects are plaintext today and say so ([`Encryption::None`]); the
//! field is where steps running in an attested enclave learn which key to
//! decrypt with and re-encrypt under. The inbox is shared until per-operator
//! NATS tenancy; a step re-hashes what it reads against the digest, so an
//! object overwritten in the inbox fails instead of landing.

use std::time::Duration;

use anyhow::{Context as _, Result};
use async_nats::jetstream::{self, object_store, stream};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;

use crate::blob::ObjectMeta;

/// The shared object store uploads are staged in.
pub const INBOX: &str = "files_inbox";
/// The work-queue stream the pipeline's messages travel on.
pub const STREAM: &str = "FILES_PIPELINE";
/// How long a staged object may wait before the broker drops it.
pub const INBOX_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// The step that writes an upload into its owner's store; always last.
pub const COMMIT: &str = "commit";

/// Where a producer announces an upload for `owner`.
pub fn received_subject(owner: &str) -> String {
    format!("files.received.{owner}")
}

/// Where the node listens for announced uploads; the owner is the last token.
pub const RECEIVED_ALL: &str = "files.received.*";

/// Where a step's worker takes its uploads from.
pub fn step_subject(step: &str) -> String {
    format!("files.step.{step}")
}

/// How a staged object's bytes are protected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Encryption {
    /// Plaintext: until steps run in enclaves with released keys.
    #[default]
    None,
}

/// One upload on its way through the pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    /// Names the staged object in [`INBOX`] and its derived objects.
    pub upload_id: String,
    /// The principal the upload belongs to, from the subject it arrived on.
    /// Empty in what a producer announces; the node fills it.
    #[serde(default)]
    pub owner: String,
    pub meta: ObjectMeta,
    #[serde(default)]
    pub encryption: Encryption,
    /// The steps still to run, in order; the node plans them, ending in
    /// [`COMMIT`].
    #[serde(default)]
    pub plan: Vec<String>,
    /// Objects steps derived and staged (HLS segments, a playlist), to commit
    /// with the source. Named by [`derived_name`].
    #[serde(default)]
    pub derived: Vec<ObjectMeta>,
    /// Annotations steps recorded, committed onto the source.
    #[serde(default)]
    pub notes: std::collections::BTreeMap<String, String>,
}

impl Staged {
    /// The step to run next, if any.
    pub fn next_step(&self) -> Option<&str> {
        self.plan.first().map(String::as_str)
    }

    /// Pass the upload to its next step on `js`, dropping the one just done.
    pub async fn advance(mut self, js: &jetstream::Context) -> Result<()> {
        if !self.plan.is_empty() {
            self.plan.remove(0);
        }
        let Some(next) = self.next_step().map(str::to_string) else {
            return Ok(());
        };
        publish(js, &step_subject(&next), &self).await
    }
}

/// The inbox name of an object `upload_id` derived, e.g. a segment.
pub fn derived_name(upload_id: &str, name: &str) -> String {
    format!("{upload_id}/{name}")
}

/// Create the inbox and the stream if missing. `inbox_max_bytes` caps
/// everything staged at once, across owners.
pub async fn ensure(js: &jetstream::Context, inbox_max_bytes: i64) -> Result<()> {
    let inbox = object_store::Config {
        bucket: INBOX.to_string(),
        description: Some("Uploads waiting for the files pipeline".to_string()),
        max_age: INBOX_MAX_AGE,
        max_bytes: inbox_max_bytes,
        storage: stream::StorageType::File,
        ..Default::default()
    };
    if js.get_object_store(INBOX).await.is_err() {
        js.create_object_store(inbox)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("create the files inbox")?;
    }
    js.get_or_create_stream(stream::Config {
        name: STREAM.to_string(),
        subjects: vec![RECEIVED_ALL.to_string(), step_subject("*")],
        retention: stream::RetentionPolicy::WorkQueue,
        max_age: INBOX_MAX_AGE,
        storage: stream::StorageType::File,
        ..Default::default()
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
    .context("create the files pipeline stream")?;
    Ok(())
}

/// Stage `reader` as an upload for `owner` and announce it. The upload id.
///
/// `meta.digest` must be the SHA-256 of the bytes; the pipeline checks it
/// before anything lands.
pub async fn stage(
    js: &jetstream::Context,
    owner: &str,
    reader: &mut (dyn AsyncRead + Unpin + Send),
    meta: &ObjectMeta,
) -> Result<String> {
    let upload_id = uuid::Uuid::new_v4().simple().to_string();
    let inbox = js
        .get_object_store(INBOX)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("open the files inbox")?;
    let mut reader = reader;
    inbox
        .put(upload_id.as_str(), &mut reader)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("stage the upload")?;
    let announced = Staged {
        upload_id: upload_id.clone(),
        owner: String::new(),
        meta: meta.clone(),
        encryption: Encryption::None,
        plan: Vec::new(),
        derived: Vec::new(),
        notes: Default::default(),
    };
    publish(js, &received_subject(owner), &announced).await?;
    Ok(upload_id)
}

/// The staged bytes of `staged`, checked as they are read.
pub async fn open(
    js: &jetstream::Context,
    staged: &Staged,
) -> Result<Verified<object_store::Object>> {
    let inbox = js
        .get_object_store(INBOX)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("open the files inbox")?;
    let source = inbox
        .get(staged.upload_id.as_str())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("read staged upload {}", staged.upload_id))?;
    Ok(Verified::new(source, &staged.meta))
}

/// Delete what `staged` holds in the inbox: its source and every derived
/// object. Already-missing objects are not an error, so a retry is safe.
pub async fn discard(js: &jetstream::Context, staged: &Staged) -> Result<()> {
    let inbox = js
        .get_object_store(INBOX)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("open the files inbox")?;
    let derived = staged
        .derived
        .iter()
        .map(|d| derived_name(&staged.upload_id, &d.filename));
    for name in std::iter::once(staged.upload_id.clone()).chain(derived) {
        if !inbox
            .info(name.as_str())
            .await
            .is_ok_and(|info| !info.deleted)
        {
            continue;
        }
        inbox
            .delete(name.as_str())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("discard staged {name}"))?;
    }
    Ok(())
}

/// Reads through to `inner` and, at its end, fails with `InvalidData` unless
/// what passed was exactly `meta.bytes` long and hashed to `meta.digest`. A
/// store writing from it errors before committing, so altered bytes never
/// land under the digest they claim.
pub struct Verified<R> {
    inner: R,
    hasher: sha2::Sha256,
    seen: u64,
    digest: String,
    bytes: u64,
    /// What the end of the stream showed, kept for every later read of it.
    verdict: Option<bool>,
}

impl<R> Verified<R> {
    /// Check `inner` against the length and digest `meta` claims.
    pub fn new(inner: R, meta: &ObjectMeta) -> Self {
        Self {
            inner,
            hasher: sha2::Digest::new(),
            seen: 0,
            digest: meta.digest.clone(),
            bytes: meta.bytes,
            verdict: None,
        }
    }

    fn check(&mut self) -> std::io::Result<()> {
        let digest = match self.verdict {
            Some(true) => return Ok(()),
            Some(false) => "altered".to_string(),
            None => hex::encode(sha2::Digest::finalize(std::mem::take(&mut self.hasher))),
        };
        let matches = self.seen == self.bytes && digest == self.digest;
        self.verdict = Some(matches);
        if matches {
            return Ok(());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "staged bytes ({} bytes, {digest}) are not the {} bytes of {}",
                self.seen, self.bytes, self.digest
            ),
        ))
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Verified<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if !matches!(polled, std::task::Poll::Ready(Ok(()))) {
            return polled;
        }
        let read = &buf.filled()[before..];
        if read.is_empty() && buf.remaining() > 0 {
            return std::task::Poll::Ready(self.check());
        }
        sha2::Digest::update(&mut self.hasher, read);
        self.seen += read.len() as u64;
        std::task::Poll::Ready(Ok(()))
    }
}

async fn publish(js: &jetstream::Context, subject: &str, staged: &Staged) -> Result<()> {
    let payload = serde_json::to_vec(staged).context("encode a staged upload")?;
    js.publish(subject.to_string(), payload.into())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("publish to {subject}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::{Visibility, stamped_now};
    use futures::StreamExt as _;
    use tokio::io::AsyncReadExt as _;

    fn meta(bytes: &[u8]) -> ObjectMeta {
        ObjectMeta {
            digest: crate::sha256_hex_bytes(bytes),
            filename: "talk.mp4".into(),
            mime: "video/mp4".into(),
            bytes: bytes.len() as u64,
            visibility: Visibility::Public,
            uploaded_by: "agent-1".into(),
            created_at: stamped_now(),
        }
    }

    #[test]
    fn subjects_name_the_owner_and_the_step() {
        assert_eq!(received_subject("op_ann"), "files.received.op_ann");
        assert_eq!(step_subject("hls"), "files.step.hls");
        assert_eq!(derived_name("u1", "seg0.m4s"), "u1/seg0.m4s");
    }

    #[test]
    fn a_staged_message_round_trips_and_names_its_next_step() {
        let staged = Staged {
            upload_id: "u1".into(),
            owner: "op_ann".into(),
            meta: meta(b"x"),
            encryption: Encryption::None,
            plan: vec!["hls".into(), COMMIT.into()],
            derived: vec![],
            notes: Default::default(),
        };
        let wire = serde_json::to_value(&staged).unwrap();
        assert_eq!(wire["encryption"], serde_json::json!({"kind": "none"}));
        let back: Staged = serde_json::from_value(wire).unwrap();
        assert_eq!(back, staged);
        assert_eq!(back.next_step(), Some("hls"));
        let bare: Staged =
            serde_json::from_value(serde_json::json!({"upload_id": "u", "meta": meta(b"y")}))
                .unwrap();
        assert!(
            bare.plan.is_empty() && bare.owner.is_empty(),
            "what a producer announces"
        );
    }

    async fn read_all(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await?;
        Ok(out)
    }

    #[tokio::test]
    async fn verified_bytes_read_through_and_altered_ones_fail_at_the_end() {
        let good = meta(b"the bytes");
        assert_eq!(
            read_all(Verified::new(&b"the bytes"[..], &good))
                .await
                .unwrap(),
            b"the bytes"
        );
        let swapped = read_all(Verified::new(&b"the bytez"[..], &good)).await;
        assert_eq!(
            swapped.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
            "same length, other bytes"
        );
        let short = read_all(Verified::new(&b"the"[..], &good)).await;
        assert_eq!(short.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn reading_past_the_end_again_keeps_the_verdict() {
        let mut reader = Verified::new(&b"the bytes"[..], &meta(b"the bytes"));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();
        let mut more = [0u8; 8];
        assert_eq!(reader.read(&mut more).await.unwrap(), 0, "still verified");
        let mut altered = Verified::new(&b"the bytez"[..], &meta(b"the bytes"));
        assert!(altered.read_to_end(&mut out).await.is_err());
        assert!(altered.read(&mut more).await.is_err(), "still altered");
    }

    async fn broker() -> Option<jetstream::Context> {
        let url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".into());
        Some(jetstream::new(async_nats::connect(&url).await.ok()?))
    }

    /// A pull consumer on `subject`, created before anything is published.
    async fn consumer_on(
        js: &jetstream::Context,
        subject: String,
    ) -> Result<jetstream::consumer::Consumer<jetstream::consumer::pull::Config>> {
        let stream = js
            .get_stream(STREAM)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        stream
            .create_consumer(jetstream::consumer::pull::Config {
                filter_subject: subject,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// The next message on `consumer`, acked and decoded.
    async fn take_one(
        consumer: &jetstream::consumer::Consumer<jetstream::consumer::pull::Config>,
    ) -> Result<Staged> {
        let mut messages = consumer
            .fetch()
            .max_messages(1)
            .messages()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let message = messages
            .next()
            .await
            .context("nothing published")?
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        message.ack().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(serde_json::from_slice(&message.payload)?)
    }

    #[tokio::test]
    async fn a_staged_upload_is_in_the_inbox_and_announced_for_its_owner() -> Result<()> {
        let Some(js) = broker().await else {
            eprintln!("no broker; skipping");
            return Ok(());
        };
        ensure(&js, 64 * 1024 * 1024).await?;
        let owner = format!("op_test{}", uuid::Uuid::new_v4().simple());
        let received = consumer_on(&js, received_subject(&owner)).await?;

        let bytes = b"a staged video";
        let upload_id = stage(&js, &owner, &mut &bytes[..], &meta(bytes)).await?;

        let announced = take_one(&received).await?;
        assert_eq!(announced.upload_id, upload_id);
        assert_eq!(announced.meta.digest, crate::sha256_hex_bytes(bytes));
        assert!(
            announced.owner.is_empty(),
            "the node, not the producer, names the owner"
        );
        let inbox = js
            .get_object_store(INBOX)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let back = read_all(open(&js, &announced).await?).await?;
        assert_eq!(back, bytes);
        let segment = derived_name(&upload_id, "seg0.m4s");
        inbox
            .put(segment.as_str(), &mut &b"seg"[..])
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let announced = Staged {
            derived: vec![ObjectMeta {
                filename: "seg0.m4s".into(),
                ..meta(b"seg")
            }],
            ..announced
        };
        discard(&js, &announced).await?;
        discard(&js, &announced).await?;
        assert!(
            inbox
                .info(segment.as_str())
                .await
                .is_ok_and(|info| info.deleted),
            "derived objects go with the source"
        );
        assert!(
            inbox
                .info(upload_id.as_str())
                .await
                .is_ok_and(|info| info.deleted),
            "discarded, twice without error"
        );
        Ok(())
    }

    #[tokio::test]
    async fn advancing_passes_the_upload_to_its_next_step_only() -> Result<()> {
        let Some(js) = broker().await else {
            eprintln!("no broker; skipping");
            return Ok(());
        };
        ensure(&js, 64 * 1024 * 1024).await?;
        let step = format!("t{}", uuid::Uuid::new_v4().simple());
        let next = consumer_on(&js, step_subject(&step)).await?;
        let planned = Staged {
            upload_id: "u1".into(),
            owner: "op_ann".into(),
            meta: meta(b"x"),
            encryption: Encryption::None,
            plan: vec!["done".into(), step.clone()],
            derived: vec![],
            notes: Default::default(),
        };

        planned.advance(&js).await?;
        let passed = take_one(&next).await?;
        assert_eq!(passed.plan, std::slice::from_ref(&step));

        passed.advance(&js).await?;
        let mut after = next
            .fetch()
            .max_messages(1)
            .expires(Duration::from_millis(300))
            .messages()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert!(
            after.next().await.is_none(),
            "the last step publishes nothing"
        );
        Ok(())
    }
}
