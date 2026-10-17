//! The examples printed in RFC 8620 and RFC 8621 parse into these types and
//! serialize back to the same JSON.

use noolog_jmap::{
    CORE, Email, GetResponse, Id, Invocation, MAIL, MethodError, Request, Response, Session, Thread,
};
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

/// RFC 8620 §3.3.1.
#[test]
fn the_example_request() {
    let example = json!({
      "using": [ "urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail" ],
      "methodCalls": [
        [ "method1", { "arg1": "arg1data", "arg2": "arg2data" }, "c1" ],
        [ "method2", { "arg1": "arg1data" }, "c2" ],
        [ "method3", {}, "c3" ]
      ]
    });
    let request: Request = round_trips(&example);
    assert_eq!(request.using, [CORE, MAIL]);
    assert_eq!(request.method_calls[1].name, "method2");
    assert_eq!(request.method_calls[1].call_id, "c2");
    assert_eq!(request.method_calls[1].arguments["arg1"], "arg1data");
}

/// RFC 8620 §3.4.1, whose last response is a method-level error (§3.6.2).
#[test]
fn the_example_response_and_its_error() {
    let example = json!({
      "methodResponses": [
        [ "method1", { "arg1": 3, "arg2": "foo" }, "c1" ],
        [ "method2", { "isBlah": true }, "c2" ],
        [ "anotherResponseFromMethod2", { "data": 10, "yetmoredata": "Hello" }, "c2"],
        [ "error", { "type":"unknownMethod" }, "c3" ]
      ],
      "sessionState": "75128aab4b1b"
    });
    let response: Response = round_trips(&example);
    let error = &response.method_responses[3];
    let parsed: MethodError =
        serde_json::from_value(Value::Object(error.arguments.clone())).unwrap();
    assert_eq!(parsed.kind, "unknownMethod");
    assert_eq!(
        Invocation::error("c9", MethodError::new("serverFail")).name,
        "error"
    );
}

/// RFC 8620 §2.1, repaired as printed text cannot be JSON: the comma after
/// `"urn:ietf:params:jmap:mail": {}`, the `...` placeholders and the wrapped
/// URLs.
#[test]
fn the_example_session() {
    let example = json!({
      "capabilities": {
        "urn:ietf:params:jmap:core": {
          "maxSizeUpload": 50000000,
          "maxConcurrentUpload": 8,
          "maxSizeRequest": 10000000,
          "maxConcurrentRequests": 8,
          "maxCallsInRequest": 32,
          "maxObjectsInGet": 256,
          "maxObjectsInSet": 128,
          "collationAlgorithms": [ "i;ascii-numeric", "i;ascii-casemap", "i;unicode-casemap" ]
        },
        "urn:ietf:params:jmap:mail": {},
        "urn:ietf:params:jmap:contacts": {},
        "https://example.com/apis/foobar": { "maxFoosFinangled": 42 }
      },
      "accounts": {
        "A13824": {
          "name": "john@example.com",
          "isPersonal": true,
          "isReadOnly": false,
          "accountCapabilities": {
            "urn:ietf:params:jmap:mail": { "maxMailboxesPerEmail": null, "maxMailboxDepth": 10 },
            "urn:ietf:params:jmap:contacts": {}
          }
        },
        "A97813": {
          "name": "jane@example.com",
          "isPersonal": false,
          "isReadOnly": true,
          "accountCapabilities": {
            "urn:ietf:params:jmap:mail": { "maxMailboxesPerEmail": 1, "maxMailboxDepth": 10 }
          }
        }
      },
      "primaryAccounts": {
        "urn:ietf:params:jmap:mail": "A13824",
        "urn:ietf:params:jmap:contacts": "A13824"
      },
      "username": "john@example.com",
      "apiUrl": "https://jmap.example.com/api/",
      "downloadUrl": "https://jmap.example.com/download/{accountId}/{blobId}/{name}?accept={type}",
      "uploadUrl": "https://jmap.example.com/upload/{accountId}/",
      "eventSourceUrl": "https://jmap.example.com/eventsource/?types={types}&closeafter={closeafter}&ping={ping}",
      "state": "75128aab4b1b"
    });
    let session: Session = round_trips(&example);
    assert_eq!(session.core().unwrap().max_objects_in_get, 256);
    let shared = &session.accounts[&Id::new("A97813").unwrap()];
    assert!(shared.is_read_only && !shared.is_personal);
    assert_eq!(session.primary_accounts[MAIL].as_str(), "A13824");
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
