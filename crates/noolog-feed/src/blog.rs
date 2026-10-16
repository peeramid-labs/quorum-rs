//! The blog entry extension to a channel record.
//!
//! A record is shaped like a JMAP `Email`, and an email has nowhere to put a
//! slug, a feature image or a canonical URL. Rather than bend `subject` and
//! `keywords` into carrying them, everything a blog entry needs beyond what
//! mail already has lives under one vendor property, [`BLOG`], the way RFC
//! 8620 §1.8 tells a vendor to extend a standard type: a client that never
//! opted into the capability ignores the property and still renders the post
//! as a message.
//!
//! The shape is the subject of RFC-0001 in the `noolog-rfcs` repository; this
//! module is its reference implementation. Ghost is the first system it was
//! fitted against, so every Ghost post field has a home here, but nothing in
//! it is Ghost's — `origin` is the only place the source system is named.

use anyhow::Result;

/// The property a blog entry hangs off a record.
///
/// Colon-namespaced like JMAP's own `header:` properties, and prefixed with a
/// name only this vendor uses, so it cannot collide with a property a later
/// JMAP revision adds to `Email`.
pub const BLOG: &str = "noolog:blog";

/// The capability a JMAP client names in `using` to see the property.
///
/// A URL on a domain the vendor owns, per RFC 8620 §1.8.
pub const BLOG_CAPABILITY: &str = "https://noolog.io/jmap/blog";

/// The longest a slug may be. Generous — it is a path segment, and a path
/// segment longer than this is a sentence, not a name.
const MAX_SLUG: usize = 200;

/// Whether a record is an article or a standalone page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Post,
    Page,
}

/// Who may read the full entry. Advisory on a world-readable record: it is
/// what the *source* said, kept so a port loses nothing, and a renderer that
/// gates content may honour it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Public,
    Members,
    Paid,
}

/// An image a renderer shows before or beside the entry.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

/// One of the entry's authors, by name.
///
/// `from` on the record is the operator who posted it, which is a different
/// thing: one operator ports a whole blog, and every entry still names who
/// wrote it. No email — a byline is public, an address is not.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    pub slug: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_image: Option<String>,
}

/// A tag, with what a tag page would need. The slug is also a record keyword,
/// so a JMAP client filters on it without knowing this property exists.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    pub slug: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_image: Option<String>,
    /// `true` for a tag the source used for its own bookkeeping — Ghost's
    /// `#internal` tags — which a renderer must not show as a label.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub internal: bool,
}

/// What search engines and link unfurlers are told.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub og_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub og_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub og_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub twitter_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub twitter_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub twitter_image: Option<String>,
}

impl Meta {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Markup the entry asks to have injected into the page around it.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeInjection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foot: Option<String>,
}

/// The editor's own document, kept so the entry can be edited again where it
/// came from. `htmlBody` is what a reader sees; this is what an author opens.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    /// `lexical`, `mobiledoc`, `markdown`, … — whatever the editor calls it.
    pub format: String,
    pub content: String,
}

/// Where the entry was ported from, for anyone reconciling the two later.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Origin {
    pub system: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
}

/// Everything a blog entry carries beyond a message.
///
/// Absent on a record that is not a blog entry. Present with only `slug` on
/// the smallest one — every other field defaults, so an author writing by
/// hand sends the slug and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Blog {
    /// The entry's path segment: `introducing-nsed` in `/blog/introducing-nsed`.
    /// Unique within a channel per operator; posting again under the same
    /// slug is how an entry is revised.
    pub slug: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "is_default")]
    pub visibility: Visibility,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub featured: bool,
    /// A summary the author wrote, as opposed to one cut from the body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_image: Option<Image>,
    /// When the author last touched it. `receivedAt` on the record is when it
    /// was published, and a record is immutable, so this is the only place an
    /// edit date can live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<Author>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<Tag>,
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_injection: Option<CodeInjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

impl Blog {
    /// The entry a caller with nothing but a slug — a test, a post written by
    /// hand — sends: the slug is the one field without a default.
    pub fn with_slug(slug: impl Into<String>) -> Self {
        Self {
            slug: slug.into(),
            ..Self::default()
        }
    }

    /// Refuse an entry a channel cannot index.
    ///
    /// Only the slug is checked: it is the one field the server acts on. The
    /// rest is carried for a renderer, and a wrong `canonicalUrl` is the
    /// author's to fix, not the server's to refuse.
    pub fn validate(&self) -> Result<()> {
        validate_slug(&self.slug)
    }
}

/// A slug is a single lower-case path segment.
///
/// Underscores are allowed alongside hyphens because a blog being ported
/// already has URLs, and refusing `introducing_nsed` would break every link
/// to it for the sake of a style rule.
pub fn validate_slug(slug: &str) -> Result<()> {
    if slug.is_empty() {
        anyhow::bail!("a blog entry needs a slug");
    }
    if slug.len() > MAX_SLUG {
        anyhow::bail!("a slug is at most {MAX_SLUG} characters");
    }
    let legal =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.');
    if !slug.chars().all(legal) {
        anyhow::bail!("{slug:?} is not a slug: lower-case letters, digits, `-`, `_` and `.` only");
    }
    if slug.starts_with('.') || slug == ".." {
        anyhow::bail!("{slug:?} is not a slug: it would name a directory");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smallest_entry_is_a_slug_and_nothing_else() {
        // An author writing by hand sends the slug; a Ghost port fills the
        // rest. Serialising the minimal one must not spray defaults a reader
        // then has to know are defaults.
        let json = serde_json::to_value(Blog::with_slug("hello")).expect("serialise");
        assert_eq!(json, serde_json::json!({ "slug": "hello" }));

        let back: Blog = serde_json::from_value(json).expect("deserialise");
        assert_eq!(back.kind, Kind::Post);
        assert_eq!(back.visibility, Visibility::Public);
        assert!(!back.featured && back.tags.is_empty() && back.meta.is_empty());
    }

    #[test]
    fn a_full_entry_round_trips_in_camel_case() {
        let entry = Blog {
            slug: "introducing_nsed".into(),
            kind: Kind::Post,
            visibility: Visibility::Members,
            featured: true,
            excerpt: Some("short".into()),
            canonical_url: Some("https://elsewhere.example/x".into()),
            locale: Some("en".into()),
            feature_image: Some(Image {
                url: "https://img.example/a.png".into(),
                alt: Some("alt".into()),
                caption: None,
            }),
            updated_at: Some("2026-01-22T13:14:42Z".into()),
            created_at: Some("2026-01-20T00:00:00Z".into()),
            authors: vec![Author {
                slug: "peersky".into(),
                name: "Tim".into(),
                ..Default::default()
            }],
            tags: vec![Tag {
                slug: "news".into(),
                name: "News".into(),
                internal: false,
                ..Default::default()
            }],
            meta: Meta {
                og_image: Some("https://img.example/og.png".into()),
                ..Default::default()
            },
            code_injection: Some(CodeInjection {
                head: Some("<style></style>".into()),
                foot: None,
            }),
            source: Some(Source {
                format: "lexical".into(),
                content: "{}".into(),
            }),
            origin: Some(Origin {
                system: "ghost".into(),
                id: Some("1".into()),
                uuid: None,
            }),
        };
        let json = serde_json::to_value(&entry).expect("serialise");
        assert_eq!(json["canonicalUrl"], "https://elsewhere.example/x");
        assert_eq!(json["featureImage"]["url"], "https://img.example/a.png");
        assert_eq!(json["meta"]["ogImage"], "https://img.example/og.png");
        assert_eq!(json["codeInjection"]["head"], "<style></style>");
        assert!(
            json["tags"][0].get("internal").is_none(),
            "false is the default and not carried"
        );
        let back: Blog = serde_json::from_value(json).expect("deserialise");
        assert_eq!(back, entry);
    }

    #[test]
    fn a_slug_is_one_lower_case_path_segment() {
        for ok in ["hello", "introducing_nsed", "nsed-v0-5-0", "v1.2", "7702"] {
            validate_slug(ok).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
        }
        for bad in ["", "Hello", "a b", "a/b", "..", ".hidden", "ünïcode", "a?b"] {
            assert!(validate_slug(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(validate_slug(&"x".repeat(MAX_SLUG + 1)).is_err());
    }
}
