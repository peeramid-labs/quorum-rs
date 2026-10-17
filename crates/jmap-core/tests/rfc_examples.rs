//! The examples printed in RFC 8620 parse into these types and serialize back
//! to the same JSON.

use jmap_core::{CORE, Id, Invocation, MethodError, Request, Response, Session};
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
    assert_eq!(request.using, [CORE, "urn:ietf:params:jmap:mail"]);
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
    assert_eq!(
        session.primary_accounts["urn:ietf:params:jmap:mail"].as_str(),
        "A13824"
    );
}
