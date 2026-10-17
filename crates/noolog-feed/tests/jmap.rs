//! A record as a JMAP Email, a channel as a Mailbox, and the ids that name
//! them (feature `jmap`).

#![cfg(all(feature = "jmap", feature = "conformance"))]

use noolog_feed::blog::{BLOG, BLOG_CAPABILITY, Blog};
use noolog_feed::conformance::{a_channel, a_record};
use noolog_feed::jmap::{
    email_id, mailbox_id, parse_email_id, parse_mailbox_id, parse_thread_id, thread_id, to_email,
    to_mailbox,
};
use noolog_feed::{BodyPart, Record};

const A: &str = "acme-list@noolog.io";

fn a_post() -> Record {
    let mut r = a_record("Acme Corp <acme@noolog.io>", A, "[PATCH 1/2] change");
    r.id = 7;
    r.thread_id = Some("3".into());
    r.received_at = "2026-10-17T10:00:00+00:00".into();
    r.keywords = vec!["$seen".into()];
    r.message_id = Some("<m7@noolog.io>".into());
    r.in_reply_to = Some("<m3@noolog.io>".into());
    r.references = vec!["<m1@noolog.io>".into(), "<m3@noolog.io>".into()];
    r.attachments = vec![BodyPart {
        blob_id: "ab12".into(),
        media_type: "video/mp4".into(),
        size: 1024,
        name: Some("clip.mp4".into()),
        disposition: Some("attachment".into()),
        url: Some("https://x/content/acme/ab12".into()),
        playlist_url: None,
    }];
    r.blog = Some(Blog::with_slug("change"));
    r
}

#[test]
fn ids_are_jmap_ids_that_start_with_a_letter_and_round_trip() {
    for address in [A, "0digit@x", "-dash@x", "_u@x"] {
        let m = mailbox_id(address);
        let e = email_id(address, 42);
        for id in [&m, &e, &thread_id(address, 3)] {
            assert!(
                id.as_str().starts_with(|c: char| c.is_ascii_alphabetic()),
                "{id}"
            );
        }
        assert_eq!(parse_mailbox_id(m.as_str()).as_deref(), Some(address));
        assert_eq!(parse_email_id(e.as_str()), Some((address.to_string(), 42)));
        assert_eq!(
            parse_thread_id(thread_id(address, 3).as_str()),
            Some((address.to_string(), 3))
        );
    }
    assert_eq!(
        parse_thread_id(email_id(A, 1).as_str()),
        None,
        "an email id is not a thread id"
    );
    assert_eq!(parse_email_id("junk"), None);
    assert_eq!(parse_mailbox_id("x!"), None);
}

#[test]
fn a_record_maps_onto_the_rfc_email_properties() {
    let email = to_email(A, &a_post(), &[]);
    assert_eq!(email.id, Some(email_id(A, 7)));
    assert_eq!(email.thread_id, Some(thread_id(A, 3)));
    assert!(email.mailbox_ids.as_ref().unwrap()[&mailbox_id(A)]);
    assert!(email.keywords.as_ref().unwrap()["$seen"]);
    assert_eq!(
        email.received_at.as_deref(),
        Some("2026-10-17T10:00:00Z"),
        "UTCDate ends in Z"
    );
    assert_eq!(
        email.message_id,
        Some(vec!["m7@noolog.io".to_string()]),
        "no angle brackets"
    );
    assert_eq!(email.in_reply_to, Some(vec!["m3@noolog.io".to_string()]));
    assert_eq!(email.references.as_ref().unwrap().len(), 2);
    let from = &email.from.as_ref().unwrap()[0];
    assert_eq!(
        (from.name.as_deref(), from.email.as_str()),
        (Some("Acme Corp"), "acme@noolog.io")
    );
    assert_eq!(email.to.as_ref().unwrap()[0].name, None);
    assert_eq!(email.subject.as_deref(), Some("[PATCH 1/2] change"));
    let text = &email.text_body.as_ref().unwrap()[0];
    let body = &email.body_values.as_ref().unwrap()[text.part_id.as_ref().unwrap()];
    assert_eq!(body.value, "the body of [PATCH 1/2] change");
    assert_eq!(
        email.html_body, email.text_body,
        "no html: htmlBody falls back to the text part"
    );
    let attachment = &email.attachments.as_ref().unwrap()[0];
    assert_eq!(
        attachment.blob_id.as_ref().map(|b| b.as_str()),
        Some("ab12")
    );
    assert_eq!(email.has_attachment, Some(true));
    assert_eq!(
        email.preview.as_deref(),
        Some("the body of [PATCH 1/2] change")
    );
}

#[test]
fn a_vendor_property_is_shown_only_to_a_client_that_opted_in() {
    assert!(!to_email(A, &a_post(), &[]).extra.contains_key(BLOG));
    let opted = to_email(A, &a_post(), &[BLOG_CAPABILITY]);
    assert_eq!(opted.extra[BLOG]["slug"], "change");
}

#[test]
fn an_html_only_record_fills_both_body_lists_with_its_html_part() {
    let mut r = a_post();
    r.text_body = None;
    r.html_body = Some("<p>hi</p>".into());
    let email = to_email(A, &r, &[]);
    assert_eq!(
        email.text_body.as_ref().unwrap()[0].media_type.as_deref(),
        Some("text/html")
    );
    assert_eq!(email.html_body, email.text_body);
    assert_eq!(email.preview.as_deref(), Some(""), "no text to preview");
}

#[test]
fn a_record_that_started_its_thread_is_its_own_thread() {
    let mut r = a_post();
    r.thread_id = None;
    assert_eq!(to_email(A, &r, &[]).thread_id, Some(thread_id(A, 7)));
}

#[test]
fn a_channel_is_a_read_only_mailbox_named_by_its_address() {
    let mailbox = to_mailbox(&a_channel(A));
    assert_eq!(mailbox.id, mailbox_id(A));
    assert_eq!(mailbox.name, A);
    assert!(mailbox.my_rights.may_read_items && !mailbox.my_rights.may_add_items);
}

#[test]
fn jmap_client_parses_a_mapped_record_and_mailbox() {
    let wire = serde_json::to_value(to_email(A, &a_post(), &[])).unwrap();
    let theirs: jmap_client::email::Email = serde_json::from_value(wire).expect("parses");
    assert_eq!(theirs.subject(), Some("[PATCH 1/2] change"));
    assert_eq!(theirs.from().unwrap()[0].name(), Some("Acme Corp"));
    assert!(theirs.received_at().is_some());
    let mailbox = serde_json::to_value(to_mailbox(&a_channel(A))).unwrap();
    let theirs: jmap_client::mailbox::Mailbox = serde_json::from_value(mailbox).expect("parses");
    assert_eq!(theirs.name(), Some(A));
}
