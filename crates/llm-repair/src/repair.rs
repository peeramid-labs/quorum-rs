//! Functions for repairing broken or malformed JSON strings.

use async_openai::types::ChatCompletionResponseMessage;
use serde_json::json;
use tracing::info;

/// Repairs tool call arguments within a response message if they contain malformed JSON.
///
/// This modifies the `response_message` in-place, logging any repairs made.
pub fn repair_tool_calls(response_message: &mut ChatCompletionResponseMessage, agent_name: &str) {
    if let Some(tool_calls) = &mut response_message.tool_calls {
        for tool_call in tool_calls {
            let original_args = tool_call.function.arguments.clone();

            // Check if arguments are valid JSON.
            if serde_json::from_str::<serde_json::Value>(&original_args).is_ok() {
                continue;
            }

            // Strategy 1: Truncation repair
            let mut repaired = repair_truncated_json(&original_args);

            // Strategy 2: Invalid escapes (LaTeX) + Truncation
            if serde_json::from_str::<serde_json::Value>(&repaired).is_err() {
                let escaped = repair_invalid_escapes(&original_args);
                repaired = repair_truncated_json(&escaped);
            }

            // Strategy 3: Aggressive escapes + Truncation
            if serde_json::from_str::<serde_json::Value>(&repaired).is_err() {
                let escaped = repair_aggressive_escapes(&original_args);
                repaired = repair_truncated_json(&escaped);
            }

            // Apply only if changed AND the result is now valid JSON
            if repaired != original_args
                && serde_json::from_str::<serde_json::Value>(&repaired).is_ok()
            {
                info!(
                    target: "nsed_activity",
                    agent = %agent_name,
                    tool = %tool_call.function.name,
                    "🔧 Repaired malformed tool arguments to prevent API error."
                );
                tool_call.function.arguments = repaired;
            }
        }
    }
}

/// If `s` ends with an INCOMPLETE `\uXXXX` escape (an UNESCAPED `\`, then `u`, then
/// 0–3 hex digits), return the byte index to truncate to (dropping the partial
/// escape). Returns `None` otherwise — crucially for `\\u0` (an escaped backslash
/// followed by a literal `u0`): the backslash run before `u` is even, so the `u` is
/// NOT part of an escape, and trimming would eat the escaped backslash. A complete
/// 4-hex `A` also returns `None` (not incomplete). All indices land on the
/// ASCII `\`/`u`/hex bytes, so truncation is always char-boundary-safe.
fn incomplete_unicode_trunc(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = b.len();
    let mut hex = 0;
    while i > 0 && hex < 3 && b[i - 1].is_ascii_hexdigit() {
        i -= 1;
        hex += 1;
    }
    // Require `\` `u` immediately before the (0–3) trailing hex digits.
    if i < 2 || b[i - 1] != b'u' || b[i - 2] != b'\\' {
        return None;
    }
    // Parity of the backslash run ending at i-2: odd ⇒ the last `\` escapes the `u`
    // (a real, incomplete `\u…`); even ⇒ the backslashes are literal pairs (`\\u…`).
    let mut bs = 0usize;
    let mut j = i - 1; // the `u` position; walk left over backslashes
    while j > 0 && b[j - 1] == b'\\' {
        j -= 1;
        bs += 1;
    }
    if bs % 2 == 1 { Some(i - 2) } else { None }
}

/// Attempts to repair a truncated JSON string by closing open braces, brackets, and quotes.
pub fn repair_truncated_json(input: &str) -> String {
    let mut repaired = input.to_string();

    // 1. Handle trailing escape sequence or incomplete unicode
    // If the string ends with an incomplete unicode escape like "\u", "\u0", "\u00", or "\u000",
    // or just a backslash "\", we need to trim it before closing.
    // However, blind removal is dangerous (e.g. removing '\' from "\\") so we rely on parser state.

    if let Some(pos) = incomplete_unicode_trunc(&repaired) {
        repaired.truncate(pos);
    }

    // 2. Scan string state
    let mut in_string = false;
    let mut escape = false;
    for c in repaired.chars() {
        if escape {
            escape = false;
        } else if c == '\\' {
            escape = true;
        } else if c == '"' {
            in_string = !in_string;
        }
    }

    // If we ended in an escape state (e.g. "foo\"), remove the dangling backslash
    if escape {
        repaired.pop();
    }

    // Now safe to close the string
    if in_string {
        repaired.push('"');
    }

    // 2b. A truncated trailing NUMBER in object-value position (`… : 9` cut from
    // `95`) would otherwise be closed into a valid-but-WRONG value and flow silently
    // into consensus scoring. A complete `5` and a truncated `5…` are indistinguishable,
    // so — outside a string — strip a numeric run that directly follows a `:`, leaving
    // `{"k": }` which fails to parse (→ rejected/retried) instead of a wrong score. A
    // cut-off keyword (a partial `true`/`false`/`null`) already fails to parse safely;
    // only numbers repair into a valid wrong value, so only numbers need this.
    // `in_string` here is the pre-close state — when we were mid-string the trailing
    // run is string content, not a value.
    if !in_string {
        let b = repaired.as_bytes();
        let mut end = repaired.len();
        while end > 0 && matches!(b[end - 1], b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E') {
            end -= 1;
        }
        if end < repaired.len() && repaired[..end].trim_end().ends_with(':') {
            repaired.truncate(end);
        }
    }

    // 3. Close open objects/arrays
    let mut stack = Vec::new();

    in_string = false;
    escape = false;

    for c in repaired.chars() {
        if escape {
            escape = false;
        } else if c == '\\' {
            escape = true;
        } else if c == '"' {
            in_string = !in_string;
        } else if !in_string {
            if c == '{' {
                stack.push('}');
            } else if c == '[' {
                stack.push(']');
            } else if c == '}' {
                if let Some('}') = stack.last() {
                    stack.pop();
                }
            } else if c == ']'
                && let Some(']') = stack.last()
            {
                stack.pop();
            }
        }
    }

    while let Some(c) = stack.pop() {
        repaired.push(c);
    }

    repaired
}

/// Repairs invalid escape sequences (e.g. `\(` for LaTeX) by double-escaping backslashes.
pub fn repair_invalid_escapes(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next_char) = chars.peek() {
                match next_char {
                    '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u' => {
                        // Valid escape, keep as is
                        output.push('\\');
                    }
                    _ => {
                        // Invalid escape (e.g. `\(`), so we escape the backslash to preserve it as literal text
                        output.push('\\');
                        output.push('\\');
                    }
                }
            } else {
                // Trailing backslash
                output.push('\\');
                output.push('\\');
            }
        } else {
            output.push(c);
        }
    }
    output
}

/// Aggressively escapes all backslashes except those escaping quotes or other backslashes.
pub fn repair_aggressive_escapes(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next_char) = chars.peek() {
                match next_char {
                    '"' | '\\' => {
                        // Keep escapes for quotes and backslashes as they are essential for JSON structure
                        output.push('\\');
                    }
                    _ => {
                        // Aggressively escape everything else (including \n, \t, etc) to be safe literals
                        output.push('\\');
                        output.push('\\');
                    }
                }
            } else {
                // Trailing backslash
                output.push('\\');
                output.push('\\');
            }
        } else {
            output.push(c);
        }
    }
    output
}

/// Removes backslashes that are not part of valid JSON escapes.
/// This is lossy (e.g. `\(` becomes `(`) but ensures JSON validity.
#[allow(clippy::collapsible_match)]
pub fn sanitize_json_string_lossy(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c.is_control() {
            // Escape control characters
            match c {
                '\n' => output.push_str("\\n"),
                '\r' => output.push_str("\\r"),
                '\t' => output.push_str("\\t"),
                '\x08' => output.push_str("\\b"), // Backspace
                '\x0c' => output.push_str("\\f"), // Form Feed
                _ => {
                    // For other control chars, do nothing or strip? Safest to ignore or encode unicode.
                    // For now, let's just strip unknown control chars to prevent errors.
                }
            }
        } else if c == '\\' {
            if let Some(&next_char) = chars.peek() {
                match next_char {
                    '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u' => {
                        // Valid escape, keep the backslash
                        output.push('\\');
                    }
                    _ => {
                        // Invalid escape: DROP the backslash, keep the character.
                        // e.g. \( -> (
                        // This breaks LaTeX formatting but saves the JSON structure.
                    }
                }
            } else {
                // Trailing backslash - drop it
            }
        } else {
            output.push(c);
        }
    }
    output
}

/// Parses conversational output (e.g., "THOUGHT: ... RESPONSE: ...") into a JSON structure.
pub fn repair_conversational_response(input: &str) -> Option<String> {
    // Regex to capture THOUGHT and RESPONSE sections common in Rnj-1 outputs.
    // Handles patterns like "THOUGHT: ... RESPONSE: ..."
    // Updated to be non-greedy for content to avoid capturing repeated hallucinations.
    let re = regex::Regex::new(
        r"(?si)THOUGHT\s*:\s*(?P<thought>.*?)\s*RESPONSE\s*:\s*(?P<content>.*?)(?:\s*THOUGHT:|$)",
    )
    .ok()?;

    if let Some(caps) = re.captures(input) {
        let thought = caps
            .name("thought")
            .map(|m| m.as_str().trim())
            .unwrap_or("");
        let content = caps
            .name("content")
            .map(|m| m.as_str().trim())
            .unwrap_or("");

        // We construct a JSON object matching `StructuredProposalResponse`.
        // If the target type T is something else (e.g. evaluations), this parsing will fail, which is acceptable.
        let json_obj = json!({
            "thought_process": thought,
            "solution_content": content,
            "solution_summary": content // Best guess for summary if missing
        });

        return Some(json_obj.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repair_truncated_json_with_multibyte() {
        // \u{fe0f} is 3 bytes: ef b8 8f.
        // A string ending with this char should not cause panic in repair_truncated_json
        // when checking for trailing "\u".
        let input = "Here is an emoji \u{fe0f}";
        let repaired = repair_truncated_json(input);
        // It shouldn't change anything because it doesn't look like an incomplete unicode escape
        assert_eq!(repaired, input);
    }

    #[test]
    fn test_repair_truncated_json() {
        // Basic truncation
        assert_eq!(
            repair_truncated_json(r#"{"key": "val"#),
            r#"{"key": "val"}"#
        );

        // Trailing backslash (escaped quote avoidance)
        assert_eq!(
            repair_truncated_json(r#"{"key": "val\"#),
            r#"{"key": "val"}"#
        );

        // Trailing unicode
        assert_eq!(
            repair_truncated_json(r#"{"key": "val\u00"#),
            r#"{"key": "val"}"#
        );

        // Nested objects
        assert_eq!(
            repair_truncated_json(r#"{"a": {"b": [1, 2"#),
            r#"{"a": {"b": [1, 2]}}"#
        );
    }

    #[test]
    fn test_repair_invalid_escapes() {
        // "Formula \( x \)" is invalid JSON because \ is not followed by valid escape char
        let input = r#"{"content": "Formula \( x \)"}"#;
        // We expect it to become "Formula \\( x \\)" which is valid JSON for "Formula \( x \)"
        let repaired = repair_invalid_escapes(input);
        assert_eq!(repaired, r#"{"content": "Formula \\( x \\)"}"#);

        let json: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(json["content"], "Formula \\( x \\)");
    }

    #[test]
    fn test_repair_alic_gsm8k_failure() {
        // Mimic the failure where model outputs LaTeX-like escapes (\(, \)) mixed with valid escapes (\n)
        let input = r#"{"content": "Text\nMath: \( x = \frac{1}{2} \)"}"#;

        let repaired = repair_invalid_escapes(input);

        let json: serde_json::Result<serde_json::Value> = serde_json::from_str(&repaired);
        assert!(json.is_ok(), "Repaired JSON should be valid: {repaired}");

        let val = json.unwrap();
        let content = val["content"].as_str().unwrap();

        assert!(content.contains("Text\nMath"));
        assert!(content.contains(r"\( x = "));
        // \f becomes \x0c (Form Feed) because it was a valid escape in the input string
        assert!(content.contains("\x0crac"));
        assert!(content.contains(r"\)"));
    }

    #[test]
    fn test_repair_aggressive_escapes() {
        // Input has invalid escape \F (if we pretend F is invalid) and valid \n.
        // Also has \" which must be preserved as \" (escaped quote).
        let input = r#"{"content": "Line 1\nLine 2 \"Quote\" \Fract{1}{2}"}"#;

        let repaired = repair_aggressive_escapes(input);
        // \n -> \\n
        // \" -> \" (kept)
        // \F -> \\F
        // \F is technically valid in regex but invalid in JSON (F is not valid escape).
        // repair_invalid_escapes would fix \F but keep \n.
        // repair_aggressive_escapes fixes \F AND escapes \n to \\n.

        let json: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        let content = json["content"].as_str().unwrap();

        // In the parsed string value:
        // \\n becomes literal characters \n
        assert!(content.contains(r"Line 1\nLine 2"));
        // \" became \" in string source -> " in value
        assert!(content.contains(r#" "Quote" "#));
        // \\F became literal \F
        assert!(content.contains(r"\Fract{1}{2}"));
    }

    #[test]
    fn test_sanitize_json_string_lossy() {
        // Input has invalid escapes like \( and \F.
        // Lossy sanitize should drop the backslash.
        let input = r#"{"content": "Math \( x \) and \Fract"}"#;

        let sanitized = sanitize_json_string_lossy(input);
        // \( -> (
        // \) -> )
        // \F -> F

        let json: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        let content = json["content"].as_str().unwrap();

        assert_eq!(content, "Math ( x ) and Fract");
    }

    #[test]
    fn test_repair_conversational_response() {
        let input = "THOUGHT: I am thinking hard.\n\nRESPONSE: 42";
        let repaired = repair_conversational_response(input).expect("Should repair");
        let json: serde_json::Value = serde_json::from_str(&repaired).unwrap();

        assert_eq!(json["thought_process"], "I am thinking hard.");
        assert_eq!(json["solution_content"], "42");
        assert_eq!(json["solution_summary"], "42");
    }

    #[test]
    fn test_repair_conversational_response_loop() {
        // Rnj-1 sometimes loops: THOUGHT: ... RESPONSE: ... THOUGHT: ... RESPONSE: ...
        // We should capture only the first instance.
        let input = "THOUGHT: Think 1 RESPONSE: Content 1\nTHOUGHT: Think 2 RESPONSE: Content 2";
        let repaired = repair_conversational_response(input).expect("Should repair");
        let json: serde_json::Value = serde_json::from_str(&repaired).unwrap();

        assert_eq!(json["thought_process"], "Think 1");
        assert_eq!(json["solution_content"], "Content 1");
    }

    #[test]
    fn test_full_pipeline_alic_gsm8k_failure() {
        // Replicate logic from generate_structured_output to debug failure
        // Use a concise string that contains the same problematic escape patterns
        // \( c > v > s \geq 1 \) -> invalid escapes \(, \), \g
        let raw_content =
            r#"{"content": "Constraints: \( c > v > s \geq 1 \)", "mode": "overwrite"}"#;

        // Step 0: Clean
        let cleaned_json = crate::utils::clean_json_string(raw_content, false, None);

        // Step 1: Initial Parse
        let parse_result: serde_json::Result<serde_json::Value> =
            serde_json::from_str(&cleaned_json);

        if parse_result.is_err() {
            // Step 2: Repair Truncated
            let repaired = repair_truncated_json(&cleaned_json);
            if let Ok(_val) = serde_json::from_str::<serde_json::Value>(&repaired) {
                // Success
                return;
            }

            // Step 3: Repair Invalid Escapes (Smart)
            let repaired_escapes = repair_invalid_escapes(&cleaned_json);
            let repaired_escapes_truncated = repair_truncated_json(&repaired_escapes);
            if let Ok(_val) = serde_json::from_str::<serde_json::Value>(&repaired_escapes_truncated)
            {
                // Success
                return;
            }

            // Step 4: Repair Aggressive
            let aggressive = repair_aggressive_escapes(&cleaned_json);
            let aggressive_truncated = repair_truncated_json(&aggressive);
            if let Ok(_val) = serde_json::from_str::<serde_json::Value>(&aggressive_truncated) {
                // Success
                return;
            }

            // Step 5: Nuclear Option
            let sanitized = sanitize_json_string_lossy(&cleaned_json);
            let sanitized_truncated = repair_truncated_json(&sanitized);
            if let Ok(_val) = serde_json::from_str::<serde_json::Value>(&sanitized_truncated) {
                // Success
                return;
            }

            panic!("All repairs failed! Last error: {:?}", parse_result.err());
        }
    }

    #[test]
    fn test_repair_skips_unrepairable_json() {
        use async_openai::types::{
            ChatCompletionMessageToolCall, ChatCompletionResponseMessage, ChatCompletionToolType,
            FunctionCall,
        };

        let tool_call = ChatCompletionMessageToolCall {
            id: "test".to_string(),
            r#type: ChatCompletionToolType::Function,
            function: FunctionCall {
                name: "test_tool".to_string(),
                arguments: "completely invalid not json at all!!".to_string(),
            },
        };
        let original = tool_call.function.arguments.clone();

        let mut response_message = ChatCompletionResponseMessage {
            content: None,
            refusal: None,
            tool_calls: Some(vec![tool_call]),
            role: async_openai::types::Role::Assistant,
            #[allow(deprecated)]
            function_call: None,
            audio: None,
        };

        repair_tool_calls(&mut response_message, "test-agent");

        // Should NOT have changed since repair can't produce valid JSON from gibberish
        let calls = response_message.tool_calls.unwrap();
        assert_eq!(calls[0].function.arguments, original);
    }

    #[test]
    fn test_repair_alic_gsm8k_exact() {
        // Exact string from user report
        let content = r#"Let's break down the problem step-by-step to determine how many vacuum cleaners Melanie started with.\n\n1. Define the initial number of vacuum cleaners as \( x \).\n2. At the green house, she sold a third of her vacuum cleaners. So, she sold \( \frac{x}{3} \) and has \( x - \frac{x}{3} = \frac{2x}{3} \) left.\n3. She then sold 2 more vacuum cleaners at the red house. Now, the remaining vacuum cleaners are \( \frac{2x}{3} - 2 \).\n4. At the orange house, she sold half of what was left after the red house. This means she sold \( \frac{1}{2} \left( \frac{2x}{3} - 2 \right) \), and the remaining amount is also \( \frac{1}{2} \left( \frac{2x}{3} - 2 \right) \).\n5. According to the problem, the remaining vacuum cleaners after all sales are 5. Therefore, we can set up the equation:\n   \[\n   \frac{1}{2} \left( \frac{2x}{3} - 2 \right) = 5\n   \]\n6. Solve for \( x \):\n   - Multiply both sides by 2 to eliminate the fraction:\n     \[\n     \frac{2x}{3} - 2 = 10\n     \]\n   - Add 2 to both sides:\n     \[\n     \frac{2x}{3} = 12\n     \]\n   - Multiply both sides by 3 to eliminate the denominator:\n     \[\n     2x = 36\n     \]\n   - Divide both sides by 2:\n     \[\n     x = 18\n     \]\n\nThis seems logical, but let's verify it to ensure correctness."#;

        // Wrap in JSON structure as seen in log
        let input = format!(r#"{{"content": "{content}", "mode": "append"}}"#);

        // Attempt repair
        let repaired = repair_invalid_escapes(&input);
        // Also apply truncation repair as done in production
        let repaired_truncated = repair_truncated_json(&repaired);

        // Validate
        let json: serde_json::Result<serde_json::Value> = serde_json::from_str(&repaired_truncated);
        assert!(
            json.is_ok(),
            "Repaired JSON should be valid. Error: {:?}\nRepaired: {}",
            json.err(),
            repaired_truncated
        );

        let val = json.unwrap();
        let parsed_content = val["content"].as_str().unwrap();
        assert!(parsed_content.contains(r"\( x \)"));
        assert!(parsed_content.contains(r"\["));
    }

    #[test]
    fn test_repair_valid_json_passthrough() {
        // Already-valid JSON should pass through all repair stages unchanged.
        let input = r#"{"foo": "bar"}"#;

        // Each repair function should return the same valid JSON.
        assert_eq!(repair_truncated_json(input), input);
        assert_eq!(repair_invalid_escapes(input), input);
        assert_eq!(repair_aggressive_escapes(input), input);
        assert_eq!(sanitize_json_string_lossy(input), input);

        // And it must still parse correctly.
        let val: serde_json::Value = serde_json::from_str(input).unwrap();
        assert_eq!(val["foo"], "bar");
    }

    #[test]
    fn test_repair_conversational_with_json_code_block() {
        // Models often wrap their answer in conversational text with a code block.
        // clean_json_string (which is the first step in the pipeline) should extract
        // the JSON from between the braces.
        let input = "Sure! Here's the answer:\n```json\n{\"score\": 0.8}\n```";

        let cleaned = crate::utils::clean_json_string(input, false, None);
        let val: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(val["score"], 0.8);
    }

    #[test]
    fn test_repair_merged_fields() {
        // A key without quotes merged with its value — repair_truncated_json
        // should at least close the structure so downstream stages can attempt parsing.
        // This represents truncated JSON where the value is cut off mid-field.
        let input = r#"{"thought_process": "step 1", "solution_content": "partial answ"#;

        let repaired = repair_truncated_json(input);
        let val: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(val["thought_process"], "step 1");
        // The truncated value should be closed and recoverable.
        assert!(
            val["solution_content"]
                .as_str()
                .unwrap()
                .starts_with("partial answ")
        );
    }

    #[test]
    fn test_repair_thinking_token_prefix() {
        // Models with reasoning/thinking tokens may prefix output with <think>...</think>.
        // clean_json_string should skip past the thinking tags to find the real JSON.
        let input = "<think>reasoning here</think>{\"real\": \"json\"}";

        let cleaned = crate::utils::clean_json_string(input, false, None);
        let val: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(val["real"], "json");
    }

    #[test]
    fn test_repair_tool_calls_basic() {
        use async_openai::types::{
            ChatCompletionMessageToolCall, ChatCompletionResponseMessage, ChatCompletionToolType,
            FunctionCall,
        };

        // Tool call with truncated JSON arguments — repair should fix them.
        let tool_call = ChatCompletionMessageToolCall {
            id: "call_1".to_string(),
            r#type: ChatCompletionToolType::Function,
            function: FunctionCall {
                name: "submit_proposal".to_string(),
                arguments: r#"{"thought_process": "thinking", "solution_content": "answer"#
                    .to_string(),
            },
        };

        let mut response_message = ChatCompletionResponseMessage {
            content: None,
            refusal: None,
            tool_calls: Some(vec![tool_call]),
            role: async_openai::types::Role::Assistant,
            #[allow(deprecated)]
            function_call: None,
            audio: None,
        };

        repair_tool_calls(&mut response_message, "test-agent");

        let calls = response_message.tool_calls.unwrap();
        // After repair, the arguments should be valid JSON.
        let val: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(val["thought_process"], "thinking");
        assert!(
            val["solution_content"]
                .as_str()
                .unwrap()
                .starts_with("answer")
        );
    }

    // ---------------------------------------------------------------
    // Incomplete unicode escapes
    // ---------------------------------------------------------------

    #[test]
    fn test_repair_truncated_json_incomplete_unicode_1hex() {
        let input = r#"{"key": "val\u0"#;
        let repaired = repair_truncated_json(input);
        let val: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(val["key"], "val");
    }

    #[test]
    fn escaped_backslash_before_u_is_not_eaten_as_incomplete_unicode() {
        // `\\u0` is an ESCAPED backslash followed by a literal `u0`, not an incomplete
        // `\u` escape. The old fixed-offset branch trimmed `\u0` and then popped the
        // orphaned `\`, losing the escaped backslash. The parity check preserves it.
        // Even backslash run before `u` ⇒ no truncation.
        assert_eq!(incomplete_unicode_trunc(r"foo\\u0"), None);
        assert_eq!(incomplete_unicode_trunc(r"foo\\u"), None);
        // Odd run ⇒ a real incomplete escape ⇒ trims from the escaping backslash.
        assert_eq!(incomplete_unicode_trunc(r"foo\u0"), Some(3));
        assert_eq!(incomplete_unicode_trunc(r"foo\\\u00"), Some(5));
        // A complete 4-hex escape is not "incomplete".
        assert_eq!(incomplete_unicode_trunc(r"xA"), None);
        // Trailing hex with no `\u` doesn't false-trigger.
        assert_eq!(incomplete_unicode_trunc("deadbeef"), None);
    }

    #[test]
    fn test_repair_truncated_json_incomplete_unicode_2hex() {
        let input = r#"{"key": "val\u00"#;
        let repaired = repair_truncated_json(input);
        let val: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(val["key"], "val");
    }

    #[test]
    fn test_repair_truncated_json_incomplete_unicode_3hex() {
        let input = r#"{"key": "val\u000"#;
        let repaired = repair_truncated_json(input);
        let val: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(val["key"], "val");
    }

    // ---------------------------------------------------------------
    // Trailing backslash in repair functions
    // ---------------------------------------------------------------

    #[test]
    fn test_repair_invalid_escapes_trailing_backslash() {
        let input = r#"{"key": "trailing\"#;
        let repaired = repair_invalid_escapes(input);
        // The trailing backslash creates an invalid escape — repair should handle it
        // by either removing or escaping the final backslash
        let repaired_truncated = repair_truncated_json(&repaired);
        let result: serde_json::Result<serde_json::Value> =
            serde_json::from_str(&repaired_truncated);
        assert!(
            result.is_ok(),
            "Repaired trailing backslash should yield valid JSON: {repaired_truncated}"
        );
    }

    #[test]
    fn test_repair_aggressive_escapes_trailing_backslash() {
        let input = r#"{"key": "trailing\"#;
        let repaired = repair_aggressive_escapes(input);
        let repaired_truncated = repair_truncated_json(&repaired);
        let result: serde_json::Result<serde_json::Value> =
            serde_json::from_str(&repaired_truncated);
        assert!(
            result.is_ok(),
            "Aggressive repair of trailing backslash should yield valid JSON: {repaired_truncated}"
        );
    }

    // ---------------------------------------------------------------
    // Control chars in sanitize_json_string_lossy
    // ---------------------------------------------------------------

    #[test]
    fn test_sanitize_json_string_lossy_control_chars() {
        // Note: control chars like \x01 in the input would cause JSON parse failure
        // but sanitize_json_string_lossy only processes backslash-letter sequences,
        // not raw control chars. Test that invalid escapes are dropped.
        let input = r#"{"key": "has \q and \z bad escapes"}"#;
        let sanitized = sanitize_json_string_lossy(input);
        let val: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        let content = val["key"].as_str().unwrap();
        // \q -> q, \z -> z (backslash dropped)
        assert_eq!(content, "has q and z bad escapes");
    }

    // ---------------------------------------------------------------
    // Valid JSON passthrough (additional)
    // ---------------------------------------------------------------

    #[test]
    fn test_all_repair_functions_on_complex_valid_json() {
        let input = r#"{"a": "hello\nworld", "b": [1, 2, 3], "c": {"d": "test \"quote\""}}"#;
        // Should all pass through unchanged since input is already valid
        assert_eq!(repair_truncated_json(input), input);
        assert_eq!(repair_invalid_escapes(input), input);
        // Note: repair_aggressive_escapes will double-escape \n to \\n,
        // so it's not a true passthrough for valid escapes. That's by design.

        // Verify the original still parses
        let val: serde_json::Value = serde_json::from_str(input).unwrap();
        assert_eq!(val["a"], "hello\nworld");
    }

    #[test]
    fn test_repair_truncated_json_nested_arrays_and_objects() {
        let input = r#"{"a": [{"b": [1, 2, {"c": "deep"#;
        let repaired = repair_truncated_json(input);
        let val: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert!(val["a"].is_array());
    }

    // ---------------------------------------------------------------
    // sanitize_json_string_lossy — additional edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_sanitize_json_string_lossy_trailing_backslash() {
        // Trailing backslash should be dropped
        let input = r#"{"key": "trailing\"#;
        let sanitized = sanitize_json_string_lossy(input);
        // Trailing backslash is dropped
        let repaired = repair_truncated_json(&sanitized);
        let val: serde_json::Result<serde_json::Value> = serde_json::from_str(&repaired);
        assert!(
            val.is_ok(),
            "Sanitized + repaired should be valid JSON: {}",
            repaired
        );
    }

    #[test]
    fn test_sanitize_json_string_lossy_control_char_newline() {
        // Raw newline in JSON value is illegal; sanitize should escape it
        let input = "{\"key\": \"line1\nline2\"}";
        let sanitized = sanitize_json_string_lossy(input);
        // The raw \n should be replaced with \\n
        assert!(sanitized.contains("\\n"), "Raw newline should be escaped");
        let val: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        assert!(val["key"].as_str().unwrap().contains('\n'));
    }

    #[test]
    fn test_sanitize_json_string_lossy_control_char_tab() {
        let input = "{\"key\": \"col1\tcol2\"}";
        let sanitized = sanitize_json_string_lossy(input);
        assert!(sanitized.contains("\\t"), "Raw tab should be escaped");
        let val: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        assert!(val["key"].as_str().unwrap().contains('\t'));
    }

    #[test]
    fn test_sanitize_json_string_lossy_control_char_carriage_return() {
        let input = "{\"key\": \"line1\rline2\"}";
        let sanitized = sanitize_json_string_lossy(input);
        assert!(
            sanitized.contains("\\r"),
            "Raw carriage return should be escaped"
        );
        // Verify the parsed value contains the actual \r character
        let val: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        assert_eq!(val["key"].as_str().unwrap(), "line1\rline2");
    }

    #[test]
    fn test_sanitize_json_string_lossy_backspace_and_formfeed() {
        let input = "{\"key\": \"a\x08b\x0cc\"}";
        let sanitized = sanitize_json_string_lossy(input);
        assert!(sanitized.contains("\\b"), "Backspace should be escaped");
        assert!(sanitized.contains("\\f"), "Form feed should be escaped");
        let val: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        assert_eq!(val["key"].as_str().unwrap(), "a\x08b\x0cc");
    }

    #[test]
    fn test_sanitize_json_string_lossy_strips_other_control_chars() {
        // \x01 (SOH) is not \n, \r, \t, \b, or \f — should be stripped
        let input = "{\"key\": \"a\x01b\"}";
        let sanitized = sanitize_json_string_lossy(input);
        let val: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        assert_eq!(val["key"], "ab", "Unknown control char should be stripped");
    }

    #[test]
    fn test_sanitize_json_string_lossy_empty_input() {
        let result = sanitize_json_string_lossy("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_sanitize_json_string_lossy_no_backslashes() {
        let input = r#"{"key": "value"}"#;
        let result = sanitize_json_string_lossy(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_sanitize_json_string_lossy_preserves_valid_escapes() {
        // Valid escapes should be preserved: \", \/, \n, \r, \t
        // Note: the function processes each backslash independently (it doesn't
        // consume the peeked char), so a \\ pair is NOT treated as a single
        // escaped-backslash. Each \ is evaluated against what follows it.
        let input = r#"{"key": "quote\" slash\/ newline\n ret\r tab\t"}"#;
        let result = sanitize_json_string_lossy(input);
        assert!(result.contains(r#"\""#));
        assert!(result.contains(r"\/"));
        assert!(result.contains(r"\n"));
        assert!(result.contains(r"\r"));
        assert!(result.contains(r"\t"));

        // JSON round-trip: parse the sanitized output to verify escapes produce valid JSON
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("Sanitized output should be valid JSON");
        let val = parsed["key"].as_str().expect("key should be a string");
        assert!(val.contains("quote"), "Should contain 'quote'");
        assert!(
            val.contains("newline"),
            "Should contain 'newline' (decoded \\n)"
        );
        assert!(val.contains("tab"), "Should contain 'tab' (decoded \\t)");
    }

    // ---------------------------------------------------------------
    // repair_conversational_response — additional edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_repair_conversational_response_no_match() {
        let input = "This text has no THOUGHT or RESPONSE markers.";
        let result = repair_conversational_response(input);
        assert!(result.is_none());
    }

    #[test]
    fn test_repair_conversational_response_empty_sections() {
        let input = "THOUGHT:  RESPONSE: ";
        let result = repair_conversational_response(input);
        assert!(result.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["thought_process"], "");
        assert_eq!(parsed["solution_content"], "");
    }

    #[test]
    fn test_repair_conversational_response_multiline() {
        let input = "THOUGHT: I need to consider\nmultiple lines of\nreasoning.\n\nRESPONSE: The answer\nis 42.";
        let result = repair_conversational_response(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let thought = parsed["thought_process"].as_str().unwrap();
        assert!(thought.contains("multiple lines"));
        let content = parsed["solution_content"].as_str().unwrap();
        assert!(content.contains("42"));
    }

    // ---------------------------------------------------------------
    // repair_truncated_json — additional edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_repair_truncated_json_empty_input() {
        let repaired = repair_truncated_json("");
        assert_eq!(repaired, "");
    }

    #[test]
    fn test_repair_truncated_json_already_valid() {
        let input = r#"{"key": "value", "num": 42}"#;
        let repaired = repair_truncated_json(input);
        assert_eq!(repaired, input);
    }

    #[test]
    fn truncated_number_after_colon_is_not_silently_closed_to_wrong_value() {
        // "95" cut to "9" must NOT become a valid {"endorsement_weight": 9} — that
        // wrong score would flow silently into consensus. The dangling number is
        // stripped so the result fails to parse (→ rejected/retried).
        let out = repair_truncated_json(r#"{"endorsement_weight": 9"#);
        assert!(
            serde_json::from_str::<serde_json::Value>(&out).is_err(),
            "a truncated number must not repair to valid JSON, got {out:?}"
        );
        // Multi-field: the truncated last value is dropped, not accepted as-is.
        let out2 = repair_truncated_json(r#"{"a": 1, "score": 0.9"#);
        assert!(
            serde_json::from_str::<serde_json::Value>(&out2).is_err(),
            "trailing truncated number after a colon must invalidate, got {out2:?}"
        );
    }

    #[test]
    fn truncated_string_and_array_values_still_repair() {
        // The number-strip is colon-scoped: string values and array elements
        // (comma/bracket-preceded) are untouched and still close correctly.
        let s = repair_truncated_json(r#"{"a": "hello"#);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&s).unwrap()["a"],
            "hello"
        );
        let a = repair_truncated_json(r#"{"a": [1, 2"#);
        let v: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert_eq!(v["a"], serde_json::json!([1, 2]));
    }

    #[test]
    fn test_repair_truncated_json_open_array() {
        let input = r#"[1, 2, 3"#;
        let repaired = repair_truncated_json(input);
        assert_eq!(repaired, "[1, 2, 3]");
    }

    #[test]
    fn test_repair_truncated_json_mixed_open() {
        let input = r#"{"arr": [1, "two", {"nested": true"#;
        let repaired = repair_truncated_json(input);
        let val: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert!(val["arr"].is_array());
    }

    #[test]
    fn test_repair_truncated_json_dangling_backslash_inside_string() {
        let input = r#"{"key": "value\"#;
        let repaired = repair_truncated_json(input);
        // Should handle the dangling backslash (remove it), close string, close object
        let val: serde_json::Result<serde_json::Value> = serde_json::from_str(&repaired);
        assert!(val.is_ok(), "Should produce valid JSON: {}", repaired);
    }

    // ---------------------------------------------------------------
    // repair_invalid_escapes — additional edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_repair_invalid_escapes_empty_input() {
        assert_eq!(repair_invalid_escapes(""), "");
    }

    #[test]
    fn test_repair_invalid_escapes_no_backslashes() {
        let input = "plain text without any escapes";
        assert_eq!(repair_invalid_escapes(input), input);
    }

    #[test]
    fn test_repair_invalid_escapes_preserves_valid_json_escapes() {
        // Valid JSON escape sequences should be kept as-is.
        // Note: the function processes each backslash independently (no pair
        // consumption), so avoid \\ followed by a non-escape char in this test.
        let input = r#"{"val": "a\"b\/d\be\ff\ng\rh\ti\u0041"}"#;
        let result = repair_invalid_escapes(input);
        assert_eq!(result, input);
    }

    // ---------------------------------------------------------------
    // repair_aggressive_escapes — additional edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_repair_aggressive_escapes_empty() {
        assert_eq!(repair_aggressive_escapes(""), "");
    }

    #[test]
    fn test_repair_aggressive_escapes_no_backslashes() {
        let input = "plain text";
        assert_eq!(repair_aggressive_escapes(input), input);
    }

    #[test]
    fn test_repair_aggressive_escapes_preserves_quotes_and_backslashes() {
        // \" should be kept, \n gets double-escaped to \\n
        let input = r#"a\"b\nd"#;
        let result = repair_aggressive_escapes(input);
        assert!(result.contains(r#"a\"b"#));
        // \n gets double-escaped to \\n
        assert!(result.contains(r"\\n"));
    }

    // ---------------------------------------------------------------
    // repair_tool_calls — no tool_calls field
    // ---------------------------------------------------------------

    #[test]
    fn test_repair_tool_calls_no_tool_calls() {
        use async_openai::types::ChatCompletionResponseMessage;

        let mut response_message = ChatCompletionResponseMessage {
            content: Some("Regular content".to_string()),
            refusal: None,
            tool_calls: None,
            role: async_openai::types::Role::Assistant,
            #[allow(deprecated)]
            function_call: None,
            audio: None,
        };

        // Should not panic when tool_calls is None
        repair_tool_calls(&mut response_message, "test-agent");
        assert!(response_message.tool_calls.is_none());
    }

    #[test]
    fn test_repair_tool_calls_already_valid() {
        use async_openai::types::{
            ChatCompletionMessageToolCall, ChatCompletionResponseMessage, ChatCompletionToolType,
            FunctionCall,
        };

        let tool_call = ChatCompletionMessageToolCall {
            id: "call_valid".to_string(),
            r#type: ChatCompletionToolType::Function,
            function: FunctionCall {
                name: "submit_proposal".to_string(),
                arguments: r#"{"thought_process": "thinking", "solution_content": "answer"}"#
                    .to_string(),
            },
        };
        let original_args = tool_call.function.arguments.clone();

        let mut response_message = ChatCompletionResponseMessage {
            content: None,
            refusal: None,
            tool_calls: Some(vec![tool_call]),
            role: async_openai::types::Role::Assistant,
            #[allow(deprecated)]
            function_call: None,
            audio: None,
        };

        repair_tool_calls(&mut response_message, "test-agent");

        // Already-valid args should not be modified
        let calls = response_message.tool_calls.unwrap();
        assert_eq!(calls[0].function.arguments, original_args);
    }
}
