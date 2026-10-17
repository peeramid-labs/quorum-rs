//! Id validation (RFC 8620 §1.2) and the standard method arguments (§5).

use noolog_jmap::{
    Comparator, Email, EmailFilterCondition, Filter, Id, QueryRequest, SetError, SetRequest,
};
use serde_json::json;

#[test]
fn an_id_is_one_to_255_characters_of_the_url_safe_alphabet() {
    for ok in ["a", "A-z_09", &"x".repeat(255)] {
        assert!(Id::new(ok).is_ok(), "{ok}");
    }
    for bad in ["", "a=b", "a/b", "a@b", "é", &"x".repeat(256)] {
        assert!(Id::new(bad).is_err(), "{bad}");
        assert!(serde_json::from_value::<Id>(json!(bad)).is_err(), "{bad}");
    }
}

#[test]
fn a_query_takes_defaults_and_nested_filters() {
    let query: QueryRequest<EmailFilterCondition> = serde_json::from_value(json!({
        "accountId": "a1",
        "filter": {
            "operator": "AND",
            "conditions": [
                { "inMailbox": "m1" },
                { "hasKeyword": "$flagged", "noolog:slug": "hello" }
            ]
        },
        "sort": [{ "property": "receivedAt", "isAscending": false }, { "property": "size" }]
    }))
    .unwrap();
    assert_eq!(query.position, 0);
    assert_eq!(query.anchor_offset, 0);
    assert!(!query.calculate_total);
    assert_eq!(query.limit, None);
    let sort: &Vec<Comparator> = query.sort.as_ref().unwrap();
    assert!(
        !sort[0].is_ascending && sort[1].is_ascending,
        "isAscending defaults to true"
    );
    let Some(Filter::Operator {
        operator,
        conditions,
    }) = &query.filter
    else {
        panic!("an operator");
    };
    assert_eq!(operator, "AND");
    let Filter::Condition(second) = &conditions[1] else {
        panic!("a condition")
    };
    assert_eq!(second.has_keyword.as_deref(), Some("$flagged"));
    assert_eq!(second.extra["noolog:slug"], "hello");
}

#[test]
fn a_set_carries_creates_patches_and_errors() {
    let set: SetRequest<Email> = serde_json::from_value(json!({
        "accountId": "a1",
        "create": { "k1": { "subject": "hi", "mailboxIds": { "m1": true } } },
        "update": { "e1": { "keywords/$seen": true } },
        "destroy": ["e2"]
    }))
    .unwrap();
    assert_eq!(
        set.create.as_ref().unwrap()["k1"].subject.as_deref(),
        Some("hi")
    );
    assert_eq!(
        set.update.as_ref().unwrap()[&Id::new("e1").unwrap()]["keywords/$seen"],
        true
    );
    let error = SetError::new("invalidProperties").with_properties(["subject"]);
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({ "type": "invalidProperties", "properties": ["subject"] })
    );
}
