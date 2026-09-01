//! Accepting an uploaded file, whoever is accepting it.
//!
//! Both the agent's dashboard API and the orchestrator's operator API take
//! uploads, and they have to agree on every part a viewer later depends on:
//! what the digest is, which media types are storable, how large is too large,
//! and what the stored metadata says. One implementation, two HTTP shells —
//! each renders [`Refusal`] in its own idiom.

use crate::files::blob::{Blob, ObjectMeta, QuotaExceeded, Visibility, stamped_now};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// How much of a file is read before its type is decided.
const SNIFF_BYTES: usize = 16;

/// Why an upload was not stored.
///
/// Carries the reason rather than a status code: the two callers answer on
/// different surfaces and map these themselves.
#[derive(Debug)]
pub enum Refusal {
    /// Larger than the caller's ceiling. Detected while streaming.
    TooLarge { max: u64 },
    /// Not a media type this deployment is willing to serve back.
    UnsupportedType,
    /// The request body was not a well-formed upload.
    Malformed(String),
    /// The operator's file space is full.
    OutOfSpace { used_bytes: u64, quota_bytes: u64 },
    /// Spooling or storing failed for a reason the caller cannot fix.
    Io(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { max } => write!(f, "upload exceeds the {max} byte limit"),
            Self::UnsupportedType => {
                write!(f, "this is not a media type this deployment serves")
            }
            Self::Malformed(why) => write!(f, "malformed upload: {why}"),
            Self::OutOfSpace {
                used_bytes,
                quota_bytes,
            } => write!(
                f,
                "storage quota exceeded: {used_bytes} of {quota_bytes} bytes used"
            ),
            Self::Io(why) => write!(f, "{why}"),
        }
    }
}

/// A filename safe to store and to hand to whatever serves it.
///
/// The value is the uploader's, and a downstream process puts it in a
/// `Content-Disposition` header. Sanitised at the boundary where it enters
/// storage rather than trusting every later reader to remember.
pub fn safe_filename(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .take(120)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "upload".to_string()
    } else {
        cleaned
    }
}

/// An upload on disk, hashed on the way there.
pub struct Spooled {
    pub(crate) file: tokio::fs::File,
    /// Kept alive: dropping the handle deletes the file, and the spool has to
    /// outlive both the read that streams it into the bucket and any transcode
    /// that follows.
    pub(crate) guard: tempfile::TempPath,
    pub digest: String,
    pub bytes: u64,
    /// The first bytes, for deciding the media type without re-reading.
    pub(crate) head: Vec<u8>,
    pub filename: String,
}

impl Spooled {
    /// Where the upload is on disk.
    ///
    /// Valid until this is dropped, which deletes it — a transcode that reads
    /// the file has to be given the whole `Spooled`, not just this path.
    pub fn path(&self) -> &std::path::Path {
        &self.guard
    }
}

/// Write a field to a temporary file, hashing as it lands and stopping at the
/// ceiling.
///
/// The cap is enforced while reading, not after: reading a whole body to
/// discover it was too large is the denial of service the cap exists to
/// prevent.
pub async fn spool(
    mut field: axum::extract::multipart::Field<'_>,
    max: u64,
) -> Result<Spooled, Refusal> {
    let spool =
        tempfile::NamedTempFile::new().map_err(|e| Refusal::Io(format!("no spool file: {e}")))?;
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
        .map_err(|e| Refusal::Io(format!("no spool file: {e}")))?;

    let mut hasher = Sha256::new();
    let mut bytes: u64 = 0;
    let mut head = Vec::with_capacity(SNIFF_BYTES);

    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| Refusal::Malformed(format!("upload interrupted: {e}")))?
    {
        bytes += chunk.len() as u64;
        if bytes > max {
            return Err(Refusal::TooLarge { max });
        }
        if head.len() < SNIFF_BYTES {
            let wanted = SNIFF_BYTES - head.len();
            head.extend_from_slice(&chunk[..wanted.min(chunk.len())]);
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| Refusal::Io(format!("could not spool the upload: {e}")))?;
    }
    // Propagated, not swallowed: `write_all` can return before the bytes reach
    // the OS, so a full disk surfaces here. Ignoring it would store a short
    // file under the digest of the whole one, which is the single invariant
    // content addressing rests on.
    file.flush()
        .await
        .map_err(|e| Refusal::Io(format!("could not flush the upload to disk: {e}")))?;

    Ok(Spooled {
        file,
        guard: path,
        digest: hex::encode(hasher.finalize()),
        bytes,
        head,
        filename: String::new(),
    })
}

pub(crate) const MAGIC: [(&[u8], &str); 8] = [
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
pub fn sniff_mime(head: &[u8]) -> Option<&'static str> {
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
pub fn sniff_container(head: &[u8]) -> Option<&'static str> {
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
pub fn sniff_mpeg_frame(head: &[u8]) -> Option<&'static str> {
    (head.len() >= 2 && head[0] == 0xff && (head[1] & 0xe0) == 0xe0).then_some("audio/mpeg")
}

/// The longest a non-file part may be.
///
/// `Field::text()` collects a part whole with no ceiling of its own, and an
/// upload route has to raise the request-level limit so a video can through.
/// A part named anything but `file` would otherwise be unbounded.
pub const MAX_FIELD_BYTES: usize = 256;

/// Refuse an upload whose declared size is already over the ceiling.
///
/// The streaming check catches an oversize body too, but only once enough of
/// it has arrived to exceed the limit — and a server that rejects while the
/// client is still sending gives the client a reset connection rather than the
/// status it sent. For a large file over a slow link that is minutes of
/// upload ending in "the connection dropped", which says nothing about why.
///
/// `Content-Length` is absent on a chunked body, in which case there is
/// nothing to check here and the streaming limit is what stops it.
pub fn declared_too_large(headers: &axum::http::HeaderMap, max: u64) -> Option<Refusal> {
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()?;
    // The envelope rides along with the file, so this only fires where the
    // body could not possibly fit however small the envelope is.
    (declared > max.saturating_add(MULTIPART_ENVELOPE_ALLOWANCE))
        .then_some(Refusal::TooLarge { max })
}

/// Room left for multipart boundaries and part headers when judging a declared
/// body length against the file ceiling.
pub const MULTIPART_ENVELOPE_ALLOWANCE: u64 = 64 * 1024;

/// Walk an upload form: the file, spooled and hashed, and its visibility.
///
/// Shared so both surfaces enforce the same rules — in particular that
/// `visibility` is stated rather than assumed. Defaulting either way guesses
/// whether the uploader meant to publish, and the mistake is invisible until
/// the link is out, or until it is not.
pub async fn read_upload_form(
    form: &mut axum::extract::Multipart,
    max_upload_bytes: u64,
) -> Result<(Visibility, Spooled), Refusal> {
    let mut visibility: Option<Visibility> = None;
    let mut file: Option<Spooled> = None;

    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| Refusal::Malformed(e.to_string()))?
    {
        match field.name().unwrap_or_default() {
            "visibility" => visibility = Some(read_visibility(read_short_field(field).await?)?),
            "file" => {
                let filename = safe_filename(field.file_name().unwrap_or("upload"));
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

    let file = file.ok_or_else(|| Refusal::Malformed("no `file` part in the upload".into()))?;
    let visibility = visibility.ok_or_else(|| {
        Refusal::Malformed("no `visibility` part: send `public` or `private` explicitly".into())
    })?;
    Ok((visibility, file))
}

/// Read a small non-file part, refusing one that is not small.
async fn read_short_field(
    mut field: axum::extract::multipart::Field<'_>,
) -> Result<String, Refusal> {
    let mut raw = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| Refusal::Malformed(e.to_string()))?
    {
        raw.extend_from_slice(&chunk);
        if raw.len() > MAX_FIELD_BYTES {
            return Err(Refusal::TooLarge {
                max: MAX_FIELD_BYTES as u64,
            });
        }
    }
    String::from_utf8(raw).map_err(|_| Refusal::Malformed("a form field is not text".into()))
}

fn read_visibility(raw: String) -> Result<Visibility, Refusal> {
    match raw.trim() {
        "public" => Ok(Visibility::Public),
        "private" => Ok(Visibility::Private),
        other => Err(Refusal::Malformed(format!(
            "visibility {other:?} is neither public nor private"
        ))),
    }
}

/// Store a spooled upload, returning what was written.
///
/// The media type is sniffed from the file's own first bytes and checked
/// against the allowlist here, not by the caller: the stored type is echoed
/// back as `Content-Type` by whatever serves it, so a type this deployment
/// never agreed to serve must not become storable by adding a second entry
/// point.
pub async fn store_upload(
    blob: &dyn Blob,
    spooled: &mut Spooled,
    visibility: Visibility,
    uploaded_by: &str,
) -> Result<ObjectMeta, Refusal> {
    let Some(mime) = sniff_mime(&spooled.head) else {
        return Err(Refusal::UnsupportedType);
    };

    let meta = ObjectMeta {
        digest: spooled.digest.clone(),
        filename: spooled.filename.clone(),
        mime: mime.to_string(),
        bytes: spooled.bytes,
        visibility,
        uploaded_by: uploaded_by.to_string(),
        created_at: stamped_now(),
    };

    spooled
        .file
        .rewind()
        .await
        .map_err(|e| Refusal::Io(format!("could not re-read the upload: {e}")))?;

    match blob
        .put_stream(&spooled.digest, &mut spooled.file, &meta)
        .await
    {
        Ok(stored) => Ok(stored),
        Err(e) => match e.downcast_ref::<QuotaExceeded>() {
            Some(full) => Err(Refusal::OutOfSpace {
                used_bytes: full.used_bytes,
                quota_bytes: full.quota_bytes,
            }),
            None => Err(Refusal::Io(format!("could not store the upload: {e:#}"))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_declared_length_over_the_ceiling_is_refused_before_the_body_arrives() {
        let max = 1_000_u64;
        let with = |value: &str| {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(axum::http::header::CONTENT_LENGTH, value.parse().unwrap());
            headers
        };

        assert!(
            declared_too_large(
                &with(&(max + MULTIPART_ENVELOPE_ALLOWANCE + 1).to_string()),
                max
            )
            .is_some(),
            "a body that cannot fit however small the envelope is must be refused"
        );

        // Everything that could still fit is left to the streaming check,
        // which measures the file rather than the envelope around it.
        for could_fit in [
            "0",
            "999",
            "1000",
            &(max + MULTIPART_ENVELOPE_ALLOWANCE).to_string(),
        ] {
            assert!(
                declared_too_large(&with(could_fit), max).is_none(),
                "{could_fit} might still fit"
            );
        }

        // No length at all is a chunked body: nothing to judge here, and the
        // streaming limit is what stops it.
        assert!(declared_too_large(&axum::http::HeaderMap::new(), max).is_none());
        // Nor is a length that is not a number a reason to refuse outright.
        assert!(declared_too_large(&with("banana"), max).is_none());
        // And an absurd one does not overflow into acceptance.
        assert!(declared_too_large(&with(&u64::MAX.to_string()), max).is_some());
    }

    #[test]
    fn a_filename_is_made_safe_where_it_enters_storage() {
        // The value is the uploader's and a downstream process puts it in a
        // header, so it is sanitised here rather than trusted by every reader.
        assert_eq!(safe_filename("clip.mp4"), "clip.mp4");
        assert_eq!(safe_filename("ev\"il\r\n.mp4"), "evil.mp4");
        assert_eq!(safe_filename(""), "upload");
        assert_eq!(safe_filename("   "), "upload");
        assert_eq!(safe_filename(&"x".repeat(500)).len(), 120);
    }
}
