//! Turning an uploaded video into something a viewer can seek through.
//!
//! The object store has no server-side range read: an offset is reached by
//! reading and discarding everything before it. A viewer scrubbing a
//! half-gigabyte file therefore re-reads it from the start on every drag,
//! which is expensive for them and worse for everyone else sharing the broker.
//!
//! Segmenting removes the problem rather than working around it. Each HLS
//! segment is its own small, content-addressed object, fetched whole; there
//! are no range reads left to be slow, and a CDN caches each segment
//! separately. It also makes object storage that ranges natively a swap rather
//! than a rescue.

use anyhow::{Context as _, Result};
use std::collections::HashMap;

/// Where an upload is in the segmenting pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HlsState {
    /// Queued or running. The original is stored and servable meanwhile.
    Pending,
    /// A playlist exists and is stored.
    Ready,
    /// Segmenting was attempted and did not finish. The original is intact.
    Failed,
    /// Not attempted: not a video, or no transcoder on this host.
    Skipped,
}

impl HlsState {
    /// How the state reads back out of an annotation.
    ///
    /// One place, because the process that writes the note, the one that polls
    /// it, and the one that turns it into a playlist link are three different
    /// processes — and a state parsed differently in any of them is a video
    /// that plays in one surface and not another.
    ///
    /// An absent or unrecognised note is [`Self::Skipped`]: every object
    /// predates this annotation, and treating unknown as "not attempted" says
    /// exactly what is true of them.
    pub fn from_note(note: Option<&str>) -> Self {
        match note {
            Some("pending") => Self::Pending,
            Some("ready") => Self::Ready,
            Some("failed") => Self::Failed,
            _ => Self::Skipped,
        }
    }

    /// The value written back into the annotation.
    pub fn as_note(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// The `EXT-X-MAP` tag names an initialisation segment in an attribute rather
/// than on a line of its own, so a rewriter that only looked at non-comment
/// lines would leave it pointing at a file nobody stored.
const MAP_TAG: &str = "#EXT-X-MAP:";

/// Rewrite a playlist so every file it references points at its stored URL.
///
/// Errors when the playlist names a file that was not stored: a playlist that
/// resolves to a 404 halfway through is worse than one that never existed,
/// because the failure arrives mid-playback.
pub fn rewrite_playlist(playlist: &str, urls: &HashMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(playlist.len());
    let mut segments = 0_usize;

    for line in playlist.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }
        if let Some(attributes) = trimmed.strip_prefix(MAP_TAG) {
            out.push_str(&rewrite_map_tag(attributes, urls)?);
        } else if trimmed.starts_with('#') {
            out.push_str(line);
        } else {
            out.push_str(&resolve(trimmed, urls)?);
            segments += 1;
        }
        out.push('\n');
    }

    // ffmpeg can exit cleanly having written a header and nothing else. Stored
    // as-is that is a playlist a player accepts and plays silence from.
    if segments == 0 {
        anyhow::bail!("the playlist references no segments");
    }
    Ok(out)
}

/// Replace the `URI="…"` of an `EXT-X-MAP` tag, leaving its other attributes.
fn rewrite_map_tag(attributes: &str, urls: &HashMap<String, String>) -> Result<String> {
    let rewritten = attributes
        .split(',')
        .map(|attribute| match attribute.trim().strip_prefix("URI=") {
            Some(value) => {
                let name = value.trim_matches('"');
                Ok(format!("URI=\"{}\"", resolve(name, urls)?))
            }
            None => Ok(attribute.to_string()),
        })
        .collect::<Result<Vec<_>>>()?
        .join(",");
    Ok(format!("{MAP_TAG}{rewritten}"))
}

/// The stored URL for a file the playlist names.
///
/// Matched on the file name alone: ffmpeg writes bare names, but a playlist
/// carrying a directory prefix must not silently fail to match what was stored
/// under the name.
fn resolve(reference: &str, urls: &HashMap<String, String>) -> Result<String> {
    // Nothing this pipeline produces is absolute. One that is means the
    // playlist did not come from where it was thought to, and rewriting around
    // it would publish a link to somewhere else entirely.
    if reference.contains("://") {
        anyhow::bail!("playlist reference {reference:?} is already absolute");
    }
    let name = reference.rsplit('/').next().unwrap_or(reference);
    urls.get(name)
        .cloned()
        .with_context(|| format!("the playlist names {name:?}, which was not stored"))
}

#[cfg(test)]
mod tests {

    /// Stands in for the digest of whatever was uploaded. Derived objects are
    /// named under it, so a test asserting cleanup needs the same value the
    /// pipeline was given.
    const SOURCE_DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn a_video_deleted_and_uploaded_again_is_segmented_afresh() {
        // Content addressing means the same bytes come back under the same
        // digest, so a note left behind by the previous life would be read as
        // this one's: `hls=ready` pointing at a playlist that was deleted with
        // the video. Segmenting would short-circuit, the status would claim
        // ready, and the player would be handed a 404.
        let Some(blob) = blob_for("reupload", 4 * 1024 * 1024).await else {
            require_broker("a re-upload needs a broker");
            return;
        };
        let source = tempfile::NamedTempFile::new().expect("source");
        let body = b"the same video, uploaded twice";

        let first = store_bytes(
            &blob,
            body,
            SOURCE_DIGEST,
            "clip.mp4",
            "agent-7",
            Visibility::Public,
        )
        .await
        .expect("store");
        let before = segment_and_store(
            &blob,
            &FakeTranscoder::ok(),
            source.path(),
            &SegmentSpec {
                source_digest: &first,
                public_base: "https://cdn.test/c",
                uploaded_by: "agent-7",
                visibility: Visibility::Public,
                work_dir: None,
            },
        )
        .await
        .expect("segmented");
        blob.annotate(&first, HLS_NOTE, "ready")
            .await
            .expect("note");
        blob.annotate(&first, PLAYLIST_NOTE, &before.playlist)
            .await
            .expect("note");

        delete_with_derived(&blob, &first).await.expect("delete");

        // The same bytes again: the digest is identical, which is the whole
        // point of content addressing and the whole hazard here.
        let again = store_bytes(
            &blob,
            body,
            SOURCE_DIGEST,
            "clip.mp4",
            "agent-7",
            Visibility::Public,
        )
        .await
        .expect("store again");
        assert_eq!(again, first, "the same bytes must address the same object");

        let notes = blob.notes(&again).await.unwrap_or_default();
        assert!(
            !notes.contains_key(HLS_NOTE) && !notes.contains_key(PLAYLIST_NOTE),
            "a previous life's notes reattached to the new upload: {notes:?}"
        );

        // And segmenting it again produces something that is actually there.
        let after = segment_and_store(
            &blob,
            &FakeTranscoder::ok(),
            source.path(),
            &SegmentSpec {
                source_digest: &again,
                public_base: "https://cdn.test/c",
                uploaded_by: "agent-7",
                visibility: Visibility::Public,
                work_dir: None,
            },
        )
        .await
        .expect("segmented again");
        assert!(
            blob.head(&after.playlist).await.is_ok(),
            "the playlist recorded after a re-upload must be one that exists"
        );
    }

    #[tokio::test]
    async fn deleting_a_video_takes_its_segments_off_the_air_too() {
        // The bug this exists for: a playlist and its segments are separate
        // public objects. Deleting only the source leaves a video that anyone
        // holding the playlist URL keeps streaming, and that keeps spending
        // the operator's quota with nothing referencing it — invisibly, since
        // the library view hides derived objects.
        let Some(blob) = blob_for("deletederived", 4 * 1024 * 1024).await else {
            require_broker("deleting derived objects needs a broker");
            return;
        };
        let source = tempfile::NamedTempFile::new().expect("source");
        // The source itself, as an upload would have stored it.
        let uploaded = store_bytes(
            &blob,
            b"the original",
            SOURCE_DIGEST,
            "clip.mp4",
            "agent-7",
            Visibility::Public,
        )
        .await
        .expect("store the source");

        let result = segment_and_store(
            &blob,
            &FakeTranscoder::ok(),
            source.path(),
            &SegmentSpec {
                source_digest: &uploaded,
                public_base: "https://cdn.test/content/acme",
                uploaded_by: "agent-7",
                visibility: Visibility::Public,
                work_dir: None,
            },
        )
        .await
        .expect("segmented");
        assert!(!result.stored.is_empty());

        delete_with_derived(&blob, &uploaded)
            .await
            .expect("delete the video");

        for digest in &result.stored {
            assert!(
                blob.head(digest).await.is_err(),
                "a segment outlived the video it came from: {digest}"
            );
        }
        assert!(blob.head(&uploaded).await.is_err(), "the source survived");
        // Nothing left behind: the quota is the operator's, and an object with
        // nothing referencing it is one they can never find to remove.
        let left = blob.list().await.expect("list");
        assert!(left.is_empty(), "{left:?}");
    }

    #[tokio::test]
    async fn one_videos_segments_are_not_deleted_with_anothers() {
        // Derived objects are found by a name built from the source digest,
        // so two videos segmented into the same bucket must not collide.
        let Some(blob) = blob_for("deleteisolated", 4 * 1024 * 1024).await else {
            require_broker("deleting derived objects needs a broker");
            return;
        };
        let source = tempfile::NamedTempFile::new().expect("source");
        let other_digest = "f".repeat(64);

        let mine = store_bytes(
            &blob,
            b"mine",
            SOURCE_DIGEST,
            "a.mp4",
            "agent-7",
            Visibility::Public,
        )
        .await
        .expect("store");
        let theirs = store_bytes(
            &blob,
            b"theirs",
            &other_digest,
            "b.mp4",
            "agent-7",
            Visibility::Public,
        )
        .await
        .expect("store");

        let ours = segment_and_store(
            &blob,
            &FakeTranscoder::ok(),
            source.path(),
            &SegmentSpec {
                source_digest: &mine,
                public_base: "https://cdn.test/c",
                uploaded_by: "agent-7",
                visibility: Visibility::Public,
                work_dir: None,
            },
        )
        .await
        .expect("segmented");

        delete_with_derived(&blob, &theirs)
            .await
            .expect("delete the other video");

        for digest in &ours.stored {
            assert!(
                blob.head(digest).await.is_ok(),
                "deleting one video removed another's segment: {digest}"
            );
        }
        assert!(blob.head(&mine).await.is_ok());
    }

    #[test]
    fn a_state_survives_the_annotation_it_is_written_to() {
        // Written by the process that transcodes, read by the one that polls
        // and by the one that builds a playlist link. Three processes, so a
        // round trip that loses a state is a video playable in one surface
        // and not another.
        for state in [
            HlsState::Pending,
            HlsState::Ready,
            HlsState::Failed,
            HlsState::Skipped,
        ] {
            assert_eq!(HlsState::from_note(Some(state.as_note())), state);
        }
        // Every object stored before this annotation existed has no note, and
        // "not attempted" is exactly what is true of them.
        assert_eq!(HlsState::from_note(None), HlsState::Skipped);
        assert_eq!(HlsState::from_note(Some("banana")), HlsState::Skipped);
    }
    use super::*;

    use crate::files::blob::{Blob as _, NatsBlob, Visibility, quota_from_max_bytes};
    use async_nats::jetstream::{self, object_store};
    use std::path::Path;

    /// A transcoder that writes fixture files, so the pipeline around it —
    /// storing, rewriting, cleaning up — is testable on a host with no ffmpeg.
    struct FakeTranscoder {
        files: Vec<(&'static str, &'static str)>,
        fail: bool,
    }

    impl FakeTranscoder {
        fn ok() -> Self {
            Self {
                files: vec![
                    ("init.mp4", "initialisation segment"),
                    ("seg0.m4s", "first six seconds"),
                    ("seg1.m4s", "the rest"),
                    (
                        PLAYLIST_NAME,
                        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"init.mp4\"\n\
                         #EXTINF:6.0,\nseg0.m4s\n#EXTINF:2.0,\nseg1.m4s\n#EXT-X-ENDLIST\n",
                    ),
                ],
                fail: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl Transcoder for FakeTranscoder {
        async fn segment(&self, _input: &Path, out_dir: &Path) -> Result<()> {
            if self.fail {
                anyhow::bail!("the transcoder gave up");
            }
            for (name, body) in &self.files {
                tokio::fs::write(out_dir.join(name), body).await?;
            }
            Ok(())
        }
    }

    #[test]
    fn ffmpeg_is_asked_to_copy_streams_rather_than_re_encode() {
        // Most uploads are already H.264/AAC. Re-encoding them costs CPU and
        // quality for nothing, so a lost `-c copy` is a real regression and an
        // invisible one.
        let args = Ffmpeg::args(Path::new("/spool/in.mp4"), Path::new("/spool/out"));
        let joined = args.join(" ");
        assert!(joined.contains("-c copy"), "{joined}");
        assert!(joined.contains("-f hls"), "{joined}");
        assert!(joined.contains("-hls_playlist_type vod"), "{joined}");
        assert!(joined.contains("-hls_segment_type fmp4"), "{joined}");
        assert!(
            joined.contains(&format!("-hls_time {SEGMENT_SECONDS}")),
            "{joined}"
        );
        assert!(
            joined.ends_with(&format!("/spool/out/{PLAYLIST_NAME}")),
            "{joined}"
        );
        assert!(
            joined.contains("-nostdin"),
            "a transcode must never wait on a console: {joined}"
        );
    }

    #[test]
    fn produced_files_get_the_media_type_a_player_expects() {
        assert_eq!(segment_mime("index.m3u8"), "application/vnd.apple.mpegurl");
        assert_eq!(segment_mime("seg0.ts"), "video/mp2t");
        assert_eq!(segment_mime("seg0.m4s"), "video/mp4");
        assert_eq!(segment_mime("init.mp4"), "video/mp4");
    }

    async fn blob_for(tag: &str, quota: i64) -> Option<NatsBlob> {
        let url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".into());
        let js: jetstream::Context = jetstream::new(async_nats::connect(&url).await.ok()?);
        let bucket = format!("test_hls_{tag}");
        // Deleted first, so the bucket count is bounded by the number of tests
        // rather than growing with every run — JetStream reserves max_bytes.
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

    #[tokio::test]
    async fn segmenting_stores_every_piece_and_a_playlist_that_points_at_them() {
        let Some(blob) = blob_for("happy", 2 * 1024 * 1024).await else {
            require_broker("segmenting needs a broker");
            return;
        };
        let source = tempfile::NamedTempFile::new().expect("source");

        let result = segment_and_store(
            &blob,
            &FakeTranscoder::ok(),
            source.path(),
            &SegmentSpec {
                source_digest: SOURCE_DIGEST,
                public_base: "https://cdn.test/content/acme",
                uploaded_by: "agent-7",
                visibility: Visibility::Public,
                work_dir: None,
            },
        )
        .await
        .expect("segmented");

        // Three media files plus the playlist.
        assert_eq!(result.stored.len(), 4);
        assert!(result.stored.contains(&result.playlist));

        let playlist = blob.head(&result.playlist).await.expect("playlist stored");
        assert_eq!(playlist.mime, "application/vnd.apple.mpegurl");
        assert_eq!(playlist.visibility, Visibility::Public);

        let (bytes, _) = blob
            .get_range(&result.playlist, 0, playlist.bytes)
            .await
            .expect("read the playlist");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(
            !text.contains(".m4s") && !text.contains("init.mp4"),
            "no local filename may survive: {text}"
        );
        assert_eq!(
            text.matches("https://cdn.test/content/acme/").count(),
            3,
            "every piece must be addressed: {text}"
        );

        // And each URL the playlist hands out resolves to something stored.
        for digest in text
            .split('/')
            .filter(|part| part.len() == 64 && part.chars().all(|c| c.is_ascii_hexdigit()))
        {
            let piece = blob.head(digest.trim()).await.expect("a referenced piece");
            assert_eq!(piece.mime, "video/mp4");
        }
    }

    #[tokio::test]
    async fn segments_of_a_private_video_are_not_published() {
        // The segments are the video. Storing them public would publish an
        // upload its owner marked private, and the playlist would hand out the
        // link.
        let Some(blob) = blob_for("privatesegments", 2 * 1024 * 1024).await else {
            require_broker("private segmenting needs a broker");
            return;
        };
        let source = tempfile::NamedTempFile::new().expect("source");

        let result = segment_and_store(
            &blob,
            &FakeTranscoder::ok(),
            source.path(),
            &SegmentSpec {
                source_digest: SOURCE_DIGEST,
                public_base: "https://cdn.test/content/acme",
                uploaded_by: "agent-7",
                visibility: Visibility::Private,
                work_dir: None,
            },
        )
        .await
        .expect("segmented");

        for digest in &result.stored {
            let piece = blob.head(digest).await.expect("stored");
            assert_eq!(
                piece.visibility,
                Visibility::Private,
                "{digest} was published for a private source"
            );
        }
    }

    #[tokio::test]
    async fn a_transcoder_that_fails_leaves_nothing_behind() {
        let Some(blob) = blob_for("failure", 2 * 1024 * 1024).await else {
            require_broker("failure cleanup needs a broker");
            return;
        };
        let source = tempfile::NamedTempFile::new().expect("source");

        let failed = segment_and_store(
            &blob,
            &FakeTranscoder {
                fail: true,
                ..FakeTranscoder::ok()
            },
            source.path(),
            &SegmentSpec {
                source_digest: SOURCE_DIGEST,
                public_base: "https://cdn.test/acme",
                uploaded_by: "agent-7",
                visibility: Visibility::Public,
                work_dir: None,
            },
        )
        .await
        .expect_err("the transcoder failed");
        assert!(failed.to_string().contains("gave up"));

        assert!(
            blob.list().await.expect("list").is_empty(),
            "a failed segmentation must consume none of the operator's quota"
        );
    }

    #[tokio::test]
    async fn a_playlist_naming_a_piece_that_was_not_stored_stores_nothing() {
        // The dangerous half-success: segments land, the playlist references
        // something that never did, and playback dies partway through.
        let Some(blob) = blob_for("dangling", 2 * 1024 * 1024).await else {
            require_broker("the dangling-reference check needs a broker");
            return;
        };
        let source = tempfile::NamedTempFile::new().expect("source");

        let dangling = FakeTranscoder {
            files: vec![
                ("seg0.m4s", "first"),
                (
                    PLAYLIST_NAME,
                    "#EXTM3U\n#EXTINF:6.0,\nseg0.m4s\n#EXTINF:6.0,\nseg9.m4s\n#EXT-X-ENDLIST\n",
                ),
            ],
            fail: false,
        };
        let refused = segment_and_store(
            &blob,
            &dangling,
            source.path(),
            &SegmentSpec {
                source_digest: SOURCE_DIGEST,
                public_base: "https://cdn.test/acme",
                uploaded_by: "agent-7",
                visibility: Visibility::Public,
                work_dir: None,
            },
        )
        .await
        .expect_err("a dangling reference must not be published");
        assert!(refused.to_string().contains("seg9.m4s"), "{refused}");

        assert!(
            blob.list().await.expect("list").is_empty(),
            "the segments already stored must be removed again"
        );
    }

    #[tokio::test]
    async fn a_transcoder_that_writes_no_playlist_is_a_failure() {
        let Some(blob) = blob_for("noplaylist", 2 * 1024 * 1024).await else {
            require_broker("the missing-playlist check needs a broker");
            return;
        };
        let source = tempfile::NamedTempFile::new().expect("source");

        let silent = FakeTranscoder {
            files: vec![("seg0.m4s", "orphan")],
            fail: false,
        };
        assert!(
            segment_and_store(
                &blob,
                &silent,
                source.path(),
                &SegmentSpec {
                    source_digest: SOURCE_DIGEST,
                    public_base: "https://cdn.test/acme",
                    uploaded_by: "agent-7",
                    visibility: Visibility::Public,
                    work_dir: None,
                },
            )
            .await
            .is_err()
        );
        assert!(
            blob.list().await.expect("list").is_empty(),
            "nothing may be left without a playlist to reach it"
        );
    }

    #[tokio::test]
    async fn running_out_of_quota_partway_leaves_nothing_behind() {
        // The quota is per operator, and a segmentation that half-filled it
        // with unreachable objects would be the worst kind of leak: it costs
        // the operator space and nothing references it.
        let Some(blob) = blob_for("full", 4 * 1024).await else {
            require_broker("the quota check needs a broker");
            return;
        };
        let source = tempfile::NamedTempFile::new().expect("source");

        let fat = FakeTranscoder {
            files: vec![
                ("seg0.m4s", "x"),
                ("seg1.m4s", "y"),
                (
                    PLAYLIST_NAME,
                    "#EXTM3U\n#EXTINF:6.0,\nseg0.m4s\n#EXTINF:6.0,\nseg1.m4s\n#EXT-X-ENDLIST\n",
                ),
            ],
            fail: false,
        };
        // Fill the bucket first so the segmentation cannot fit.
        let filler = vec![b'z'; 3_000];
        let digest = crate::nats_utils::sha256_hex_bytes(&filler);
        let mut reader = filler.as_slice();
        let _ = blob
            .put_stream(
                &digest,
                &mut reader,
                &crate::files::blob::ObjectMeta {
                    digest: digest.clone(),
                    filename: "filler".into(),
                    mime: "video/mp4".into(),
                    bytes: filler.len() as u64,
                    visibility: Visibility::Public,
                    uploaded_by: "agent-7".into(),
                    created_at: crate::files::blob::stamped_now(),
                },
            )
            .await;

        let before = blob.list().await.expect("list").len();
        let refused = segment_and_store(
            &blob,
            &fat,
            source.path(),
            &SegmentSpec {
                source_digest: SOURCE_DIGEST,
                public_base: "https://cdn.test/acme",
                uploaded_by: "agent-7",
                visibility: Visibility::Public,
                work_dir: None,
            },
        )
        .await;
        assert!(
            refused.is_err(),
            "a segmentation that cannot fit the quota must fail rather than half-store"
        );
        let after = blob.list().await.expect("list");
        assert_eq!(
            after.len(),
            before,
            "a segmentation that could not finish must leave the bucket as it found it: {after:?}"
        );
    }

    /// A few seconds of real H.264, made by ffmpeg itself.
    ///
    /// `None` where this host has no ffmpeg — the same gate the pipeline uses,
    /// so a developer without one sees a skip rather than a failure, and CI,
    /// which installs it, does not.
    async fn real_video() -> Result<Option<(tempfile::TempDir, std::path::PathBuf)>> {
        let ffmpeg = Ffmpeg::default();
        if !ffmpeg.available().await {
            // Same rule as the broker gate: CI installs ffmpeg, so a skip
            // there is a test that silently stopped covering the one thing a
            // fake transcoder cannot check.
            let required = std::env::var("REQUIRE_FFMPEG")
                .map(|v| {
                    let v = v.trim().to_string();
                    !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
                })
                .unwrap_or(false);
            anyhow::ensure!(
                !required,
                "REQUIRE_FFMPEG is set, so this test may not skip: no ffmpeg on this host"
            );
            eprintln!("Skipping: no ffmpeg on this host");
            return Ok(None);
        }
        let dir = tempfile::tempdir().context("temp dir")?;
        let path = dir.path().join("source.mp4");
        let made = tokio::process::Command::new(&ffmpeg.binary)
            .args([
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=8:size=320x240:rate=15",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .context("run ffmpeg to build the fixture")?;
        // Loud rather than a skip: ffmpeg is present, so a fixture it cannot
        // build is a broken build of it, not a reason to stop testing.
        anyhow::ensure!(made.success(), "ffmpeg could not make a test video");
        Ok(Some((dir, path)))
    }

    #[tokio::test]
    async fn real_ffmpeg_produces_a_playlist_whose_segments_all_resolve() -> Result<()> {
        // The one thing the fake transcoder cannot tell us: that the argument
        // vector we actually run produces an HLS playlist, and that every
        // piece it names is something we stored.
        let Some(blob) = blob_for("realffmpeg", 8 * 1024 * 1024).await else {
            require_broker("the real transcode needs a broker");
            return Ok(());
        };
        let Some((_dir, source)) = real_video().await? else {
            return Ok(());
        };

        let result = segment_and_store(
            &blob,
            &Ffmpeg::default(),
            &source,
            &SegmentSpec {
                source_digest: SOURCE_DIGEST,
                public_base: "https://cdn.test/content/acme",
                uploaded_by: "agent-7",
                visibility: Visibility::Public,
                work_dir: None,
            },
        )
        .await
        .expect("segmented");

        // Eight seconds at six-second segments is two, plus an init segment
        // and the playlist. Asserted as "more than one segment" rather than an
        // exact count, which ffmpeg is entitled to change.
        assert!(
            result.stored.len() >= 3,
            "expected an init segment, media segments and a playlist: {:?}",
            result.stored
        );

        let playlist = blob.head(&result.playlist).await.expect("playlist stored");
        assert_eq!(playlist.mime, "application/vnd.apple.mpegurl");
        let (bytes, _) = blob
            .get_range(&result.playlist, 0, playlist.bytes)
            .await
            .expect("read the playlist");
        let text = String::from_utf8(bytes).expect("utf8");

        assert!(text.starts_with("#EXTM3U"), "not a playlist: {text}");
        assert!(
            text.contains("#EXT-X-ENDLIST"),
            "not a finished VOD: {text}"
        );
        assert!(
            !text.contains(".m4s") && !text.contains(".ts"),
            "a local filename survived: {text}"
        );

        // Every URL the playlist hands a player must resolve to bytes we hold,
        // which is the property a viewer actually depends on.
        let referenced: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("https://cdn.test/content/acme/"))
            .filter_map(|line| line.rsplit('/').next())
            .map(|tail| tail.trim_end_matches('"'))
            .collect();
        assert!(referenced.len() >= 2, "too few pieces: {text}");
        for digest in referenced {
            let piece = blob.head(digest).await.unwrap_or_else(|e| {
                panic!("the playlist names {digest}, which is not stored: {e}")
            });
            assert!(piece.bytes > 0, "an empty segment: {digest}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_host_without_ffmpeg_reports_it_rather_than_failing() {
        // A library user who never asked for a media pipeline gets `Skipped`
        // and an intact original, not an error.
        let absent = Ffmpeg {
            binary: "ffmpeg-that-is-not-installed".to_string(),
        };
        assert!(!absent.available().await);
    }

    fn urls(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    const PLAYLIST: &str = "#EXTM3U\n\
                            #EXT-X-VERSION:7\n\
                            #EXT-X-TARGETDURATION:6\n\
                            #EXT-X-PLAYLIST-TYPE:VOD\n\
                            #EXT-X-MAP:URI=\"init.mp4\"\n\
                            #EXTINF:6.000000,\n\
                            seg0.m4s\n\
                            #EXTINF:5.500000,\n\
                            seg1.m4s\n\
                            #EXT-X-ENDLIST\n";

    #[test]
    fn every_referenced_file_becomes_its_stored_url() {
        let rewritten = rewrite_playlist(
            PLAYLIST,
            &urls(&[
                ("init.mp4", "https://cdn.test/content/acme/aaa"),
                ("seg0.m4s", "https://cdn.test/content/acme/bbb"),
                ("seg1.m4s", "https://cdn.test/content/acme/ccc"),
            ]),
        )
        .expect("rewritten");

        assert!(rewritten.contains("#EXT-X-MAP:URI=\"https://cdn.test/content/acme/aaa\""));
        assert!(rewritten.contains("\nhttps://cdn.test/content/acme/bbb\n"));
        assert!(rewritten.contains("\nhttps://cdn.test/content/acme/ccc\n"));
        assert!(
            !rewritten.contains("seg0.m4s") && !rewritten.contains("init.mp4"),
            "no local filename may survive: {rewritten}"
        );
    }

    #[test]
    fn the_tags_a_player_needs_are_left_alone() {
        let rewritten = rewrite_playlist(
            PLAYLIST,
            &urls(&[
                ("init.mp4", "https://cdn.test/a"),
                ("seg0.m4s", "https://cdn.test/b"),
                ("seg1.m4s", "https://cdn.test/c"),
            ]),
        )
        .expect("rewritten");

        for tag in [
            "#EXTM3U",
            "#EXT-X-VERSION:7",
            "#EXT-X-TARGETDURATION:6",
            "#EXT-X-PLAYLIST-TYPE:VOD",
            "#EXTINF:6.000000,",
            "#EXT-X-ENDLIST",
        ] {
            assert!(rewritten.contains(tag), "{tag} must survive: {rewritten}");
        }
        assert_eq!(
            rewritten.lines().count(),
            PLAYLIST.lines().count(),
            "rewriting must not add or drop lines"
        );
    }

    #[test]
    fn a_file_that_was_not_stored_is_an_error() {
        // A playlist that resolves to a 404 halfway through fails mid-playback,
        // which is worse than never producing one.
        let missing = rewrite_playlist(
            PLAYLIST,
            &urls(&[
                ("init.mp4", "https://cdn.test/a"),
                ("seg0.m4s", "https://cdn.test/b"),
            ]),
        )
        .expect_err("a missing segment must not be skipped over");
        assert!(
            missing.to_string().contains("seg1.m4s"),
            "the error must name the file: {missing}"
        );
    }

    #[test]
    fn an_unstored_init_segment_is_an_error_too() {
        // The init segment is referenced from an attribute, so a rewriter that
        // only checked whole lines would let this one through unrewritten.
        let missing = rewrite_playlist(
            PLAYLIST,
            &urls(&[
                ("seg0.m4s", "https://cdn.test/b"),
                ("seg1.m4s", "https://cdn.test/c"),
            ]),
        )
        .expect_err("a missing init segment must be caught");
        assert!(missing.to_string().contains("init.mp4"));
    }

    #[test]
    fn blank_lines_and_ordering_survive() {
        let spaced = "#EXTM3U\n\n#EXTINF:6.0,\nseg0.ts\n\n#EXT-X-ENDLIST\n";
        let rewritten =
            rewrite_playlist(spaced, &urls(&[("seg0.ts", "https://cdn.test/x")])).expect("ok");
        assert_eq!(
            rewritten,
            "#EXTM3U\n\n#EXTINF:6.0,\nhttps://cdn.test/x\n\n#EXT-X-ENDLIST\n"
        );
    }

    #[test]
    fn a_playlist_with_no_segments_is_an_error() {
        // ffmpeg exiting cleanly having written a header and nothing else is a
        // failure that would otherwise be stored as a working playlist.
        assert!(rewrite_playlist("#EXTM3U\n#EXT-X-ENDLIST\n", &urls(&[])).is_err());
    }

    #[test]
    fn a_segment_reference_with_a_path_is_matched_by_its_file_name() {
        // ffmpeg writes bare names, but a playlist that carries a directory
        // prefix must not silently fail to match what was stored under the
        // name alone.
        let nested = "#EXTM3U\n#EXTINF:6.0,\nout/seg0.ts\n#EXT-X-ENDLIST\n";
        let rewritten =
            rewrite_playlist(nested, &urls(&[("seg0.ts", "https://cdn.test/x")])).expect("ok");
        assert!(rewritten.contains("https://cdn.test/x"));
    }

    #[test]
    fn a_reference_that_is_already_absolute_is_refused() {
        // Nothing this pipeline produces is absolute. One that is means the
        // playlist did not come from where it was thought to, and rewriting it
        // would publish a link to somewhere else entirely.
        let hostile = "#EXTM3U\n#EXTINF:6.0,\nhttps://evil.test/seg0.ts\n#EXT-X-ENDLIST\n";
        assert!(rewrite_playlist(hostile, &urls(&[("seg0.ts", "https://cdn.test/x")])).is_err());
    }
}

/// What ffmpeg is asked to produce.
///
/// Six seconds is the usual VOD segment length: short enough that a seek lands
/// quickly, long enough that a two-hour film is not tens of thousands of
/// objects.
const SEGMENT_SECONDS: u32 = 6;

/// Prefix on the stored filename of everything segmenting produces.
///
/// A library view lists what someone uploaded, not the hundreds of segments
/// derived from one video, and the filename is the only field that can say so
/// without a second object per segment.
pub const DERIVED_PREFIX: &str = "hls/";

/// Where a source's derived objects are named, as a filename prefix.
///
/// Namespaced by the source so its segments can be found again from the
/// digest alone. Without that, deleting a video would delete only the video:
/// its segments are separate public objects, and they would keep serving to
/// anyone holding the playlist and keep spending the operator's quota, with
/// nothing left that referenced them.
pub fn derived_prefix(source_digest: &str) -> String {
    format!("{DERIVED_PREFIX}{source_digest}/")
}

/// Where segmenting is recorded against the original, as an annotation on it.
///
/// Public because the process that segments and the process that serves are
/// not the same one: a playlist is content-addressed by its own bytes, so its
/// digest cannot be derived from the source's and has to be read from here.
pub const HLS_NOTE: &str = "hls";
/// Where the produced playlist's digest is recorded against the original.
pub const PLAYLIST_NOTE: &str = "playlist";

/// What ffmpeg is told to write, and what the pipeline then looks for.
const PLAYLIST_NAME: &str = "index.m3u8";

/// How long a transcode may run before it is killed.
///
/// Stream copying is fast even for a long film, so overrunning this means
/// something is stuck rather than slow — and a stuck ffmpeg holds the one
/// transcode slot, which would stop every later upload from ever segmenting.
const TRANSCODE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Segmenting a video into a directory.
///
/// A trait because the pipeline around it — storing segments, rewriting the
/// playlist, cleaning up a failure — is where the bugs live, and none of that
/// should need a transcoder installed to be tested.
#[async_trait::async_trait]
pub trait Transcoder: Send + Sync {
    /// Write segments and a playlist named [`PLAYLIST_NAME`] into `out_dir`.
    async fn segment(&self, input: &std::path::Path, out_dir: &std::path::Path) -> Result<()>;
}

/// Segmenting by shelling out to ffmpeg.
pub struct Ffmpeg {
    binary: String,
}

impl Default for Ffmpeg {
    fn default() -> Self {
        Self {
            binary: std::env::var("FFMPEG_BIN").unwrap_or_else(|_| "ffmpeg".to_string()),
        }
    }
}

impl Ffmpeg {
    /// Whether this host can segment at all.
    ///
    /// Probed rather than assumed: a library user who never asked for a media
    /// pipeline should get `Skipped` and an intact original, not a failure.
    pub async fn available(&self) -> bool {
        // Bounded: this runs while the agent is starting, and a wedged binary
        // would otherwise hang startup rather than reporting "no transcoder".
        let probe = tokio::process::Command::new(&self.binary)
            .arg("-version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status();
        matches!(
            tokio::time::timeout(std::time::Duration::from_secs(10), probe).await,
            Ok(Ok(status)) if status.success()
        )
    }

    /// The argument vector, so what is run is testable without running it.
    fn args(input: &std::path::Path, out_dir: &std::path::Path) -> Vec<String> {
        let s = |p: &std::path::Path| p.to_string_lossy().into_owned();
        vec![
            "-nostdin".into(),
            "-y".into(),
            // The input is a file this process spooled, and nothing else. An
            // uploaded container can carry external references (`dref` boxes
            // pointing at file:// or http://) which ffmpeg would otherwise
            // resolve and mux into segments this pipeline then publishes.
            "-protocol_whitelist".into(),
            "file".into(),
            "-i".into(),
            s(input),
            "-c".into(),
            "copy".into(),
            "-f".into(),
            "hls".into(),
            "-hls_time".into(),
            SEGMENT_SECONDS.to_string(),
            "-hls_playlist_type".into(),
            "vod".into(),
            "-hls_segment_type".into(),
            "fmp4".into(),
            "-hls_flags".into(),
            "independent_segments".into(),
            s(&out_dir.join(PLAYLIST_NAME)),
        ]
    }
}

#[async_trait::async_trait]
impl Transcoder for Ffmpeg {
    async fn segment(&self, input: &std::path::Path, out_dir: &std::path::Path) -> Result<()> {
        use tokio::io::AsyncReadExt as _;

        let mut child = tokio::process::Command::new(&self.binary)
            .args(Self::args(input, out_dir))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            // Without this a cancelled segmentation task, or a runtime
            // shutdown, drops the handle and leaves ffmpeg running orphaned.
            .kill_on_drop(true)
            .spawn()
            .context("run ffmpeg")?;

        // Drained concurrently with the wait. ffmpeg is chatty on stderr, and
        // a full pipe with nobody reading it blocks the process forever —
        // which the timeout would then report as a hang of its own making.
        let mut stderr = child.stderr.take();
        let drain = tokio::spawn(async move {
            let mut buffer = String::new();
            if let Some(pipe) = stderr.as_mut() {
                let _ = pipe.read_to_string(&mut buffer).await;
            }
            buffer
        });

        let waited = tokio::time::timeout(TRANSCODE_TIMEOUT, child.wait()).await;
        let status = match waited {
            Ok(status) => status.context("wait for ffmpeg")?,
            Err(_) => {
                let _ = child.kill().await;
                drain.abort();
                anyhow::bail!(
                    "ffmpeg did not finish within {}s and was killed",
                    TRANSCODE_TIMEOUT.as_secs()
                );
            }
        };

        if !status.success() {
            // ffmpeg's diagnosis is on stderr and is the only useful thing
            // about a failure; an exit code alone leaves nothing to act on.
            let stderr = drain.await.unwrap_or_default();
            let tail: String = stderr.lines().rev().take(5).collect::<Vec<_>>().join("; ");
            anyhow::bail!("ffmpeg failed ({status}): {tail}");
        }
        Ok(())
    }
}

/// The media type of a file the transcoder produced.
fn segment_mime(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or_default() {
        "m3u8" => "application/vnd.apple.mpegurl",
        "ts" => "video/mp2t",
        _ => "video/mp4",
    }
}

/// What segmenting produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segmented {
    /// The digest to point a player at.
    pub playlist: String,
    /// Every object stored, playlist included — what a caller deletes to undo
    /// this.
    pub stored: Vec<String>,
}

/// Segment `source` and store the result, returning the playlist's digest.
///
/// Everything stored is removed again if any step fails. A half-stored
/// segmentation is worse than none: it consumes the operator's quota and
/// nothing references it, so nothing will ever clean it up.
/// What a segmentation should produce, and where.
///
/// Grouped rather than passed one by one: five of these are strings and paths
/// whose order the compiler cannot always tell apart, and the cost of getting
/// two of them the wrong way round is a playlist that points somewhere wrong
/// on every viewer's screen.
pub struct SegmentSpec<'a> {
    /// The stored object being segmented. Derived objects are named under it,
    /// so deleting it can find them again.
    pub source_digest: &'a str,
    /// What a playlist's entries are addressed against, including the path the
    /// operator's files answer on — entries come out as `{public_base}/{digest}`.
    pub public_base: &'a str,
    pub uploaded_by: &'a str,
    /// Inherited by every derived object. A private video whose segments were
    /// public would not be private at all: the segments are the video.
    pub visibility: crate::files::blob::Visibility,
    /// Where the transcoder writes before anything is stored. `None` is the
    /// platform temporary directory, which in a container is frequently a
    /// tmpfs sized for nothing like a video.
    pub work_dir: Option<&'a std::path::Path>,
}

pub async fn segment_and_store(
    blob: &dyn crate::files::blob::Blob,
    transcoder: &dyn Transcoder,
    source: &std::path::Path,
    spec: &SegmentSpec<'_>,
) -> Result<Segmented> {
    let SegmentSpec {
        source_digest,
        public_base,
        uploaded_by,
        visibility,
        work_dir,
    } = *spec;
    // ffmpeg writes every segment here before any of it is stored, so this
    // needs room for the whole output. Unset it is the platform temporary
    // directory, which in a container is frequently a tmpfs sized for nothing
    // like a video — and a transcode that fills it reports only `failed`.
    let work = match work_dir {
        Some(dir) => tempfile::tempdir_in(dir),
        None => tempfile::tempdir(),
    }
    .with_context(|| {
        format!(
            "make a working directory in {}",
            work_dir
                .map(|d| d.display().to_string())
                .unwrap_or_else(|| std::env::temp_dir().display().to_string())
        )
    })?;
    transcoder.segment(source, work.path()).await?;

    let produced = produced_files(work.path()).await?;

    let mut stored = Vec::new();
    let mut urls = HashMap::new();
    for name in produced.iter().filter(|name| *name != PLAYLIST_NAME) {
        match store_file(
            blob,
            &work.path().join(name),
            source_digest,
            name,
            uploaded_by,
            visibility,
        )
        .await
        {
            Ok(digest) => {
                urls.insert(name.clone(), format!("{public_base}/{digest}"));
                stored.push(digest);
            }
            Err(e) => {
                remove_all(blob, &stored).await;
                return Err(e.context(format!("store segment {name}")));
            }
        }
    }

    let playlist = match finish(
        blob,
        work.path(),
        source_digest,
        &urls,
        uploaded_by,
        visibility,
    )
    .await
    {
        Ok(digest) => digest,
        Err(e) => {
            remove_all(blob, &stored).await;
            return Err(e);
        }
    };
    stored.push(playlist.clone());
    Ok(Segmented { playlist, stored })
}

/// The files the transcoder left behind, checked for a playlist among them.
async fn produced_files(work: &std::path::Path) -> Result<Vec<String>> {
    let mut produced = Vec::new();
    let mut entries = tokio::fs::read_dir(work)
        .await
        .context("read what the transcoder produced")?;
    while let Some(entry) = entries.next_entry().await.context("read a produced file")? {
        if entry.path().is_file() {
            produced.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    // ffmpeg can exit zero having written nothing useful; without a playlist
    // the segments are unreachable and storing them is pure cost.
    if !produced.iter().any(|name| name == PLAYLIST_NAME) {
        anyhow::bail!("the transcoder wrote no {PLAYLIST_NAME}");
    }
    Ok(produced)
}

/// Rewrite the playlist against the stored URLs and store it too.
async fn finish(
    blob: &dyn crate::files::blob::Blob,
    work: &std::path::Path,
    source_digest: &str,
    urls: &HashMap<String, String>,
    uploaded_by: &str,
    visibility: crate::files::blob::Visibility,
) -> Result<String> {
    let raw = tokio::fs::read_to_string(work.join(PLAYLIST_NAME))
        .await
        .context("read the playlist")?;
    let rewritten = rewrite_playlist(&raw, urls)?;
    store_bytes(
        blob,
        rewritten.as_bytes(),
        source_digest,
        PLAYLIST_NAME,
        uploaded_by,
        visibility,
    )
    .await
}

async fn store_file(
    blob: &dyn crate::files::blob::Blob,
    path: &std::path::Path,
    source_digest: &str,
    name: &str,
    uploaded_by: &str,
    visibility: crate::files::blob::Visibility,
) -> Result<String> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read {name}"))?;
    store_bytes(blob, &bytes, source_digest, name, uploaded_by, visibility).await
}

async fn store_bytes(
    blob: &dyn crate::files::blob::Blob,
    bytes: &[u8],
    source_digest: &str,
    name: &str,
    uploaded_by: &str,
    visibility: crate::files::blob::Visibility,
) -> Result<String> {
    use crate::files::blob::{ObjectMeta, stamped_now};

    let digest = crate::nats_utils::sha256_hex_bytes(bytes);
    let mut reader = bytes;
    blob.put_stream(
        &digest,
        &mut reader,
        &ObjectMeta {
            digest: digest.clone(),
            filename: format!("{}{name}", derived_prefix(source_digest)),
            mime: segment_mime(name).to_string(),
            bytes: bytes.len() as u64,
            // Inherited from the source. A private video whose segments were
            // public would not be private at all: the segments are the video.
            visibility,
            uploaded_by: uploaded_by.to_string(),
            created_at: stamped_now(),
        },
    )
    .await?;
    Ok(digest)
}

/// Delete a stored object and everything segmenting derived from it.
///
/// Deleting only the source would not take the video off the air: its segments
/// and playlist are separate objects, public in their own right, and a player
/// already holding the playlist URL would keep streaming from them. They would
/// also keep spending the operator's quota with nothing referencing them, and
/// the library view hides them, so nothing would ever notice.
///
/// The derived objects go first. Interrupted halfway, that leaves a source
/// whose playlist is incomplete — a video that still plays, since the original
/// is intact and a retry finishes the job. The other order leaves segments
/// nothing points at and nothing can find again.
pub async fn delete_with_derived(
    blob: &dyn crate::files::blob::Blob,
    source_digest: &str,
) -> Result<()> {
    for digest in derived_of(blob, source_digest).await {
        if let Err(e) = blob.delete(&digest).await {
            tracing::warn!(digest, error = %format!("{e:#}"), "could not remove a derived object");
        }
    }
    blob.delete(source_digest).await
}

/// The digests of everything segmenting produced from `source_digest`.
///
/// Found by name rather than from a manifest: a list of segments recorded
/// against the source would be one more thing that can disagree with what the
/// bucket actually holds, and a partial segmentation writes objects before it
/// would ever write the manifest.
async fn derived_of(blob: &dyn crate::files::blob::Blob, source_digest: &str) -> Vec<String> {
    let prefix = derived_prefix(source_digest);
    blob.list()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|meta| meta.filename.starts_with(&prefix))
        .map(|meta| meta.digest)
        .collect()
}

/// Best-effort removal of a partial segmentation.
///
/// Failures are logged, not propagated: the caller is already reporting why
/// segmenting failed, and replacing that with a cleanup error would hide it.
async fn remove_all(blob: &dyn crate::files::blob::Blob, digests: &[String]) {
    for digest in digests {
        if let Err(e) = blob.delete(digest).await {
            tracing::warn!(digest, error = %format!("{e:#}"), "could not remove a partial segment");
        }
    }
}
