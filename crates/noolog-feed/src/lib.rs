//! Channels, and the records posted to them.
//!
//! A channel is addressed like a mailing list — `acme-showcase@noolog.io` —
//! and a record is shaped like a JMAP `Email`. Both choices are for the
//! reader: an address is a name people already know how to hold and share,
//! and a mail-shaped record means a client that can render a message can
//! render a post.
//!
//! The address is an **opaque key**. It is never parsed back into an owner:
//! the separator is a hyphen, operator names may contain hyphens, and
//! `acme-showcase` cannot be split into `(acme, showcase)` and
//! `(acme-showcase, default)` unambiguously. Ownership is recorded when a
//! channel is created and resolved by lookup.

use anyhow::Result;

/// The longest a local part may be, per RFC 5321.
const MAX_LOCAL: usize = 64;
/// The longest a whole address may be, per RFC 5321.
const MAX_ADDRESS: usize = 254;

/// Build the address of an operator's channel.
///
/// `channel` of `None` is the operator's default channel. A hyphen separates
/// the parts rather than a dot: dots in a local part are legal but carry
/// folklore — some providers ignore them, and doubled or edge dots are
/// invalid — which matters if these addresses ever accept real mail.
/// The address a channel is reached at.
///
/// The name alone: `showcase@noolog.io`, not `acme-showcase@noolog.io`. A
/// channel is a subject people write to, not a thing an operator owns, so
/// prefixing it with whoever happened to post first would name the wrong
/// thing — and would give the same subject as many addresses as there are
/// operators interested in it.
///
/// Losing the prefix also loses the ambiguity it carried: `acme` + `showcase`
/// and an operator called `acme-showcase` used to produce the same string.
pub fn channel_address(name: &str, domain: &str) -> Result<String> {
    let local = label(name);
    if local.is_empty() {
        anyhow::bail!("a channel needs a name");
    }
    let address = format!("{local}@{}", domain.trim().to_ascii_lowercase());
    validate_address(&address)?;
    Ok(address)
}

/// The part of a name usable in a local part.
fn label(raw: &str) -> String {
    let mapped: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // Collapse and trim the separator, so a name full of punctuation cannot
    // produce `--` or a leading hyphen — both of which read as structure that
    // is not there.

    mapped
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Refuse an address that is not one this deployment could have issued.
pub fn validate_address(address: &str) -> Result<()> {
    let (local, domain) = address
        .split_once('@')
        .ok_or_else(|| anyhow::anyhow!("{address:?} has no domain"))?;

    if address.len() > MAX_ADDRESS {
        anyhow::bail!("address is longer than {MAX_ADDRESS} characters");
    }
    if local.is_empty() || local.len() > MAX_LOCAL {
        anyhow::bail!("local part must be 1 to {MAX_LOCAL} characters");
    }
    if domain.is_empty() || !domain.contains('.') {
        anyhow::bail!("{domain:?} is not a domain");
    }
    // Deliberately narrower than RFC 5322 allows: this deployment issues every
    // address it serves, so anything it would not produce is a mistake rather
    // than an exotic-but-legal address.
    let legal = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    };
    if !legal(local) || !legal(domain) {
        anyhow::bail!("{address:?} contains characters this deployment does not issue");
    }
    if local.starts_with(['-', '.'])
        || local.ends_with(['-', '.'])
        || local.contains("--")
        || local.contains("..")
    {
        anyhow::bail!("{local:?} is not a well-formed local part");
    }
    Ok(())
}

/// A part of a record's body, or a file attached to it.
///
/// `blobId` is a digest in the operator's file space, so an attachment is
/// already a public URL with `Range` and an immutable cache — there is no
/// second storage story for the bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyPart {
    pub blob_id: String,
    #[serde(rename = "type")]
    pub media_type: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    /// Where a reader fetches it.
    ///
    /// Filled when a record is read, never when it is stored: baking an origin
    /// into a stored record would outlive the domain it names. A caller's own
    /// value is discarded at post — a record is published under the operator's
    /// name, and an attachment link pointing anywhere they chose would carry
    /// that name with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Where a player fetches the HLS playlist for this attachment, once one
    /// exists.
    ///
    /// Filled on read like [`BodyPart::url`], and absent until segmenting has
    /// finished — a video is playable from `url` throughout, so its absence
    /// means "not segmented", never "not ready to watch". Never derived: a
    /// playlist is content-addressed by its own bytes, so its digest has no
    /// relation to the source's and must be read from the source's notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_url: Option<String>,
}

/// A record posted to one or more channels.
///
/// A subset of JMAP's `Email`, carrying the fields that mean something for a
/// published post. What is deliberately absent: `cc` and `bcc`, because a
/// broadcast has no copied recipients and a blind-copy list on a
/// world-readable record is a disclosure waiting to happen; and `$seen` and
/// `$flagged`, which are per-reader state that cannot live on a record every
/// reader shares.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// The stream sequence. Identity and ordering in one, so a permalink
    /// sorts.
    pub id: u64,
    /// Threading, for replies. Carried from the start because retrofitting an
    /// id onto records already published is not possible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Who posted. The operator today; an agent once feeds extend that far,
    /// which needs no schema change.
    pub from: String,
    /// The channels this was posted to. An array, so cross-posting is one
    /// record indexed twice rather than two records that can drift.
    pub to: Vec<String>,
    pub subject: String,
    pub received_at: String,
    pub size: u64,
    /// JMAP keywords. `$draft` is honoured; reader-state flags are not stored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_body: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<BodyPart>,
    /// The public key that signed the post, when the poster had one.
    ///
    /// A custom domain filters on keys rather than handles: a handle can be
    /// reassigned, and the filter would then admit whoever holds it today
    /// rather than whoever wrote the post. Absent on records written before
    /// this field existed, and on callers that carry no key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_by: Option<String>,
    /// RFC 5322 `Message-Id`, on a record that arrived as, or is served as,
    /// mail. What a reply names in `In-Reply-To`, and what `git am` keeps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// RFC 5322 `In-Reply-To`: the message this one answers, by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// RFC 5322 `References`: the thread's ids, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    /// The blog entry extension, on a record that is one.
    ///
    /// A vendor property, named the way RFC 8620 §1.8 has a vendor extend a
    /// standard type: a client that did not opt into
    /// [`crate::blog::BLOG_CAPABILITY`] sees an unknown property and renders
    /// the record as the message it also is.
    #[serde(
        rename = "noolog:blog",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub blog: Option<crate::blog::Blog>,
}

/// The keyword marking a record as not yet published.
pub const DRAFT: &str = "$draft";

impl Record {
    /// Whether this record is withheld from readers.
    pub fn is_draft(&self) -> bool {
        self.keywords.iter().any(|k| k == DRAFT)
    }
}

/// A channel, as registered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
/// A channel, which is an address and nothing else.
///
/// Not a resource anybody owns. A mailing list is not owned by whoever wrote
/// to it first, and neither is this: the address is a place to send to, it
/// comes into existence because somebody posted there, and no one names it or
/// describes it. What a reader sees is the business of whatever filters the
/// records for them, not of a title somebody claimed.
pub struct Channel {
    pub address: String,
    /// When the first record reached it. Not a claim on it.
    pub created_at: String,
}

pub mod blog;
#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
#[cfg(feature = "jmap")]
pub mod jmap;
pub mod mbox;
pub mod store;

pub use store::{FeedStore, MemoryFeedStore};

/// What the slug index holds for one entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlugEntry {
    pub id: u64,
    pub from: String,
}

/// A slug another operator holds in this channel.
///
/// Its own type so a handler can tell it from the broker being down: the
/// record was written, and the slug was not taken over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlugTaken {
    pub slug: String,
    pub address: String,
    pub held_by: String,
}

impl std::fmt::Display for SlugTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} in {} is already somebody else's entry",
            self.slug, self.address
        )
    }
}

impl std::error::Error for SlugTaken {}

/// Who holds `slug` in `address`, if anyone.
pub async fn slug_entry(
    store: &dyn FeedStore,
    address: &str,
    slug: &str,
) -> Result<Option<SlugEntry>> {
    Ok(store
        .slug(address, slug)
        .await
        .ok()
        .flatten()
        .map(|(entry, _)| entry))
}

/// The record a slug currently names in a channel.
///
/// The latest one: posting again under the same slug is how an entry is
/// revised, and the index follows the newest revision while every earlier one
/// stays reachable by its own id.
pub async fn get_by_slug(
    store: &dyn FeedStore,
    address: &str,
    slug: &str,
) -> Result<Option<Record>> {
    let Some(entry) = slug_entry(store, address, slug).await? else {
        return Ok(None);
    };
    get_record(store, address, entry.id).await
}

/// The record a mail `Message-Id` names in a channel.
///
/// The first one: the index is create-only, so a message delivered twice —
/// mail does that — names the record it made the first time.
pub async fn get_by_message_id(
    store: &dyn FeedStore,
    address: &str,
    message_id: &str,
) -> Result<Option<Record>> {
    let Some(id) = store.message_id(address, None, message_id).await? else {
        return Ok(None);
    };
    get_record(store, address, id).await
}

/// The record `from` sent under a mail `Message-Id` in a channel.
///
/// A `Message-Id` is the sender's own words, so two senders can use the same
/// one; this finds the one `from` sent, whoever used the id first. Records
/// indexed before ids were kept per sender are found through the first-claim
/// index when that record is theirs.
pub async fn get_by_sender_message_id(
    store: &dyn FeedStore,
    address: &str,
    from: &str,
    message_id: &str,
) -> Result<Option<Record>> {
    if let Some(id) = store.message_id(address, Some(from), message_id).await? {
        return get_record(store, address, id).await;
    }
    Ok(get_by_message_id(store, address, message_id)
        .await?
        .filter(|record| record.from == from))
}

/// Point a `Message-Id` at `record`, unless one already does: the first
/// claim for the channel, and the first claim by `record`'s sender.
async fn claim_message_id(
    store: &dyn FeedStore,
    address: &str,
    message_id: &str,
    record: &Record,
) -> Result<()> {
    store
        .create_message_id(address, None, message_id, record.id)
        .await?;
    store
        .create_message_id(address, Some(&record.from), message_id, record.id)
        .await
}

/// Register an address, if this is the first time anything was sent to it.
///
/// Idempotent and uncontested: there is nothing to conflict over, because the
/// record holds no claim — only the address and when it was first used.
pub async fn ensure_channel(store: &dyn FeedStore, channel: &Channel) -> Result<Channel> {
    validate_address(&channel.address)?;
    store.register_channel(channel).await?;
    Ok(channel.clone())
}

/// The channel registered at `address`, if any.
pub async fn get_channel(store: &dyn FeedStore, address: &str) -> Result<Option<Channel>> {
    store.channel(address).await
}

/// Every registered channel, for the public index.
pub async fn list_channels(store: &dyn FeedStore) -> Result<Vec<Channel>> {
    let mut channels = store.channels().await?;
    channels.sort_by(|a, b| a.address.cmp(&b.address));
    Ok(channels)
}

/// Append a record, returning it with the id it was given.
pub async fn post(store: &dyn FeedStore, address: &str, mut record: Record) -> Result<Record> {
    record.id = 0; // assigned by the store, never by the caller
    record.id = store.append(address, &record).await?;
    // Indexed after the write, never before: an index entry naming an id that
    // was never assigned would be a permalink to nothing. A draft is not
    // indexed at all — it is never served, and pointing the slug at it would
    // take the live revision off the air until the draft is published.
    if let Some(slug) = record.blog.as_ref().map(|b| b.slug.as_str())
        && !record.is_draft()
    {
        claim_slug(store, address, slug, &record).await?;
    }
    // Same rule for a message id: it points at the first record that carried
    // it, so a redelivered mail cannot move a thread.
    if let Some(message_id) = record.message_id.as_deref() {
        claim_message_id(store, address, message_id, &record).await?;
    }
    Ok(record)
}

/// Point `slug` at `record`, unless another operator holds it.
///
/// Compare-and-set on the index entry's revision, so two operators racing the
/// same slug cannot both win: the first write lands, the second sees an entry
/// that is not its own and fails with [`SlugTaken`]. A caller's earlier check
/// is a courtesy that keeps the common refusal from writing a record at all;
/// this is what makes the refusal hold.
async fn claim_slug(
    store: &dyn FeedStore,
    address: &str,
    slug: &str,
    record: &Record,
) -> Result<()> {
    let pointer = SlugEntry {
        id: record.id,
        from: record.from.clone(),
    };
    let written = match store.slug(address, slug).await? {
        None => store.create_slug(address, slug, &pointer).await?,
        Some((holder, _)) if holder.from != record.from => {
            return Err(refusal(address, slug, holder.from).into());
        }
        Some((_, revision)) => store.update_slug(address, slug, &pointer, revision).await?,
    };
    if written {
        Ok(())
    } else {
        verdict_of(store, address, slug, &record.from).await
    }
}

fn refusal(address: &str, slug: &str, held_by: String) -> SlugTaken {
    SlugTaken {
        slug: slug.to_string(),
        address: address.to_string(),
        held_by,
    }
}

/// After a compare-and-set lost, the index itself says who won: the caller's
/// own concurrent write, or somebody else's.
async fn verdict_of(store: &dyn FeedStore, address: &str, slug: &str, from: &str) -> Result<()> {
    match slug_entry(store, address, slug).await? {
        Some(now) if now.from == from => Ok(()),
        Some(now) => Err(refusal(address, slug, now.from).into()),
        None => Err(anyhow::anyhow!("the slug index refused the write")),
    }
}

/// One page of a channel, newest first.
///
/// `before` is exclusive, so paging is `before = the last id you saw`.
/// Drafts are left out: they are stored so an author can finish them, not so
/// a reader can find them.
pub async fn page(
    store: &dyn FeedStore,
    address: &str,
    before: Option<u64>,
    limit: usize,
) -> Result<Vec<Record>> {
    store.newest_before(address, before, limit).await
}

/// Records from id `first` on, oldest first, at most `limit`, drafts left out.
/// What a thread export reads: every reply has a higher id than the record
/// that started its thread, so nothing before `first` can belong to it.
pub async fn page_from(
    store: &dyn FeedStore,
    address: &str,
    first: u64,
    limit: usize,
) -> Result<Vec<Record>> {
    store.oldest_from(address, first, limit).await
}

/// One record by its id. A draft is not served.
pub async fn get_record(store: &dyn FeedStore, address: &str, id: u64) -> Result<Option<Record>> {
    Ok(store
        .record(address, id)
        .await?
        .filter(|record| !record.is_draft()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::a_record;

    /// Every conformance check against the in-memory reference store. The
    /// JetStream store runs the same checks in `nsed-content`.
    macro_rules! conformance {
        ($($name:ident),* $(,)?) => {
            mod memory {
                use anyhow::Result;
                $(
                    #[tokio::test]
                    async fn $name() -> Result<()> {
                        let store = super::super::MemoryFeedStore::default();
                        crate::conformance::$name(&store, "test-feed@noolog.io").await
                    }
                )*
            }
        };
    }

    conformance!(
        a_message_id_names_the_record_it_arrived_as_and_only_the_first,
        a_slug_names_the_newest_revision_and_older_ones_keep_their_ids,
        a_slug_cannot_be_taken_from_another_operator_even_in_a_race,
        records_from_an_id_come_back_oldest_first_and_bounded,
        records_come_back_newest_first_and_page_by_the_last_id_seen,
        an_id_is_assigned_by_the_channel_not_the_caller,
        a_draft_is_stored_but_never_served,
        anybody_may_post_to_a_channel_and_the_first_use_registers_it,
        an_empty_channel_is_an_empty_page_not_a_failure,
        the_index_lists_registered_channels,
    );

    #[test]
    fn an_address_is_the_channels_name_and_nothing_else() {
        // No operator prefix. A subject is the same subject whoever writes to
        // it, so prefixing it would give one channel as many addresses as it
        // had interested operators.
        assert_eq!(
            channel_address("showcase", "noolog.io").expect("an address"),
            "showcase@noolog.io"
        );
        // And the ambiguity the prefix used to carry is gone with it: there is
        // no longer any way for two different inputs to produce one address.
        assert_eq!(
            channel_address("acme-showcase", "noolog.io").expect("an address"),
            "acme-showcase@noolog.io"
        );
        assert!(channel_address("", "noolog.io").is_err());
        assert!(channel_address("   ", "noolog.io").is_err());
    }

    #[test]
    fn a_record_serialises_the_way_a_jmap_client_expects() {
        // camelCase, `type` on a body part, and no empty arrays for the
        // fields a broadcast does not use.
        let mut record = a_record("acme@noolog.io", "acme-showcase@noolog.io", "hello");
        record.attachments = vec![BodyPart {
            url: None,
            playlist_url: None,
            blob_id: "a".repeat(64),
            media_type: "video/mp4".into(),
            size: 1024,
            name: Some("clip.mp4".into()),
            disposition: Some("attachment".into()),
        }];
        let json = serde_json::to_value(&record).expect("serialise");

        assert_eq!(json["threadId"], serde_json::Value::Null);
        assert_eq!(json["receivedAt"], record.received_at);
        assert_eq!(json["textBody"], "the body of hello");
        assert_eq!(json["attachments"][0]["blobId"], "a".repeat(64));
        assert_eq!(json["attachments"][0]["type"], "video/mp4");
        assert!(
            json.get("cc").is_none() && json.get("bcc").is_none(),
            "a broadcast has no copied recipients: {json}"
        );
        assert!(
            json.get("keywords").is_none(),
            "empty flags are not carried"
        );
        assert!(
            json.get("noolog:blog").is_none(),
            "a record that is not a blog entry carries no extension"
        );

        // And it round-trips.
        let back: Record = serde_json::from_value(json).expect("deserialise");
        assert_eq!(back, record);
    }

    #[test]
    fn a_blog_entry_hangs_off_the_record_under_the_vendor_property() {
        // A JMAP client that never opted into the capability sees one unknown
        // property and the message it already knows how to render.
        let mut record = a_record("acme@noolog.io", "blog@noolog.io", "Introducing NSED");
        record.blog = Some(crate::blog::Blog::with_slug("introducing-nsed"));
        let json = serde_json::to_value(&record).expect("serialise");
        assert_eq!(json[crate::blog::BLOG]["slug"], "introducing-nsed");
        assert_eq!(json["subject"], "Introducing NSED");
        let back: Record = serde_json::from_value(json).expect("deserialise");
        assert_eq!(back, record);
    }

    #[test]
    fn awkward_names_still_yield_an_address_a_mail_server_would_accept() {
        for name in [
            "Acme Corp",
            "acme..corp",
            "  spaced  ",
            "café",
            "--weird--",
            "UPPER",
        ] {
            let address = channel_address(name, "noolog.io").unwrap();
            validate_address(&address).unwrap_or_else(|e| panic!("{name:?} -> {address:?}: {e}"));
            assert!(!address.contains("--"), "{address}");
            assert!(
                !address.starts_with('-') && !address.contains("-@"),
                "{address}"
            );
        }
    }

    #[test]
    fn an_address_this_deployment_would_not_issue_is_refused() {
        for bad in [
            "no-domain",
            "@noolog.io",
            "acme@",
            "acme@nodot",
            "ACME@noolog.io",
            "acme..x@noolog.io",
            "-acme@noolog.io",
            "acme-@noolog.io",
            "acme--x@noolog.io",
            "a b@noolog.io",
            "acme+tag@noolog.io",
        ] {
            assert!(
                validate_address(bad).is_err(),
                "{bad:?} should not be accepted"
            );
        }
        assert!(validate_address("acme-showcase-list@noolog.io").is_ok());
    }

    #[test]
    fn an_address_cannot_exceed_what_a_mail_server_accepts() {
        let long_name = "x".repeat(80);
        let address = channel_address(&long_name, "noolog.io");
        assert!(
            address.is_err(),
            "a 80-character local part must be refused"
        );
    }
}
