//! Recommend tested strategy config for known model families.
//!
//! Some model families need specific integration settings to work well — and we
//! already have end-to-end-tested configs for them. Rather than make the
//! operator guess, the wizard detects the family from the `model_name` and
//! surfaces the known-good settings: a one-line note, and the LLM-repair
//! multi-select pre-selected to the tested passes.
//!
//! Grounded in:
//! - `crate::llms::strategies::StrategyResolver` engines (`gpt-oss`/`harmony`,
//!   `vllm`).
//! - The e2e tests for qwen / tongyi (run with `unwrap_hallucinated_tool_calls`
//!   + `repair_invalid_escapes`) and gpt-oss (Harmony engine).

/// A tested-config recommendation for a model family.
pub(super) struct ModelRecommendation {
    /// Family label shown to the operator (e.g. "qwen", "gpt-oss").
    pub family: &'static str,
    /// Provider `engine` to advise (strategy selector). `None` = native default.
    /// Provider-level, not per-agent, so the wizard only *advises* setting it.
    pub engine: Option<&'static str>,
    /// Recommended `repair_invalid_escapes`.
    pub repair_invalid_escapes: bool,
    /// Recommended `unwrap_hallucinated_tool_calls`.
    pub unwrap_hallucinated_tool_calls: bool,
    /// One-line rationale shown to the operator.
    pub note: &'static str,
}

/// Recommend a tested config for `model_name`, or `None` for unknown families.
/// Case-insensitive substring match; first match wins.
pub(super) fn recommend(model_name: &str) -> Option<ModelRecommendation> {
    let m = model_name.to_ascii_lowercase();
    if m.contains("gpt-oss") || m.contains("gptoss") {
        return Some(ModelRecommendation {
            family: "gpt-oss",
            engine: Some("gpt-oss"),
            // Harmony parses tool-calls natively — no text-repair passes.
            repair_invalid_escapes: false,
            unwrap_hallucinated_tool_calls: false,
            note: "gpt-oss → Harmony engine (handles tool-calls natively).",
        });
    }
    if m.contains("qwen") {
        return Some(ModelRecommendation {
            family: "qwen",
            engine: None,
            repair_invalid_escapes: true,
            unwrap_hallucinated_tool_calls: true,
            note: "qwen3-family: fix invalid escapes + parse hallucinated tool-calls (tested).",
        });
    }
    if m.contains("tongyi") {
        return Some(ModelRecommendation {
            family: "tongyi",
            engine: None,
            repair_invalid_escapes: true,
            unwrap_hallucinated_tool_calls: true,
            note: "tongyi-deepresearch: fix invalid escapes + parse hallucinated tool-calls (tested).",
        });
    }
    if m.contains("vllm") {
        return Some(ModelRecommendation {
            family: "vllm",
            engine: Some("vllm"),
            repair_invalid_escapes: true,
            unwrap_hallucinated_tool_calls: true,
            note: "vLLM-served model: XML-regex engine.",
        });
    }
    None
}

/// The `ℹ` guidance line to print for a model (empty when no recommendation).
/// Includes engine advice the wizard can't apply itself (provider-level).
pub(super) fn note_line(rec: &Option<ModelRecommendation>) -> String {
    match rec {
        None => String::new(),
        Some(r) => {
            let engine = match r.engine {
                Some(e) => format!(" Set `engine: \"{e}\"` on the provider in quorum.yml."),
                None => String::new(),
            };
            format!(
                "  \u{2139} Recommended (tested) config for {}: {}{}\n",
                r.family, r.note, engine
            )
        }
    }
}

/// Default selection indices for the LLM-repair multi-select
/// `[fix invalid JSON escapes, parse hallucinated tool-calls, request JSON mode]`,
/// pre-selected from the recommendation. `None` keeps the standard default
/// (escapes + tool-calls).
pub(super) fn repair_default_indices(rec: &Option<ModelRecommendation>) -> Vec<usize> {
    let (esc, hall) = match rec {
        Some(r) => (r.repair_invalid_escapes, r.unwrap_hallucinated_tool_calls),
        None => (true, true),
    };
    let mut idx = Vec::new();
    if esc {
        idx.push(0);
    }
    if hall {
        idx.push(1);
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommends_qwen_with_repair_and_unwrap() {
        let r = recommend("qwen/qwen3-coder").expect("qwen recognised");
        assert_eq!(r.family, "qwen");
        assert!(r.repair_invalid_escapes && r.unwrap_hallucinated_tool_calls);
        assert_eq!(r.engine, None);
    }

    #[test]
    fn recommends_gpt_oss_engine_no_repair() {
        let r = recommend("openai/gpt-oss-120b").expect("gpt-oss recognised");
        assert_eq!(r.family, "gpt-oss");
        assert_eq!(r.engine, Some("gpt-oss"));
        assert!(!r.repair_invalid_escapes && !r.unwrap_hallucinated_tool_calls);
    }

    #[test]
    fn recommends_tongyi() {
        let r = recommend("alibaba/tongyi-deepresearch-30b-a3b").expect("tongyi recognised");
        assert_eq!(r.family, "tongyi");
        assert!(r.repair_invalid_escapes);
    }

    #[test]
    fn unknown_models_get_no_recommendation() {
        assert!(recommend("gpt-4o").is_none());
        assert!(recommend("meta-llama/llama-3.1-70b").is_none());
    }

    #[test]
    fn match_is_case_insensitive() {
        assert!(recommend("QWEN3-CODER").is_some());
        assert!(recommend("OpenAI/GPT-OSS-20B").is_some());
    }

    #[test]
    fn note_line_empty_for_unknown_and_set_for_known() {
        assert_eq!(note_line(&None), "");
        let n = note_line(&recommend("qwen3-coder"));
        assert!(n.contains("qwen"));
        let g = note_line(&recommend("gpt-oss-120b"));
        assert!(
            g.contains("engine: \"gpt-oss\""),
            "gpt-oss note advises engine"
        );
    }

    #[test]
    fn repair_defaults_track_recommendation() {
        assert_eq!(repair_default_indices(&None), vec![0, 1]);
        assert_eq!(
            repair_default_indices(&recommend("qwen3-coder")),
            vec![0, 1]
        );
        // gpt-oss → Harmony, no repair passes pre-selected.
        assert_eq!(
            repair_default_indices(&recommend("gpt-oss-120b")),
            Vec::<usize>::new()
        );
    }
}
