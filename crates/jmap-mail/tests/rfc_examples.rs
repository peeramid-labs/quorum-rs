//! The examples printed in RFC 8621 parse into these types and serialize back
//! to the same JSON.

use jmap_core::GetResponse;
use jmap_mail::{Email, Thread};
use serde_json::{Value, json};

fn round_trips<T: serde::de::DeserializeOwned + serde::Serialize>(example: &Value) -> T {
    let parsed: T = serde_json::from_value(example.clone()).expect("parses");
    assert_eq!(
        &serde_json::to_value(&parsed).unwrap(),
        example,
        "serializes back unchanged"
    );
    parsed
}

/// RFC 8621 §3.1.1.
#[test]
fn the_example_thread_get_response() {
    let example = json!({
      "accountId": "acme",
      "state": "f6a7e214",
      "list": [
        { "id": "f123u4", "emailIds": [ "eaa623", "f782cbb"] },
        { "id": "f41u44", "emailIds": [ "82cf7bb" ] }
      ],
      "notFound": []
    });
    let got: GetResponse<Thread> = round_trips(&example);
    assert_eq!(got.list[0].email_ids.len(), 2);
}

/// RFC 8621 §4.2.1: a property subset, a header form kept as an extra
/// property, and body values.
#[test]
fn the_example_email_get_response() {
    let example = json!({
      "accountId": "abc",
      "state": "41234123231",
      "list": [
        {
          "id": "f123u457",
          "threadId": "ef1314a",
          "mailboxIds": { "f123": true },
          "from": [{ "name": "Joe Bloggs", "email": "joe@example.com" }],
          "subject": "Dinner on Thursday?",
          "receivedAt": "2013-10-13T14:12:00Z",
          "header:List-POST:asURLs": [ "mailto:partytime@lists.example.com" ],
          "htmlBody": [
            { "partId": "1", "blobId": "B841623871", "size": 283331, "type": "text/html" },
            { "partId": "2", "blobId": "B319437193", "size": 10343, "type": "text/plain" }
          ],
          "bodyValues": {
            "1": { "isEncodingProblem": false, "isTruncated": true, "value": "<html><body><p>Hello ..." },
            "2": { "isEncodingProblem": false, "isTruncated": false, "value": "-- Sent by your friendly mailing list ..." }
          }
        }
      ],
      "notFound": [ "f123u456" ]
    });
    let got: GetResponse<Email> = round_trips(&example);
    let email = &got.list[0];
    assert_eq!(email.from.as_ref().unwrap()[0].email, "joe@example.com");
    assert!(email.extra.contains_key("header:List-POST:asURLs"));
    assert!(email.body_values.as_ref().unwrap()["1"].is_truncated);
    assert_eq!(got.not_found[0].as_str(), "f123u456");
}

/// RFC 8621 §4.4.1 conditions under an RFC 8620 §5.5 operator, with a vendor
/// condition kept aside.
#[test]
fn an_email_filter_nests_and_keeps_vendor_conditions() {
    let filter: jmap_core::Filter<jmap_mail::EmailFilterCondition> =
        serde_json::from_value(json!({
            "operator": "AND",
            "conditions": [ { "inMailbox": "m1" }, { "hasKeyword": "$flagged", "vendor:tag": "x" } ]
        }))
        .unwrap();
    let jmap_core::Filter::Operator { conditions, .. } = filter else {
        panic!("an operator")
    };
    let jmap_core::Filter::Condition(second) = &conditions[1] else {
        panic!("a condition")
    };
    assert_eq!(second.has_keyword.as_deref(), Some("$flagged"));
    assert_eq!(second.extra["vendor:tag"], "x");
}
