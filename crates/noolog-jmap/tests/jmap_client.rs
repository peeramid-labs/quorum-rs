//! What we serialize, Stalwart's `jmap-client` parses: an implementation we
//! did not write judges the wire format.

use std::collections::BTreeMap;

use noolog_jmap::{
    Email, EmailAddress, EmailBodyPart, EmailBodyValue, GetResponse, Id, Mailbox, MailboxRights,
    Thread,
};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}

fn an_email() -> Email {
    let mut body = BTreeMap::new();
    body.insert(
        "text".to_string(),
        EmailBodyValue {
            value: "the body".into(),
            is_encoding_problem: false,
            is_truncated: false,
        },
    );
    Email {
        id: Some(id("Ym94QG5vb2xvZy5pbw-7")),
        thread_id: Some(id("Ym94QG5vb2xvZy5pbw-3")),
        mailbox_ids: Some([(id("Ym94QG5vb2xvZy5pbw"), true)].into()),
        keywords: Some([("$seen".to_string(), true)].into()),
        size: Some(8),
        received_at: Some("2026-10-17T10:00:00Z".into()),
        message_id: Some(vec!["m1@noolog.io".into()]),
        in_reply_to: Some(vec!["m0@noolog.io".into()]),
        from: Some(vec![EmailAddress {
            name: Some("Acme".into()),
            email: "acme@noolog.io".into(),
        }]),
        to: Some(vec![EmailAddress {
            name: None,
            email: "box@noolog.io".into(),
        }]),
        subject: Some("[PATCH 1/2] a change".into()),
        body_values: Some(body),
        text_body: Some(vec![EmailBodyPart {
            part_id: Some("text".into()),
            size: Some(8),
            media_type: Some("text/plain".into()),
            charset: Some("utf-8".into()),
            ..Default::default()
        }]),
        attachments: Some(vec![EmailBodyPart {
            blob_id: Some(id("Gdeadbeef")),
            size: Some(1024),
            name: Some("clip.mp4".into()),
            media_type: Some("video/mp4".into()),
            disposition: Some("attachment".into()),
            ..Default::default()
        }]),
        has_attachment: Some(true),
        preview: Some("the body".into()),
        ..Default::default()
    }
}

/// No vendor properties: RFC 8620 §1.8 has a server show them only to a client
/// that opted into their capability, and `jmap-client` knows none of ours (it
/// reads any unknown `name:form` property as a header form and fails).
#[test]
fn an_email_parses_as_jmap_client_reads_it() {
    let wire = serde_json::to_value(an_email()).unwrap();
    let theirs: jmap_client::email::Email =
        serde_json::from_value(wire).expect("jmap-client parses it");
    assert_eq!(theirs.id(), Some("Ym94QG5vb2xvZy5pbw-7"));
    assert_eq!(theirs.thread_id(), Some("Ym94QG5vb2xvZy5pbw-3"));
    assert_eq!(theirs.mailbox_ids(), ["Ym94QG5vb2xvZy5pbw"]);
    assert_eq!(theirs.subject(), Some("[PATCH 1/2] a change"));
    assert_eq!(theirs.from().unwrap()[0].email(), "acme@noolog.io");
    assert_eq!(theirs.from().unwrap()[0].name(), Some("Acme"));
    assert_eq!(theirs.message_id().unwrap(), ["m1@noolog.io"]);
    assert_eq!(theirs.in_reply_to().unwrap(), ["m0@noolog.io"]);
    assert!(
        theirs.received_at().is_some(),
        "receivedAt is a UTCDate it can read"
    );
    assert_eq!(theirs.text_body().unwrap()[0].part_id(), Some("text"));
    assert_eq!(theirs.body_value("text").unwrap().value(), "the body");
    assert_eq!(theirs.attachments().unwrap()[0].name(), Some("clip.mp4"));
    assert!(theirs.has_attachment());
    assert_eq!(theirs.preview(), Some("the body"));
}

#[test]
fn a_mailbox_and_a_thread_parse_as_jmap_client_reads_them() {
    let mailbox = Mailbox {
        id: id("Ym94QG5vb2xvZy5pbw"),
        name: "box@noolog.io".into(),
        parent_id: None,
        role: None,
        sort_order: 0,
        total_emails: 3,
        unread_emails: 0,
        total_threads: 2,
        unread_threads: 0,
        my_rights: MailboxRights {
            may_read_items: true,
            ..Default::default()
        },
        is_subscribed: false,
        extra: Default::default(),
    };
    let theirs: jmap_client::mailbox::Mailbox =
        serde_json::from_value(serde_json::to_value(&mailbox).unwrap()).expect("parses");
    assert_eq!(theirs.name(), Some("box@noolog.io"));
    assert_eq!(theirs.total_emails(), 3);

    let got = GetResponse {
        account_id: id("public"),
        state: "0".into(),
        list: vec![Thread {
            id: id("t1"),
            email_ids: vec![id("e1"), id("e2")],
        }],
        not_found: vec![],
    };
    let theirs: jmap_client::core::response::ThreadGetResponse =
        serde_json::from_value(serde_json::to_value(&got).unwrap()).expect("parses");
    assert_eq!(theirs.list()[0].email_ids(), ["e1", "e2"]);
}
