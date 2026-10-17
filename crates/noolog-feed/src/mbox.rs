//! A record as mail, and a thread as an mbox.
//!
//! The archive is read by `git am` as readily as by a browser: a record that
//! arrived as a patch goes back out as the same message, headers intact, so
//! `curl … | git am` applies what `git send-email` posted. A record that was
//! never mail still renders — it gets an id of its own making, stable for the
//! deployment — because a reader following a thread wants all of it.

use crate::Record;

/// The record as one RFC 5322 message.
pub fn rfc5322(record: &Record, address: &str) -> String {
    let mut out = String::new();
    push(&mut out, "From", &record.from);
    push(&mut out, "To", &record.to.join(", "));
    push(&mut out, "Subject", &record.subject);
    push(&mut out, "Date", &rfc2822(&record.received_at));
    push(&mut out, "Message-ID", &message_id(record, address));
    if let Some(parent) = record.in_reply_to.as_deref().filter(|s| !s.is_empty()) {
        push(&mut out, "In-Reply-To", parent);
    }
    if !record.references.is_empty() {
        push(&mut out, "References", &record.references.join(" "));
    }
    push(&mut out, "MIME-Version", "1.0");
    push(&mut out, "Content-Type", "text/plain; charset=utf-8");
    push(&mut out, "Content-Transfer-Encoding", "8bit");
    out.push('\n');
    out.push_str(record.text_body.as_deref().unwrap_or(""));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Several records as one mbox, in the order given.
///
/// `mboxrd` quoting: a body line beginning `From ` is prefixed with `>`, and
/// one already so prefixed gains another, so the separator is unambiguous
/// and a reader that unquotes gets the body back exactly.
pub fn mbox(records: &[Record], address: &str) -> String {
    let mut out = String::new();
    for record in records {
        let sender = envelope_sender(&record.from);
        out.push_str(&format!("From {sender} {}\n", asctime(&record.received_at)));
        for line in rfc5322(record, address).lines() {
            if line.starts_with("From ") || is_quoted_from(line) {
                out.push('>');
            }
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// The id a record is known by in mail. Its own when it arrived as mail;
/// otherwise one built from where it lives, so a thread of web posts still
/// threads once it is read as mail.
pub fn message_id(record: &Record, address: &str) -> String {
    match record.message_id.as_deref().filter(|s| !s.is_empty()) {
        Some(id) => id.to_string(),
        None => {
            let (local, domain) = address.split_once('@').unwrap_or((address, "noolog"));
            format!("<{}.{}@{domain}>", record.id, local)
        }
    }
}

fn push(out: &mut String, name: &str, value: &str) {
    // A header value holds no line breaks; a subject that had one would
    // otherwise inject a header of its own.
    let flat: String = value
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();
    out.push_str(name);
    out.push_str(": ");
    out.push_str(flat.trim());
    out.push('\n');
}

/// RFC 3339 in, RFC 2822 out; an unparseable stamp is passed through, since
/// a wrong `Date` is better than none for `git am`.
fn rfc2822(received_at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(received_at)
        .map(|d| d.to_rfc2822())
        .unwrap_or_else(|_| received_at.to_string())
}

/// The `From ` separator's timestamp, in asctime form.
fn asctime(received_at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(received_at)
        .map(|d| d.format("%a %b %e %H:%M:%S %Y").to_string())
        .unwrap_or_else(|_| "Thu Jan  1 00:00:00 1970".to_string())
}

fn envelope_sender(from: &str) -> &str {
    from.rsplit_once('<')
        .and_then(|(_, rest)| rest.strip_suffix('>'))
        .unwrap_or(from)
        .trim()
}

fn is_quoted_from(line: &str) -> bool {
    line.trim_start_matches('>').starts_with("From ") && line.starts_with('>')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: u64, subject: &str, body: &str) -> Record {
        Record {
            id,
            thread_id: None,
            from: "dev@noolog.io".into(),
            to: vec!["acme@noolog.io".into()],
            subject: subject.into(),
            received_at: "2026-09-28T10:00:00Z".into(),
            size: body.len() as u64,
            keywords: vec![],
            text_body: Some(body.into()),
            html_body: None,
            attachments: vec![],
            signed_by: None,
            message_id: None,
            in_reply_to: None,
            references: vec![],
            blog: None,
        }
    }

    #[test]
    fn a_mailed_record_goes_back_out_with_its_own_headers() {
        let mut r = record(3, "[PATCH 1/2] change", "diff --git a/f b/f\n");
        r.message_id = Some("<p1@x>".into());
        r.in_reply_to = Some("<cover@x>".into());
        r.references = vec!["<cover@x>".into()];
        let text = rfc5322(&r, "acme@noolog.io");
        assert!(text.contains("Message-ID: <p1@x>\n"), "{text}");
        assert!(text.contains("In-Reply-To: <cover@x>\n"));
        assert!(text.contains("References: <cover@x>\n"));
        assert!(text.contains("Date: Mon, 28 Sep 2026 10:00:00 +0000\n"));
        assert!(text.ends_with("\n\ndiff --git a/f b/f\n"), "{text:?}");
    }

    #[test]
    fn a_web_post_is_given_a_stable_id_from_where_it_lives() {
        let r = record(7, "hello", "hi");
        assert_eq!(message_id(&r, "acme@noolog.io"), "<7.acme@noolog.io>");
        let text = rfc5322(&r, "acme@noolog.io");
        assert!(!text.contains("In-Reply-To"), "no parent, no header");
        assert!(!text.contains("References"));
    }

    #[test]
    fn a_header_value_cannot_smuggle_a_header() {
        let r = record(1, "innocent\nX-Evil: yes", "");
        let text = rfc5322(&r, "acme@noolog.io");
        assert!(
            !text.contains("\nX-Evil:"),
            "a line of its own would be a header: {text}"
        );
        assert!(text.contains("Subject: innocent X-Evil: yes\n"));
    }

    #[test]
    fn an_mbox_separates_messages_and_quotes_body_lines_that_look_like_one() {
        let a = record(1, "one", "From the top\nbody\n");
        let b = record(2, "two", ">From quoted\n");
        let text = mbox(&[a, b], "acme@noolog.io");
        assert_eq!(
            text.matches("\nFrom dev@noolog.io ").count()
                + text.starts_with("From dev@noolog.io ") as usize,
            2
        );
        assert!(text.contains("\n>From the top\n"), "{text}");
        assert!(text.contains("\n>>From quoted\n"), "{text}");
        assert!(text.starts_with("From dev@noolog.io Mon Sep 28 10:00:00 2026\n"));
    }
}
