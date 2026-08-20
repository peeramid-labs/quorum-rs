//! Measuring the shape of a proposal, one analyzer per content kind.
//!
//! Analyzers share one vocabulary of metrics rather than each inventing its
//! own: the prompt explains those names to the evaluator, so a metric no
//! prompt explains is one no evaluator can act on.

use super::defaults::ProposalShape;

/// Measures one kind of proposal content.
pub trait ShapeAnalyzer: Send + Sync {
    /// Whether this analyzer recognises the content. Cheap: it runs on every
    /// candidate, every round, ahead of the analyzer that will do the work.
    fn handles(&self, content: &str) -> bool;

    /// Measure it. Only called when [`ShapeAnalyzer::handles`] returned true.
    fn analyze(&self, content: &str) -> ProposalShape;
}

/// The analyzers, in priority order. The last one handles everything, so the
/// registry always produces a measurement.
fn analyzers() -> [&'static dyn ShapeAnalyzer; 2] {
    [&json::JsonShape, &markdown::MarkdownShape]
}

/// Measure a proposal with the first analyzer that recognises it.
pub fn analyze(content: &str) -> ProposalShape {
    for a in analyzers() {
        if a.handles(content) {
            return a.analyze(content);
        }
    }
    // Unreachable: the markdown analyzer handles everything. Measuring nothing
    // is still better than panicking inside prompt construction.
    ProposalShape::default()
}

pub mod json;
pub mod markdown;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_the_fallback_so_every_proposal_is_measured() {
        // Not JSON, not empty, no markdown structure either.
        let shape = analyze("just a sentence");
        assert_eq!(shape.lines, 1);
        assert!(shape.chars > 0, "the fallback still measures size");
    }

    #[test]
    fn an_empty_proposal_measures_without_panicking() {
        let shape = analyze("");
        assert_eq!(shape.chars, 0);
        assert!(shape.outline.is_empty());
    }
}
