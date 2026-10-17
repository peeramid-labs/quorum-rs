//! Records, channels and their ids as JMAP types (feature `jmap`).
//!
//! Channel ↔ `Mailbox`, record ↔ `Email`, thread ↔ `Thread`. Ids use the JMAP
//! `Id` alphabet and begin with a letter (RFC 8620 §1.2):
//!
//! | Object | Id |
//! |---|---|
//! | Mailbox | `m` + the channel address in unpadded base64url |
//! | Email | the mailbox id, `-`, the record id |
//! | Thread | `t` + the address in base64url, `-`, the record id of the thread's root |

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jmap_core::{Extra, Id};
use jmap_mail::{Email, EmailAddress, EmailBodyPart, EmailBodyValue, Mailbox, MailboxRights};

use crate::blog::{BLOG, BLOG_CAPABILITY};
use crate::{Channel, Record};

/// `Email/query` vendor condition: only record ids below this one.
pub const BEFORE: &str = "noolog:before";
/// `Email/query` vendor condition: the entry a blog slug names.
pub const SLUG: &str = "noolog:slug";

/// Characters a preview carries (RFC 8621 §4.1.4).
const PREVIEW_CHARS: usize = 256;

fn id(raw: String) -> Id {
    // Letters, base64url and decimal digits only, and never empty: valid by
    // construction. Addresses are bounded by `validate_address`.
    Id::new(raw).unwrap_or_else(|e| unreachable!("{e}"))
}

/// The `Mailbox` id of the channel at `address`.
pub fn mailbox_id(address: &str) -> Id {
    id(format!("m{}", URL_SAFE_NO_PAD.encode(address)))
}

/// The channel address a `Mailbox` id names, if it is one.
pub fn parse_mailbox_id(raw: &str) -> Option<String> {
    let encoded = raw.strip_prefix('m')?;
    String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()
}

/// The `Email` id of record `record_id` in the channel at `address`.
pub fn email_id(address: &str, record_id: u64) -> Id {
    id(format!("{}-{record_id}", mailbox_id(address)))
}

/// `(address, record id)` from an `Email` id, if it is one.
pub fn parse_email_id(raw: &str) -> Option<(String, u64)> {
    let (mailbox, record) = raw.rsplit_once('-')?;
    Some((parse_mailbox_id(mailbox)?, record.parse().ok()?))
}

/// The `Thread` id of the thread rooted at record `root` in `address`.
pub fn thread_id(address: &str, root: u64) -> Id {
    id(format!("t{}-{root}", URL_SAFE_NO_PAD.encode(address)))
}

/// `(address, root record id)` from a `Thread` id, if it is one.
pub fn parse_thread_id(raw: &str) -> Option<(String, u64)> {
    parse_email_id(&format!("m{}", raw.strip_prefix('t')?))
}

/// A channel as a read-only `Mailbox`. Counts are not tracked yet and read 0.
pub fn to_mailbox(channel: &Channel) -> Mailbox {
    Mailbox {
        id: mailbox_id(&channel.address),
        name: channel.address.clone(),
        parent_id: None,
        role: None,
        sort_order: 0,
        total_emails: 0,
        unread_emails: 0,
        total_threads: 0,
        unread_threads: 0,
        my_rights: MailboxRights {
            may_read_items: true,
            ..MailboxRights::default()
        },
        is_subscribed: false,
        extra: Extra::new(),
    }
}

/// `record`, stored in the channel at `address`, as an `Email`. Vendor
/// properties appear only for capabilities in `using` (RFC 8620 §1.8).
pub fn to_email(address: &str, record: &Record, using: &[&str]) -> Email {
    let mut values = std::collections::BTreeMap::new();
    let mut part = |part_id: &str, media_type: &str, body: &Option<String>| {
        body.as_ref().map(|value| {
            values.insert(
                part_id.to_string(),
                EmailBodyValue {
                    value: value.clone(),
                    is_encoding_problem: false,
                    is_truncated: false,
                },
            );
            EmailBodyPart {
                part_id: Some(part_id.into()),
                size: Some(value.len() as u64),
                media_type: Some(media_type.into()),
                charset: Some("utf-8".into()),
                ..EmailBodyPart::default()
            }
        })
    };
    let text = part("text", "text/plain", &record.text_body);
    let html = part("html", "text/html", &record.html_body);
    // Each list falls back to the other's part when its own type is absent
    // (RFC 8621 §4.1.4).
    let text_body: Vec<_> = text.clone().or_else(|| html.clone()).into_iter().collect();
    let html_body: Vec<_> = html.or(text).into_iter().collect();

    let mut extra = Extra::new();
    if using.contains(&BLOG_CAPABILITY)
        && let Some(blog) = record
            .blog
            .as_ref()
            .and_then(|b| serde_json::to_value(b).ok())
    {
        extra.insert(BLOG.into(), blog);
    }
    let root = record
        .thread_id
        .as_deref()
        .and_then(|t| t.parse().ok())
        .unwrap_or(record.id);

    Email {
        id: Some(email_id(address, record.id)),
        thread_id: Some(thread_id(address, root)),
        mailbox_ids: Some([(mailbox_id(address), true)].into()),
        keywords: Some(record.keywords.iter().map(|k| (k.clone(), true)).collect()),
        size: Some(record.size),
        received_at: Some(utc_date(&record.received_at)),
        message_id: record.message_id.as_deref().map(|m| vec![bare(m)]),
        in_reply_to: record.in_reply_to.as_deref().map(|m| vec![bare(m)]),
        references: (!record.references.is_empty())
            .then(|| record.references.iter().map(|m| bare(m)).collect()),
        from: Some(vec![address_of(&record.from)]),
        to: Some(record.to.iter().map(|t| address_of(t)).collect()),
        subject: Some(record.subject.clone()),
        preview: Some(preview(record.text_body.as_deref().unwrap_or(""))),
        body_values: Some(values),
        text_body: Some(text_body),
        html_body: Some(html_body),
        has_attachment: Some(!record.attachments.is_empty()),
        attachments: Some(
            record
                .attachments
                .iter()
                .map(|a| EmailBodyPart {
                    blob_id: Id::new(a.blob_id.clone()).ok(),
                    size: Some(a.size),
                    name: a.name.clone(),
                    media_type: Some(a.media_type.clone()),
                    disposition: a.disposition.clone(),
                    ..EmailBodyPart::default()
                })
                .collect(),
        ),
        extra,
        ..Email::default()
    }
}

/// `Name <addr>` or `addr` as an `EmailAddress`.
pub fn address_of(raw: &str) -> EmailAddress {
    match raw
        .trim()
        .strip_suffix('>')
        .and_then(|s| s.rsplit_once('<'))
    {
        Some((name, email)) => {
            let name = name.trim().trim_matches('"').trim();
            EmailAddress {
                name: (!name.is_empty()).then(|| name.to_string()),
                email: email.trim().to_string(),
            }
        }
        None => EmailAddress {
            name: None,
            email: raw.trim().to_string(),
        },
    }
}

/// A message id without its angle brackets (RFC 8621 §4.1.2.5).
fn bare(message_id: &str) -> String {
    let id = message_id.trim();
    id.strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(id)
        .to_string()
}

/// RFC 3339 with any offset, as a `UTCDate` (`Z`, no zero fraction). An
/// unparseable stamp passes through.
fn utc_date(stamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .map(|d| {
            d.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
        })
        .unwrap_or_else(|_| stamp.to_string())
}

fn preview(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(PREVIEW_CHARS)
        .collect()
}
