//! Regression corpus for citation resolution, driven from
//! `tests/fixtures/cite_vectors.json`.
//!
//! The fixture holds the real failure shapes observed in production rooms, so a
//! change to the normalisation ladder that silently widens or narrows what
//! resolves shows up here. Add a case to the file rather than only inline.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Vectors {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    why: String,
    source: String,
    cite: String,
    expect: Option<Expect>,
}

#[derive(Debug, Deserialize)]
struct Expect {
    text: String,
    start: usize,
    end: usize,
    utf16_start: usize,
    utf16_end: usize,
}

fn load() -> Vectors {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cite_vectors.json"
    );
    let raw = std::fs::read_to_string(path).expect("fixture readable");
    serde_json::from_str(&raw).expect("fixture parses")
}

#[test]
fn shared_vectors_all_hold() {
    let vectors = load();
    assert!(
        vectors.cases.len() >= 15,
        "fixture should not shrink silently"
    );

    for case in &vectors.cases {
        let got = quorum_rs::agents::cite::resolve_cite(&case.source, &case.cite);
        match (&case.expect, &got) {
            (None, Some(span)) => panic!(
                "case {:?} must NOT resolve ({}), but got {:?}",
                case.name, case.why, span
            ),
            (Some(want), None) => panic!(
                "case {:?} must resolve to {:?} ({}), but got None",
                case.name, want.text, case.why
            ),
            (None, None) => {}
            (Some(want), Some(span)) => {
                assert_eq!(
                    span.text, want.text,
                    "case {:?} text ({})",
                    case.name, case.why
                );
                assert_eq!(span.start, want.start, "case {:?} byte start", case.name);
                assert_eq!(span.end, want.end, "case {:?} byte end", case.name);
                assert_eq!(
                    span.utf16_range(&case.source),
                    (want.utf16_start, want.utf16_end),
                    "case {:?} UTF-16 range",
                    case.name
                );
                // The invariant every client relies on.
                assert_eq!(
                    &case.source[span.start..span.end],
                    span.text,
                    "case {:?}: span offsets must slice to the span text",
                    case.name
                );
            }
        }
    }
}

/// The fixture must keep covering the failure shapes that motivated the ladder;
/// deleting one should break the build, not quietly reduce coverage.
#[test]
fn shared_vectors_cover_the_known_failure_shapes() {
    let vectors = load();
    let names: Vec<&str> = vectors.cases.iter().map(|c| c.name.as_str()).collect();
    for required in [
        "markdown_emphasis_in_source",
        "markdown_emphasis_in_cite",
        "case_folded_leading_capital",
        "ellipsis_elision_must_not_resolve",
        "fabricated_cite_must_not_resolve",
        "non_ascii_offsets",
    ] {
        assert!(names.contains(&required), "fixture lost case {required:?}");
    }
}
