//! Markdown proposals: the fallback, and the shape most proposals have.
//!
//! Fences, headings, list markers and pipe-tables are enough to tell a
//! navigable annex from a wall of prose, without a markdown parser.

use super::super::defaults::ProposalShape;
use super::ShapeAnalyzer;

/// Measures prose. Handles anything, so it must be registered last.
pub struct MarkdownShape;

impl ShapeAnalyzer for MarkdownShape {
    /// Everything. This is the fallback.
    fn handles(&self, _content: &str) -> bool {
        true
    }

    fn analyze(&self, content: &str) -> ProposalShape {
        measure(content)
    }
}

/// Summarise markdown structure without a full parser: fences, ATX headings,
/// list markers and pipe-tables are enough to tell a navigable annex from a
/// wall of prose.
fn measure(text: &str) -> ProposalShape {
    let mut shape = ProposalShape {
        chars: text.chars().count(),
        ..Default::default()
    };
    let mut in_code = false;
    let mut lead_done = false;
    let mut table_open = false;
    for (idx, line) in text.lines().enumerate() {
        let lineno = idx + 1;
        shape.lines = lineno;
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            if !in_code {
                shape.code_blocks += 1;
                shape.outline.push((lineno, "code".to_string()));
            }
            in_code = !in_code;
            lead_done = true;
            table_open = false;
            continue;
        }
        if in_code {
            continue;
        }
        let is_heading = t.starts_with('#');
        let is_list = t.starts_with("- ")
            || t.starts_with("* ")
            || t.starts_with("+ ")
            || t.split_once('.').is_some_and(|(n, rest)| {
                !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' ')
            });
        let is_table = t.starts_with('|') && t.matches('|').count() >= 2;

        if is_heading {
            shape.headings += 1;
            shape.outline.push((lineno, t.chars().take(60).collect()));
        }
        if is_list {
            shape.list_items += 1;
        }
        if is_table {
            shape.table_rows += 1;
            if !table_open {
                shape.outline.push((lineno, "table".to_string()));
            }
        }
        table_open = is_table;
        if is_heading || is_list || is_table {
            lead_done = true;
        } else {
            shape.prose_chars += line.chars().count();
            if !lead_done {
                shape.lead_chars += line.chars().count();
            }
        }
    }
    shape
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(text: &str) -> ProposalShape {
        MarkdownShape.analyze(text)
    }

    #[test]
    fn each_structural_marker_is_counted_once() {
        let s = shape("Answer.\n\n## Why\n\n- a\n- b\n\n```\nx\n```\n\n| a | b |\n| - | - |\n");
        assert_eq!(s.headings, 1);
        assert_eq!(s.list_items, 2);
        assert_eq!(s.code_blocks, 1);
        assert_eq!(s.table_rows, 2);
    }

    #[test]
    fn markers_inside_a_fence_are_code_not_structure() {
        let s = shape("Answer.\n\n```\n# not a heading\n- not a list\n```\n");
        assert_eq!(s.headings, 0);
        assert_eq!(s.list_items, 0);
        assert_eq!(s.code_blocks, 1);
    }

    #[test]
    fn lead_chars_stop_at_the_first_structural_break() {
        let s = shape("Short.\n\n## Then\n\nmore\n");
        assert!(s.lead_chars < 12, "lead was {}", s.lead_chars);
    }

    #[test]
    fn a_wall_of_prose_is_all_prose_and_no_structure() {
        let s = shape("one two three four five six seven eight nine ten");
        assert_eq!(s.headings + s.list_items + s.table_rows + s.code_blocks, 0);
        assert_eq!(s.prose_chars, s.chars);
    }

    #[test]
    fn an_empty_proposal_measures_zero() {
        let s = shape("");
        assert_eq!(s.chars, 0);
        assert_eq!(s.lines, 0);
        assert!(s.outline.is_empty());
    }
}
