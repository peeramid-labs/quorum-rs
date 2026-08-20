//! Structured proposals submitted as JSON.
//!
//! Stored compactly on one line, so measured as prose it reports no structure
//! at all — the profile the conciseness axis scores most negatively. Keys are
//! the sections and arrays are the lists.
//!
//! Measured over the rendered form, since that is what the reader is shown and
//! the outline's anchors must match the lines `read_proposal` returns.

use super::super::defaults::ProposalShape;
use super::ShapeAnalyzer;

/// Measures a proposal that is a JSON object or array.
pub struct JsonShape;

impl ShapeAnalyzer for JsonShape {
    /// Only objects and arrays. A bare string or number is JSON too, but it
    /// carries no structure to describe and reads as prose.
    fn handles(&self, content: &str) -> bool {
        let t = content.trim_start();
        (t.starts_with('{') || t.starts_with('['))
            && serde_json::from_str::<serde_json::Value>(content)
                .is_ok_and(|v| v.is_object() || v.is_array())
    }

    fn analyze(&self, content: &str) -> ProposalShape {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
            return ProposalShape::default();
        };
        measure(&value, content)
    }
}

/// Render the proposal the way a reader should see it: one field per line.
///
/// The stored form is compact, so without this the outline's line anchors
/// would all point at line 1 and be useless to the read tool.
pub fn readable_form(content: &str) -> Option<String> {
    let document = serde_json::from_str::<serde_json::Value>(content).ok()?;
    if !(document.is_object() || document.is_array()) {
        return None;
    }
    serde_json::to_string_pretty(&document).ok()
}

fn measure(value: &serde_json::Value, original: &str) -> ProposalShape {
    let rendered = serde_json::to_string_pretty(value).unwrap_or_else(|_| original.to_string());

    let mut shape = ProposalShape {
        // What the reader is shown, which is the rendered form.
        chars: rendered.chars().count(),
        lines: rendered.lines().count(),
        ..Default::default()
    };

    // Top-level keys are the sections a reader navigates by; nested keys are
    // detail within one. Counting every key would make depth look like
    // navigability.
    let top_level: Vec<String> = match value {
        serde_json::Value::Object(map) => map.keys().cloned().collect(),
        _ => Vec::new(),
    };

    for (idx, line) in rendered.lines().enumerate() {
        let lineno = idx + 1;
        let trimmed = line.trim_start();
        if let Some(key) = top_level
            .iter()
            .find(|k| trimmed.starts_with(&format!("\"{k}\":")) && line.len() - trimmed.len() == 2)
        {
            shape.headings += 1;
            shape.outline.push((lineno, key.clone()));
        }
    }

    count_values(value, &mut shape);

    // Everything before the first section starts — the braces and whatever
    // sits above the first key.
    shape.lead_chars = shape
        .outline
        .first()
        .map(|(line, _)| {
            rendered
                .lines()
                .take(line.saturating_sub(1))
                .map(|l| l.chars().count() + 1)
                .sum()
        })
        .unwrap_or(shape.chars);

    shape
}

/// Walk the whole document: array elements are the lists, and string values
/// are where prose — and therefore padding — actually lives.
fn count_values(value: &serde_json::Value, shape: &mut ProposalShape) {
    match value {
        serde_json::Value::Array(items) => {
            shape.list_items += items.len();
            for item in items {
                count_values(item, shape);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                count_values(v, shape);
            }
        }
        serde_json::Value::String(s) => {
            shape.prose_chars += s.chars().count();
            // A multi-line string value is an embedded block — a diff, a patch,
            // a snippet — which is the JSON spelling of a fenced code block.
            if s.contains('\n') {
                shape.code_blocks += 1;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRUCTURED: &str = r#"{"reply":"Short answer.","ops":[{"path":"a.rs","edit":"x"},{"path":"b.rs","edit":"y"}]}"#;

    #[test]
    fn a_structured_proposal_is_not_measured_as_prose() {
        let shape = super::super::analyze(STRUCTURED);
        assert_eq!(shape.headings, 2, "reply and ops are the sections");
        assert_eq!(shape.list_items, 2, "ops carries two entries");
        assert!(
            shape.lines > 1,
            "measured over the rendered form, not the stored one line"
        );
        assert!(
            shape.lead_chars < shape.chars,
            "a structured proposal must not read as one long wait for the answer: \
             lead {} of {} chars",
            shape.lead_chars,
            shape.chars
        );
    }

    #[test]
    fn prose_chars_counts_the_text_not_the_punctuation() {
        let shape = super::super::analyze(STRUCTURED);
        // "Short answer." + the four short field values, not the braces,
        // quotes and commas around them.
        assert!(
            shape.prose_chars < shape.chars / 2,
            "syntax was counted as prose: {} of {}",
            shape.prose_chars,
            shape.chars
        );
    }

    #[test]
    fn the_outline_points_at_lines_the_read_tool_will_return() {
        let shape = super::super::analyze(STRUCTURED);
        let rendered = readable_form(STRUCTURED).expect("structured content renders");
        let lines: Vec<&str> = rendered.lines().collect();
        for (lineno, label) in &shape.outline {
            let line = lines
                .get(lineno.saturating_sub(1))
                .unwrap_or_else(|| panic!("outline points past the end: L{lineno} {label}"));
            assert!(
                line.contains(label.as_str()),
                "L{lineno} was supposed to be {label}, the line reads {line}"
            );
        }
    }

    #[test]
    fn a_multi_line_string_value_counts_as_an_embedded_block() {
        let shape = super::super::analyze(r#"{"patch":"line one\nline two\nline three"}"#);
        assert_eq!(shape.code_blocks, 1);
    }

    #[test]
    fn prose_that_merely_starts_with_a_brace_is_still_prose() {
        let shape = super::super::analyze("{ this is not json, it is a sentence");
        assert_eq!(shape.headings, 0);
        assert_eq!(shape.lines, 1, "measured by the markdown fallback");
    }

    #[test]
    fn a_bare_json_string_is_prose_not_structure() {
        assert!(
            !JsonShape.handles(r#""just a quoted sentence""#),
            "a scalar has no structure to describe"
        );
    }
}
