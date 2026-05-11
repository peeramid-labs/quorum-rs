//! Utility functions for string manipulation and cleanup.

use serde_json::Value;

/// Helper function to extract a JSON object from a potentially noisy string.
/// It finds the first `{` and the last `}` and returns the substring.
/// If no braces are found, it returns the original string (hoping it's valid JSON).
///
/// When `unwrap_hallucinated_tool_calls` is true, tool-call envelopes of the
/// form `{"name": "...", "arguments": {...}}` are unwrapped to just the
/// arguments object. If `expected_tool_name` is also provided, the unwrap
/// only happens when the envelope's `name` matches (or is missing). This
/// prevents incorrect unwrapping when the model emits an envelope for an
/// intermediate tool (e.g. `search_deliberation`) in a context where the
/// caller will try to deserialize against the terminal tool's schema —
/// that would yield confusing "missing required field" errors and burn
/// retry budget with no way for the retry prompt to realize the model
/// intended a different tool.
pub fn clean_json_string(
    input: &str,
    unwrap_hallucinated_tool_calls: bool,
    expected_tool_name: Option<&str>,
) -> String {
    // Heuristic: Find the first `{` that is followed by `"` (standard JSON key) or `}` (empty object).
    // This avoids capturing LaTeX like `\boxed{7}` which starts with `{7`.
    let mut start_index = None;
    let mut current_pos = 0;

    while let Some(pos) = input[current_pos..].find('{') {
        let absolute_pos = current_pos + pos;
        // Check identifying char after `{`
        let remainder = &input[absolute_pos + 1..];
        // We only need to check the first non-whitespace character
        if let Some(first_char) = remainder.chars().find(|c| !c.is_whitespace())
            && (first_char == '"' || first_char == '}')
        {
            start_index = Some(absolute_pos);
            break;
        }
        // Move past this `{` to search again
        current_pos = absolute_pos + 1;
    }

    // If no "valid" start found, fall back to simple find (or original string)
    let start = start_index.or_else(|| input.find('{'));
    let end = input.rfind('}');

    let candidate = match (start, end) {
        (Some(start), Some(end)) if start <= end => &input[start..=end],
        _ => input,
    };

    if unwrap_hallucinated_tool_calls
        && let Ok(val) = serde_json::from_str::<Value>(candidate)
        && let Some(args) = val.get("arguments")
        && args.is_object()
    {
        // If the caller specified an expected tool name, skip the unwrap
        // when the envelope names a different tool. Passing the raw
        // envelope through lets the retry loop surface the actual tool
        // the model chose, instead of silently stripping the name and
        // then failing to deserialize against the wrong schema.
        // Name-check policy:
        //   - key absent  → allow unwrap (caller didn't pin a name)
        //   - key present + string matches expected → allow
        //   - key present + string mismatch → reject
        //   - key present + non-string (null/number/object) → reject
        //     rather than silently allow unwrap; a malformed envelope
        //     should surface via retry, not have its wrapper stripped.
        let name_ok = match expected_tool_name {
            None => true,
            Some(expected) => match val.get("name") {
                None => true,
                Some(Value::String(actual)) => actual == expected,
                Some(_) => false,
            },
        };
        if name_ok {
            return args.to_string();
        }
    }

    candidate.to_string()
}

/// Helper to split arguments while respecting nested brackets/quotes.
/// e.g. `a, "b, c", [d, e]` -> `["a", "\"b, c\"", "[d, e]"]`
pub fn split_args_respecting_brackets(args_str: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0;
    let mut in_quote = false;
    let mut quote_char = '\0';
    let mut escape = false;

    for c in args_str.chars() {
        if escape {
            current.push(c);
            escape = false;
            continue;
        }
        if c == '\\' {
            current.push(c);
            escape = true;
            continue;
        }

        if in_quote {
            current.push(c);
            if c == quote_char {
                in_quote = false;
            }
        } else {
            match c {
                '"' | '\'' => {
                    in_quote = true;
                    quote_char = c;
                    current.push(c);
                }
                '[' | '{' | '(' => {
                    depth += 1;
                    current.push(c);
                }
                ']' | '}' | ')' => {
                    if depth > 0 {
                        depth -= 1;
                    }
                    current.push(c);
                }
                ',' => {
                    if depth == 0 {
                        parts.push(current.trim().to_string());
                        current.clear();
                    } else {
                        current.push(c);
                    }
                }
                _ => current.push(c),
            }
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_json_string_basic() {
        let input = "Here is JSON: {\"key\": \"value\"} end";
        assert_eq!(
            clean_json_string(input, false, None),
            "{\"key\": \"value\"}"
        );
    }

    #[test]
    fn test_clean_json_string_nested() {
        let input = "Start { \"a\": { \"b\": 1 } } End";
        assert_eq!(
            clean_json_string(input, false, None),
            "{ \"a\": { \"b\": 1 } }"
        );
    }

    #[test]
    fn test_clean_json_string_truncated() {
        // Simulate an EOF error scenario where the closing brace is missing or late
        // If NO closing brace is found, it returns the original string (fallback)
        let input = "{ \"key\": \"value\"";
        assert_eq!(
            clean_json_string(input, false, None),
            "{ \"key\": \"value\""
        );

        // If incomplete braces are present, it tries its best
        let input = "Prefix { \"key\": \"val... (cut)";
        assert_eq!(
            clean_json_string(input, false, None),
            "Prefix { \"key\": \"val... (cut)"
        );
    }

    #[test]
    fn test_unwrap_hallucinated_tool_call() {
        let input = r#"{"name": "submit_proposal", "arguments": {"thought_process": "Thinking...", "solution_content": "42"}}"#;
        let cleaned = clean_json_string(input, true, None);
        let cleaned_json: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        let expected_json: serde_json::Value =
            serde_json::from_str(r#"{"thought_process": "Thinking...", "solution_content": "42"}"#)
                .unwrap();

        assert_eq!(cleaned_json, expected_json);
    }

    #[test]
    fn test_unwrap_skips_when_tool_name_mismatches_expected() {
        // Qwen3 on OpenRouter has been observed emitting envelope-style
        // tool calls for INTERMEDIATE tools (search_deliberation) while
        // the caller expects the terminal tool (submit_proposal). Stripping
        // the envelope would leave args that don't match submit_proposal's
        // schema and cause 6 retries of "missing field `thought_process`"
        // with no way for the retry prompt to detect that the model chose
        // a different tool. Preserve the envelope so the mismatch is
        // recoverable upstream.
        let input = r#"{"name": "search_deliberation", "arguments": {"filters": {"agent_ids": ["CortexA"], "phase": "proposing"}}}"#;
        let cleaned = clean_json_string(input, true, Some("submit_proposal"));
        // Envelope should be kept intact — name field still present.
        let parsed: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(parsed["name"], "search_deliberation");
        assert!(parsed.get("arguments").is_some());
    }

    #[test]
    fn test_unwrap_happens_when_tool_name_matches_expected() {
        // When the envelope names the expected terminal tool, unwrap
        // as before — existing behaviour preserved.
        let input = r#"{"name": "submit_proposal", "arguments": {"thought_process": "T", "solution_content": "S"}}"#;
        let cleaned = clean_json_string(input, true, Some("submit_proposal"));
        let parsed: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(parsed["thought_process"], "T");
        assert_eq!(parsed["solution_content"], "S");
        // Envelope should be gone.
        assert!(parsed.get("name").is_none());
        assert!(parsed.get("arguments").is_none());
    }

    #[test]
    fn test_unwrap_happens_when_envelope_has_no_name_field() {
        // Some models emit `{"arguments": {...}}` without the name key.
        // With expected_tool_name set, still unwrap (we can't tell
        // whether the name was intended to match or not — back-compat
        // with models that drop the name field).
        let input = r#"{"arguments": {"thought_process": "T", "solution_content": "S"}}"#;
        let cleaned = clean_json_string(input, true, Some("submit_proposal"));
        let parsed: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(parsed["thought_process"], "T");
    }

    #[test]
    fn test_unwrap_rejects_non_string_name_as_mismatch() {
        // Malformed envelope: `name` present but not a string. Treat
        // as a mismatch (not as "missing") so the raw envelope flows
        // through to the retry loop instead of being silently
        // unwrapped against the wrong tool schema.
        let inputs = [
            r#"{"name": 42, "arguments": {"x": 1}}"#,
            r#"{"name": null, "arguments": {"x": 1}}"#,
            r#"{"name": {"nested": true}, "arguments": {"x": 1}}"#,
        ];
        for input in &inputs {
            let cleaned = clean_json_string(input, true, Some("submit_proposal"));
            // Expect the FULL envelope (not the inner arguments)
            // because name_ok is false for non-string `name`.
            assert!(
                cleaned.contains(r#""arguments""#),
                "non-string name should leave envelope intact, got: {cleaned}"
            );
        }
    }

    #[test]
    fn test_clean_json_string_with_latex_noise() {
        // Should ignore the first {7} because it's not a JSON object start
        let input = r#"The answer is \boxed{7}. {"key": "value"}"#;
        assert_eq!(clean_json_string(input, false, None), r#"{"key": "value"}"#);
    }

    // ---------------------------------------------------------------
    // split_args_respecting_brackets — escape sequences
    // ---------------------------------------------------------------

    #[test]
    fn test_split_args_basic() {
        let result = split_args_respecting_brackets("a, b, c");
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_args_nested_brackets() {
        let result = split_args_respecting_brackets("a, [b, c], d");
        assert_eq!(result, vec!["a", "[b, c]", "d"]);
    }

    #[test]
    fn test_split_args_nested_braces() {
        let result = split_args_respecting_brackets(r#"a, {"key": "val, next"}, b"#);
        assert_eq!(result, vec!["a", r#"{"key": "val, next"}"#, "b"]);
    }

    #[test]
    fn test_split_args_quoted_commas() {
        let result = split_args_respecting_brackets(r#""arg1", "arg2, with comma", "arg3""#);
        assert_eq!(
            result,
            vec![r#""arg1""#, r#""arg2, with comma""#, r#""arg3""#]
        );
    }

    #[test]
    fn test_split_args_escape_sequences() {
        // Escaped quote inside a quoted string: "arg1, \"escaped, comma\", arg3"
        let result = split_args_respecting_brackets(r#""arg1, \"escaped, comma\"", arg3"#);
        assert_eq!(result.len(), 2);
        assert!(result[0].contains("escaped, comma"));
        assert_eq!(result[1], "arg3");
    }

    #[test]
    fn test_split_args_empty() {
        let result = split_args_respecting_brackets("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_split_args_single() {
        let result = split_args_respecting_brackets("only_one");
        assert_eq!(result, vec!["only_one"]);
    }

    #[test]
    fn test_split_args_mixed_quotes() {
        let result = split_args_respecting_brackets(r#"'single, quoted', "double, quoted""#);
        assert_eq!(result, vec!["'single, quoted'", r#""double, quoted""#]);
    }

    #[test]
    fn test_split_args_nested_parens() {
        let result = split_args_respecting_brackets("func(a, b), c");
        assert_eq!(result, vec!["func(a, b)", "c"]);
    }

    // ---------------------------------------------------------------
    // clean_json_string — additional edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_clean_json_string_no_braces() {
        let input = "Just plain text without any braces";
        assert_eq!(clean_json_string(input, false, None), input);
    }

    #[test]
    fn test_clean_json_string_empty_input() {
        assert_eq!(clean_json_string("", false, None), "");
    }

    #[test]
    fn test_clean_json_string_empty_object() {
        let input = "prefix { } suffix";
        let result = clean_json_string(input, false, None);
        assert_eq!(result, "{ }");
    }

    #[test]
    fn test_clean_json_string_start_after_end() {
        // Only closing brace before opening brace
        let input = "} ... {\"key\": \"val\"}";
        let result = clean_json_string(input, false, None);
        assert_eq!(result, "{\"key\": \"val\"}");
    }

    #[test]
    fn test_clean_json_string_unwrap_disabled_keeps_wrapper() {
        let input =
            r#"{"name": "submit", "arguments": {"thought_process": "T", "solution_content": "S"}}"#;
        let result = clean_json_string(input, false, None);
        // When unwrap is disabled, should keep the full object
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.get("name").is_some());
        assert!(parsed.get("arguments").is_some());
    }

    #[test]
    fn test_clean_json_string_unwrap_non_object_arguments() {
        // If "arguments" is a string (not an object), unwrapping should NOT happen
        let input = r#"{"name": "tool", "arguments": "just a string"}"#;
        let result = clean_json_string(input, true, None);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        // Should keep the full object since arguments is not an object
        assert!(parsed.get("name").is_some());
    }

    #[test]
    fn test_clean_json_string_multiple_latex_then_json() {
        let input = r#"Consider \boxed{42} and \frac{1}{2}. {"result": "ok"}"#;
        let result = clean_json_string(input, false, None);
        assert_eq!(result, r#"{"result": "ok"}"#);
    }

    // ---------------------------------------------------------------
    // split_args_respecting_brackets — more edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_split_args_trailing_comma() {
        let result = split_args_respecting_brackets("a, b, ");
        // Trailing empty after comma gets trimmed away (empty check)
        assert_eq!(result, vec!["a", "b"]);
    }

    #[test]
    fn test_split_args_whitespace_only() {
        let result = split_args_respecting_brackets("   ");
        assert!(result.is_empty());
    }

    #[test]
    fn test_split_args_deeply_nested() {
        let result = split_args_respecting_brackets("a, [[1, 2], [3, 4]], b");
        assert_eq!(result, vec!["a", "[[1, 2], [3, 4]]", "b"]);
    }

    #[test]
    fn test_split_args_unmatched_close_bracket() {
        // Unmatched close bracket: depth should not go negative
        let result = split_args_respecting_brackets("a], b");
        assert_eq!(result, vec!["a]", "b"]);
    }
}
