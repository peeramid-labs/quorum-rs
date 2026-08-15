//! Functions for extracting structured tool calls from unstructured text.

use super::repair::repair_truncated_json;
use super::utils::{clean_json_string, split_args_respecting_brackets};
use regex::Regex;
use rustpython_parser::{Mode, ast, parse};
use serde_json::Value;
use std::str::FromStr;
use std::sync::OnceLock;
use tracing::warn;

/// Extracts the content of a markdown code block.
/// Removes ``` delimiters and language identifier.
use async_openai::types::ChatCompletionMessageToolCall;

static PYTHON_CODE_BLOCK_RE: OnceLock<Regex> = OnceLock::new();
static GENERIC_CODE_BLOCK_RE: OnceLock<Regex> = OnceLock::new();

pub fn extract_code_block(content: &str) -> String {
    // Prioritize Python blocks
    let python_re =
        PYTHON_CODE_BLOCK_RE.get_or_init(|| Regex::new(r"(?si)```python\s*\n?(.*?)```").unwrap());
    if let Some(caps) = python_re.captures(content)
        && let Some(inner) = caps.get(1)
    {
        return inner.as_str().trim().to_string();
    }

    // Fallback to generic blocks
    let re =
        GENERIC_CODE_BLOCK_RE.get_or_init(|| Regex::new(r"(?s)```(?:\w+)?\s*\n?(.*?)```").unwrap());
    if let Some(caps) = re.captures(content)
        && let Some(inner) = caps.get(1)
    {
        return inner.as_str().trim().to_string();
    }
    content.trim().to_string()
}

/// Heuristically extracts a proposal from a Markdown-formatted report.
///
/// This is a fallback for models (like gpt-oss) that ignore tool usage instructions
/// and instead output a structured report with a thought process and a code block.
///
/// Returns a JSON string matching the `submit_proposal` schema:
/// `{ "thought_process": "...", "solution_content": "..." }`
pub fn extract_proposal_from_markdown(content: &str) -> Option<String> {
    // 1. Check if the content contains a code block.
    // If there is no code block, it's unlikely to be a valid proposal report.
    if !content.contains("```") {
        return None;
    }

    // 2. Extract the solution content (prioritizing Python).
    // We reuse extract_code_block but handle the case where it returns the whole string.
    let solution_content = extract_code_block(content);
    if solution_content.is_empty() || solution_content == content.trim() {
        // If extract_code_block returns the exact same content (trimmed), it means
        // no delimiters were found (or the whole thing is inside one, but we checked contains("```") above).
        // However, extract_code_block strips delimiters.
        // If the stripped content equals the original trimmed, it means no delimiters were stripped.
        // We want to enforce delimiters for this heuristic to avoid false positives on chatty text.
        return None;
    }

    // 3. Extract the thought process by removing the code blocks.
    // We want to capture the text *around* the code as the reasoning.
    // We replace code blocks with a placeholder to keep the text clean.
    let python_re =
        PYTHON_CODE_BLOCK_RE.get_or_init(|| Regex::new(r"(?si)```python\s*\n?(.*?)```").unwrap());
    let generic_re =
        GENERIC_CODE_BLOCK_RE.get_or_init(|| Regex::new(r"(?s)```(?:\w+)?\s*\n?(.*?)```").unwrap());

    let thought_process = if python_re.is_match(content) {
        python_re.replace_all(content, "\n[Code Solution Provided]\n")
    } else {
        generic_re.replace_all(content, "\n[Code Solution Provided]\n")
    };

    let thought_process = thought_process.trim().to_string();

    // 4. Construct the JSON object.
    let obj = serde_json::json!({
        "thought_process": thought_process,
        "solution_content": solution_content
    });

    Some(obj.to_string())
}

/// Heuristically extracts batch evaluations from a Markdown table.
///
/// Returns a JSON string matching the `submit_batch_evaluation` schema.
pub fn extract_evaluations_from_markdown(content: &str) -> Option<String> {
    let mut evaluations = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        // Must start and end with pipe to be a table row
        if !line.starts_with('|') || !line.ends_with('|') {
            continue;
        }

        // Skip separator lines
        if line.contains("---") {
            continue;
        }

        // Split by pipe
        // Example: "| Xue | 95 | Justification | true |"
        // Split gives: ["", " Xue ", " 95 ", " Justification ", " true ", ""]
        let parts: Vec<&str> = line.split('|').map(|s| s.trim()).collect();

        // We need at least: empty, id, weight, just, final, empty -> 6 parts
        if parts.len() < 6 {
            continue;
        }

        // Skip header
        if parts[1].eq_ignore_ascii_case("agent_id") {
            continue;
        }

        let agent_id = parts[1];
        if agent_id.is_empty() {
            continue;
        }

        // Parse weight — skip row if value is not a valid number
        let endorsement_weight = match parts[2].parse::<f32>() {
            Ok(w) => w,
            Err(_) => {
                warn!(
                    "Skipping evaluation row with malformed endorsement_weight: {:?}",
                    parts[2]
                );
                continue;
            }
        };

        // Anchor the fixed columns from BOTH ends so a literal `|` inside the
        // justification (a free-text middle column) doesn't shift `is_final` right and
        // silently drop a finalize vote. Layout:
        //   parts = ["", agent_id, weight, <justification …>, is_final, ""]
        // Justification is everything between weight and is_final; rejoin it with
        // " | " so an embedded pipe is preserved rather than truncating the cell.
        let justification = parts[3..parts.len() - 2].join(" | ");

        let is_final_str = parts[parts.len() - 2].to_lowercase();
        let is_final_solution =
            is_final_str == "true" || is_final_str == "yes" || is_final_str == "1";

        evaluations.push(serde_json::json!({
            "agent_id": agent_id,
            "endorsement_weight": endorsement_weight,
            "justification": justification,
            "is_final_solution": is_final_solution
        }));
    }

    if evaluations.is_empty() {
        return None;
    }

    Some(serde_json::json!({ "evaluations": evaluations }).to_string())
}

/// Extracts XML-style tool calls (Nous/Qwen format).
/// Format: <tool_call>{"name": "...", "arguments": { ... }}</tool_call>
pub fn extract_xml_tool_calls(
    content: &str,
) -> Vec<async_openai::types::ChatCompletionMessageToolCall> {
    static XML_TOOL_CALL_RE: OnceLock<Regex> = OnceLock::new();
    let re =
        XML_TOOL_CALL_RE.get_or_init(|| Regex::new(r"(?s)<tool_call>(.*?)</tool_call>").unwrap());
    let mut calls = Vec::new();

    for caps in re.captures_iter(content) {
        if let Some(json_str) = caps.get(1) {
            let json_str = json_str.as_str().trim();
            // Handle optional code block wrapping inside tool_call (sometimes used for code interpreter)
            // But standard Nous format is just JSON.

            // Try parsing raw first, then fallback to repair if truncated (common with large reasoning blocks)
            let value_res = serde_json::from_str::<Value>(json_str);
            let value = if let Ok(v) = value_res {
                Some(v)
            } else {
                let repaired = repair_truncated_json(json_str);
                serde_json::from_str::<Value>(&repaired).ok()
            };

            if let Some(value) = value
                && let Some(obj) = value.as_object()
            {
                let name = obj
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                let args_val = obj.get("arguments").unwrap_or(&Value::Null);
                let args_str = if args_val.is_string() {
                    args_val.as_str().unwrap().to_string()
                } else {
                    serde_json::to_string(args_val).unwrap_or_default()
                };

                calls.push(async_openai::types::ChatCompletionMessageToolCall {
                    id: format!("call_xml_{}", uuid::Uuid::new_v4().simple()),
                    r#type: async_openai::types::ChatCompletionToolType::Function,
                    function: async_openai::types::FunctionCall {
                        name,
                        arguments: args_str,
                    },
                });
            }
        }
    }
    calls
}

/// Parse the `[round], <id>` argument shape shared by `read_proposal`
/// (`id_key = "agent_id"`) and `read_critiques` (`id_key = "target_agent_id"`).
/// JSON object args win verbatim; otherwise `key=value` kwargs are inserted as
/// typed values and bare positionals map to `round` (when numeric first) + the
/// id key. The only difference between the two tools is `id_key`.
fn parse_round_and_id_args(
    args_str: &str,
    id_key: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut arguments = serde_json::Map::new();
    if args_str.trim().starts_with('{')
        && let Ok(serde_json::Value::Object(map)) = serde_json::from_str(args_str)
    {
        arguments = map;
    }
    if arguments.is_empty() {
        let parts = split_args_respecting_brackets(args_str);
        let mut positional_args = Vec::new();
        for part in &parts {
            if let Some((key, raw_value)) = part.split_once('=') {
                let key = key.trim();
                let kw_value = raw_value.trim().trim_matches(|c| c == '"' || c == '\'');
                if let Ok(num) = kw_value.parse::<u32>() {
                    arguments.insert(key.to_string(), serde_json::json!(num));
                } else {
                    arguments.insert(key.to_string(), serde_json::json!(kw_value));
                }
            } else {
                positional_args.push(part.trim());
            }
        }
        if !positional_args.is_empty() {
            if positional_args.len() >= 2 {
                if let Ok(round_num) = positional_args[0].parse::<u32>() {
                    arguments.insert("round".to_string(), serde_json::json!(round_num));
                }
                let id_val = positional_args[1].trim_matches(|c| c == '"' || c == '\'');
                arguments.insert(id_key.to_string(), serde_json::json!(id_val));
            } else if positional_args.len() == 1 {
                let single_id = positional_args[0].trim_matches(|c| c == '"' || c == '\'');
                arguments.insert(id_key.to_string(), serde_json::json!(single_id));
            }
        }
    }
    arguments
}

/// Extracts Python-style function calls (e.g., `func(arg="val")`) from a text block.
///
/// This handles:
/// 1. Markdown code blocks (e.g., ```python ... ```)
/// 2. Raw text calls
/// 3. Command-style calls (e.g., "ToolName\nkey: value")
/// 4. Special handling for specific NSED tools like `update_scratchpad`.
pub fn extract_python_tool_calls(
    content: &str,
) -> Vec<async_openai::types::ChatCompletionMessageToolCall> {
    let mut calls = Vec::new();

    // 1. Use full content to find tool calls, do not restrict to code blocks.
    let search_content = content;

    // 2. Parse function calls: name(arg1, "arg2") manually to handle nested parens
    // Regex just finds the start: name(
    static CALL_START_RE: OnceLock<Regex> = OnceLock::new();
    let call_start_re = CALL_START_RE.get_or_init(|| Regex::new(r#"(?s)\b(\w+)\s*\("#).unwrap());

    // 3. Parse "Command style" calls (seen in Ministral/Gemma):
    static CMD_RE: OnceLock<Regex> = OnceLock::new();
    let cmd_re = CMD_RE.get_or_init(|| Regex::new(r#"(?m)^(\w+)\s*\n((?:.*:\s*.*\n?)+)"#).unwrap());

    // Combined iteration
    let mut found_any = false;

    for mat in call_start_re.find_iter(search_content) {
        let start_idx = mat.start();
        let name_end = mat.end() - 1; // before '('
        let name = search_content[start_idx..name_end].trim().to_string();
        let args_start = mat.end();

        // Scan for matching closing parenthesis
        let mut depth = 0;
        let mut in_quote = false;
        let mut quote_char = '\0';
        let mut escape = false;
        let mut args_end = args_start;
        let mut found_end = false;

        let chars_iter = search_content[args_start..].chars();
        let mut current_offset = 0;

        for c in chars_iter {
            let char_len = c.len_utf8();
            current_offset += char_len;

            if escape {
                escape = false;
                continue;
            }
            if c == '\\' {
                escape = true;
                continue;
            }
            if in_quote {
                if c == quote_char {
                    in_quote = false;
                }
            } else {
                match c {
                    '"' | '\'' => {
                        in_quote = true;
                        quote_char = c;
                    }
                    '(' | '{' | '[' => depth += 1,
                    ')' => {
                        if depth == 0 {
                            // FOUND IT
                            args_end = args_start + current_offset - char_len; // Exclude ')'
                            found_end = true;
                            break;
                        } else {
                            depth -= 1;
                        }
                    }
                    '}' | ']' if depth > 0 => {
                        depth -= 1;
                    }
                    _ => {}
                }
            }
        }

        if !found_end {
            // Check if it looks like a truncated call (started but didn't finish)
            // If we are at the end of the content, assume truncation and try to recover.
            if args_start + current_offset >= search_content.len() {
                args_end = search_content.len();
                found_end = true;
            }
        }

        if found_end {
            let args_str = &search_content[args_start..args_end];
            let mut arguments = serde_json::Map::new();
            // This handles: read_proposal({"round": 1, "agent_id": "Xue"})
            if name == "read_proposal" {
                arguments = parse_round_and_id_args(args_str, "agent_id");
            } else if name == "read_critiques" {
                arguments = parse_round_and_id_args(args_str, "target_agent_id");
            } else if name == "read_own_proposal" {
                // No mandatory args
            } else if name == "submit_proposal"
                || name == "submit_batch_evaluation"
                || name == "update_scratchpad"
            {
                // Attempt to parse kwargs style: key=value
                let parts = split_args_respecting_brackets(args_str);
                for part in parts {
                    if let Some((key, val)) = part.split_once('=') {
                        let key = key.trim();
                        let val = val.trim();
                        // If val looks like JSON (starts with { or [), try to parse it
                        if (val.starts_with('{') && val.ends_with('}'))
                            || (val.starts_with('[') && val.ends_with(']'))
                        {
                            if let Some(json_val) = parse_json_or_python_literal(val) {
                                arguments.insert(key.to_string(), json_val);
                            } else {
                                // Fallback to string if parse fails
                                arguments.insert(key.to_string(), serde_json::json!(val));
                            }
                        } else {
                            // Simple string/number
                            let val_clean = val.trim_matches(|c| c == '"' || c == '\'');
                            // If it looks like a structure after stripping quotes, try parsing it
                            if ((val_clean.starts_with('{') && val_clean.ends_with('}'))
                                || (val_clean.starts_with('[') && val_clean.ends_with(']')))
                                && let Some(parsed) = parse_json_or_python_literal(val_clean)
                            {
                                arguments.insert(key.to_string(), parsed);
                                continue;
                            }
                            arguments.insert(key.to_string(), serde_json::json!(val_clean));
                        }
                    } else if name == "update_scratchpad" {
                        // Positional argument for update_scratchpad is 'content'
                        let val = part.trim();
                        let val_clean = val.trim_matches(|c| c == '"' || c == '\'');
                        // If it's the first part, assume it's content
                        if !arguments.contains_key("content") {
                            arguments.insert("content".to_string(), serde_json::json!(val_clean));
                            arguments.insert("mode".to_string(), serde_json::json!("append"));
                        }
                    }
                }
            } else {
                continue;
            }

            if !arguments.is_empty() || name == "read_own_proposal" {
                found_any = true;
                calls.push(async_openai::types::ChatCompletionMessageToolCall {
                    id: format!("call_{}", uuid::Uuid::new_v4()),
                    r#type: async_openai::types::ChatCompletionToolType::Function,
                    function: async_openai::types::FunctionCall {
                        name,
                        arguments: serde_json::to_string(&arguments).unwrap_or_default(),
                    },
                });
            }
        }
    }

    if !found_any {
        // 4. Parse "Backtick Label" style: `ToolName`: ... ```code```
        // Pattern: optional backticks around name, optional colon, code block.
        // Example: `update_scratchpad`: \n ```...```
        static BACKTICK_LABEL_RE: OnceLock<Regex> = OnceLock::new();
        let backtick_label_re = BACKTICK_LABEL_RE.get_or_init(|| {
            Regex::new(r#"(?ms)^`?(\w+)`?:?\s*```(?:json|python)?\s*\n?(.*?)```"#).unwrap()
        });

        for caps in backtick_label_re.captures_iter(search_content) {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let content_block = caps
                .get(2)
                .map(|m| m.as_str())
                .unwrap_or("")
                .trim()
                .to_string();

            let mut arguments = serde_json::Map::new();

            // Logic to convert content_block to arguments
            // If it looks like JSON, try parsing it as arguments
            if content_block.starts_with('{')
                && content_block.ends_with('}')
                && let Ok(json_args) =
                    serde_json::from_str::<serde_json::Map<String, Value>>(&content_block)
            {
                arguments = json_args;
            }

            // Special handling for update_scratchpad (it takes 'content')
            if arguments.is_empty() && name == "update_scratchpad" {
                // Treat the whole block as the scratchpad content
                arguments.insert("content".to_string(), serde_json::json!(content_block));
                arguments.insert("mode".to_string(), serde_json::json!("append"));
            }

            if !arguments.is_empty() {
                found_any = true;
                calls.push(async_openai::types::ChatCompletionMessageToolCall {
                    id: format!("call_{}", uuid::Uuid::new_v4()),
                    r#type: async_openai::types::ChatCompletionToolType::Function,
                    function: async_openai::types::FunctionCall {
                        name: name.clone(),
                        arguments: serde_json::to_string(&arguments).unwrap_or_default(),
                    },
                });
            }
        }
    }

    if !found_any {
        // Try Command Style Parsing
        if let Some(caps) = cmd_re.captures(search_content) {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let args_block = caps.get(2).map(|m| m.as_str()).unwrap_or("");

            let mut arguments = serde_json::Map::new();

            // Parse lines like `key: value` or `key: "value"`
            for line in args_block.lines() {
                if let Some((key, val)) = line.split_once(':') {
                    let key = key.trim();
                    let val = val.trim();
                    // Remove quotes if present
                    let val_clean = val.trim_matches(|c| c == '"' || c == '\'');

                    // Simple heuristic for types
                    if let Ok(num) = val_clean.parse::<f64>() {
                        // Check if it's actually an integer (no decimal point or .0)
                        if val_clean.contains('.') {
                            arguments.insert(key.to_string(), serde_json::json!(num));
                        } else {
                            // integer
                            if let Ok(int_val) = val_clean.parse::<i64>() {
                                arguments.insert(key.to_string(), serde_json::json!(int_val));
                            } else {
                                arguments.insert(key.to_string(), serde_json::json!(num));
                            }
                        }
                    } else if val_clean == "true" {
                        arguments.insert(key.to_string(), serde_json::json!(true));
                    } else if val_clean == "false" {
                        arguments.insert(key.to_string(), serde_json::json!(false));
                    } else {
                        arguments.insert(key.to_string(), serde_json::json!(val_clean));
                    }
                }
            }

            if !arguments.is_empty() {
                calls.push(async_openai::types::ChatCompletionMessageToolCall {
                    id: format!("call_{}", uuid::Uuid::new_v4()),
                    r#type: async_openai::types::ChatCompletionToolType::Function,
                    function: async_openai::types::FunctionCall {
                        name,
                        arguments: serde_json::to_string(&arguments).unwrap_or_default(),
                    },
                });
            }
        }
    }

    if calls.is_empty() {
        // 5. Heuristic: Check for raw JSON arguments for specific tools
        // This handles models that output valid JSON but forget the tool wrapper.
        // We use clean_json_string to extract the main JSON object if present.
        let json_candidate = clean_json_string(search_content, false, None);
        if let Ok(json_args) = serde_json::from_str::<serde_json::Value>(&json_candidate)
            && let Some(obj) = json_args.as_object()
        {
            warn!("No tool calls found; attempting heuristic JSON extraction");
            let mut name = None;
            if obj.contains_key("evaluations") {
                name = Some("submit_batch_evaluation");
            } else if obj.contains_key("solution_content") {
                name = Some("submit_proposal");
            } else if obj.contains_key("content") && obj.contains_key("mode") {
                name = Some("update_scratchpad");
            }

            if let Some(tool_name) = name {
                let mut final_args = json_args.clone();

                // Fix for models that stringify JSON arrays/objects (e.g. "evaluations": "[{'candidate_id':...}]")
                // and use single quotes (common Pythonism from weak models).
                if tool_name == "submit_batch_evaluation"
                    && let Some(evals) = final_args.get("evaluations").and_then(|v| v.as_str())
                    && let Some(parsed) = parse_json_or_python_literal(evals)
                {
                    final_args["evaluations"] = parsed;
                }

                calls.push(async_openai::types::ChatCompletionMessageToolCall {
                    id: format!("call_{}", uuid::Uuid::new_v4()),
                    r#type: async_openai::types::ChatCompletionToolType::Function,
                    function: async_openai::types::FunctionCall {
                        name: tool_name.to_string(),
                        arguments: final_args.to_string(),
                    },
                });
            }
        }
    }

    calls
}

fn parse_json_or_python_literal(input: &str) -> Option<Value> {
    // 1. Try Standard JSON first
    if let Ok(parsed) = serde_json::from_str::<Value>(input) {
        return Some(parsed);
    }

    // 2. Fallback: Parse as Python literal using rustpython-parser
    // (handles True/False/None, single-quoted strings, etc.)
    match parse(input, Mode::Expression, "<string>") {
        Ok(ast::Mod::Expression(expr)) => match python_ast_to_json(&expr.body) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                warn!(
                    "Failed to convert Python AST to JSON: {}. String: {}",
                    e, input
                );
                None
            }
        },
        Ok(_) => {
            warn!("Parsed Python code was not an expression: {}", input);
            None
        }
        Err(e) => {
            // Only warn if it really looked like structure but failed
            if input.trim().starts_with('{') || input.trim().starts_with('[') {
                warn!(
                    "Failed to parse potential Python literal: {}. String: {}",
                    e, input
                );
            }
            None
        }
    }
}

/// Max nesting the Python-literal → JSON traversal will descend. Matches the
/// depth-50 cap on the sibling `heuristic_json_tool_calls_recursive`. Deeply
/// nested adversarial input (`[[[[…]]]]`) would otherwise recurse one stack frame
/// per level and overflow.
const MAX_PY_AST_DEPTH: usize = 50;

fn python_ast_to_json(expr: &ast::Expr) -> anyhow::Result<Value> {
    python_ast_to_json_depth(expr, 0)
}

fn python_ast_to_json_depth(expr: &ast::Expr, depth: usize) -> anyhow::Result<Value> {
    if depth > MAX_PY_AST_DEPTH {
        return Err(anyhow::anyhow!(
            "Python literal nesting exceeds max depth {MAX_PY_AST_DEPTH}"
        ));
    }
    match expr {
        ast::Expr::Constant(ast::ExprConstant { value, .. }) => match value {
            ast::Constant::Str(s) => Ok(Value::String(s.clone())),
            ast::Constant::Int(i) => {
                Ok(Value::Number(serde_json::Number::from_str(&i.to_string())?))
            }
            ast::Constant::Float(f) => Ok(Value::Number(
                serde_json::Number::from_f64(*f).ok_or_else(|| anyhow::anyhow!("Invalid float"))?,
            )),
            ast::Constant::Bool(b) => Ok(Value::Bool(*b)),
            ast::Constant::None => Ok(Value::Null),
            _ => Err(anyhow::anyhow!("Unsupported constant type")),
        },
        ast::Expr::List(ast::ExprList { elts, .. }) => {
            let mut arr = Vec::new();
            for elt in elts {
                arr.push(python_ast_to_json_depth(elt, depth + 1)?);
            }
            Ok(Value::Array(arr))
        }
        ast::Expr::Dict(ast::ExprDict { keys, values, .. }) => {
            let mut obj = serde_json::Map::new();
            for (key, value) in keys.iter().zip(values.iter()) {
                if let Some(key_expr) = key {
                    // Keys must be strings in JSON
                    let key_json = python_ast_to_json_depth(key_expr, depth + 1)?;
                    if let Value::String(key_str) = key_json {
                        obj.insert(key_str, python_ast_to_json_depth(value, depth + 1)?);
                    } else {
                        return Err(anyhow::anyhow!("Dict keys must be strings"));
                    }
                }
            }
            Ok(Value::Object(obj))
        }
        // Bare identifier — Gemma family models emit `{agent_id: "value"}`
        // and `is_final_solution: false` with unquoted keys and lowercase
        // boolean literals. The Python parser represents these as
        // `Expr::Name`. Map known boolean/null identifiers to their JSON
        // equivalents; everything else becomes a string (for dict keys).
        ast::Expr::Name(ast::ExprName { id, .. }) => match id.as_str() {
            "true" | "True" => Ok(Value::Bool(true)),
            "false" | "False" => Ok(Value::Bool(false)),
            "None" | "null" | "none" => Ok(Value::Null),
            _ => Ok(Value::String(id.to_string())),
        },
        _ => Err(anyhow::anyhow!("Unsupported AST node type: {expr:?}")),
    }
}

/// Detects implicit tool calls where the model outputs a raw JSON object
/// that matches the signature of a known NSED tool (e.g. read_proposal, submit_proposal).
pub fn heuristic_json_tool_calls(content: &str) -> Vec<ChatCompletionMessageToolCall> {
    heuristic_json_tool_calls_recursive(content, 0)
}

fn heuristic_json_tool_calls_recursive(
    content: &str,
    depth: usize,
) -> Vec<ChatCompletionMessageToolCall> {
    // Prevent stack overflow from deeply nested JSON strings
    if depth > 50 {
        return Vec::new();
    }

    let mut calls = Vec::new();

    // Extract potential JSON candidates using bracket counting that respects strings
    let mut candidates = Vec::new();
    let mut stack = 0;
    let mut start = None;
    let mut in_string = false;
    let mut escape = false;

    for (i, c) in content.char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if c == '{' {
            if stack == 0 {
                start = Some(i);
            }
            stack += 1;
        } else if c == '}' {
            stack -= 1;
            if stack == 0 {
                if let Some(s) = start {
                    candidates.push(&content[s..=i]);
                    start = None;
                }
            } else if stack < 0 {
                // Unbalanced, reset
                stack = 0;
                start = None;
            }
        }
    }

    for candidate in candidates {
        if let Ok(json) = serde_json::from_str::<Value>(candidate)
            && let Some(obj) = json.as_object()
        {
            let args = candidate.to_string();

            // Heuristic 0: Wrapped tool call { "tool": "name", "args": { ... } }
            // Some models (gpt-oss) output this format even when asked for direct JSON.
            // Also supports "arguments" instead of "args".
            if let Some(tool_name) = obj.get("tool").and_then(|t| t.as_str())
                && let Some(tool_args) = obj.get("args").or_else(|| obj.get("arguments"))
            {
                let args_str = if tool_args.is_string() {
                    tool_args.as_str().unwrap().to_string()
                } else {
                    serde_json::to_string(tool_args).unwrap_or_default()
                };

                calls.push(ChatCompletionMessageToolCall {
                    id: format!("call_heuristic_{}", uuid::Uuid::new_v4().simple()),
                    r#type: async_openai::types::ChatCompletionToolType::Function,
                    function: async_openai::types::FunctionCall {
                        name: tool_name.to_string(),
                        arguments: args_str,
                    },
                });
            }
            // Heuristic 1: read_proposal (agent_id + round, and NOT carrying a
            // proposal payload). Some models echo their own agent_id/round alongside
            // the proposal; without this guard such a proposal is misclassified as a
            // research read and its payload silently discarded. submit_proposal
            // (Heuristic 4) is the more-specific match, so defer to it.
            else if obj.contains_key("agent_id")
                && obj.contains_key("round")
                && !obj.contains_key("thought_process")
                && !obj.contains_key("solution_content")
            {
                calls.push(ChatCompletionMessageToolCall {
                    id: format!("call_heuristic_{}", uuid::Uuid::new_v4().simple()),
                    r#type: async_openai::types::ChatCompletionToolType::Function,
                    function: async_openai::types::FunctionCall {
                        name: "read_proposal".to_string(),
                        arguments: args,
                    },
                });
            }
            // Heuristic 2: submit_batch_evaluation (has evaluations list)
            else if obj.contains_key("evaluations") {
                calls.push(ChatCompletionMessageToolCall {
                    id: format!("call_heuristic_{}", uuid::Uuid::new_v4().simple()),
                    r#type: async_openai::types::ChatCompletionToolType::Function,
                    function: async_openai::types::FunctionCall {
                        name: "submit_batch_evaluation".to_string(),
                        arguments: args,
                    },
                });
            }
            // Heuristic 3: update_scratchpad (has content + mode)
            else if obj.contains_key("content")
                && obj.contains_key("mode")
                && obj
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "append" || s == "overwrite")
                    .unwrap_or(false)
            {
                calls.push(ChatCompletionMessageToolCall {
                    id: format!("call_heuristic_{}", uuid::Uuid::new_v4().simple()),
                    r#type: async_openai::types::ChatCompletionToolType::Function,
                    function: async_openai::types::FunctionCall {
                        name: "update_scratchpad".to_string(),
                        arguments: args,
                    },
                });
            }
            // Heuristic 4: submit_proposal (has thought_process + solution_content)
            else if obj.contains_key("thought_process") && obj.contains_key("solution_content") {
                calls.push(ChatCompletionMessageToolCall {
                    id: format!("call_heuristic_{}", uuid::Uuid::new_v4().simple()),
                    r#type: async_openai::types::ChatCompletionToolType::Function,
                    function: async_openai::types::FunctionCall {
                        name: "submit_proposal".to_string(),
                        arguments: args,
                    },
                });
            }
            // Heuristic 5: Deep Unwrap (Model put JSON inside a key or value)
            else {
                // Check keys
                for key in obj.keys() {
                    if key.trim().starts_with('{')
                        && let Ok(inner) = serde_json::from_str::<Value>(key)
                        && inner.as_object().is_some()
                    {
                        let inner_calls = heuristic_json_tool_calls_recursive(key, depth + 1);
                        calls.extend(inner_calls);
                    }
                }
                // Check string values
                for val in obj.values() {
                    if let Some(s) = val.as_str()
                        && s.trim().starts_with('{')
                        && let Ok(inner) = serde_json::from_str::<Value>(s)
                        && inner.as_object().is_some()
                    {
                        let inner_calls = heuristic_json_tool_calls_recursive(s, depth + 1);
                        calls.extend(inner_calls);
                    }
                }
            }
        }
    }

    calls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heuristic_json_tool_calls_complex() {
        // Case 1: Concatenated JSONs (The "Jaya" failure mode)
        let input = r#"
            {"agent_id": "Xue", "round": 2}
            {"evaluations": [{"agent_id": "Xue", "endorsement_weight": 95, "is_final_solution": false, "justification": "Good"}]}
        "#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 2, "Should find both tool calls");
        assert_eq!(calls[0].function.name, "read_proposal");
        assert_eq!(calls[1].function.name, "submit_batch_evaluation");

        // Case 2: Buried JSON in text
        let input_text = r#"
            Here is my reasoning.
            <think>Thinking...</think>
            I will now update my scratchpad.
            {"content": "New plan", "mode": "append"}
            And I will also read the proposal.
            {"agent_id": "Alic", "round": 1}
        "#;
        let calls_text = heuristic_json_tool_calls(input_text);
        assert_eq!(
            calls_text.len(),
            2,
            "Should find buried update_scratchpad and read_proposal"
        );
        assert_eq!(calls_text[0].function.name, "update_scratchpad");
        assert_eq!(calls_text[1].function.name, "read_proposal");

        // Case 3: Nested JSON objects (should not be confused)
        // Heuristic looks for specific top-level keys.
        let input_nested = r#"
            {"wrapper": {"agent_id": "Xue", "round": 2}}
        "#;
        // This should NOT match because "agent_id" is not a top-level key of the extracted JSON object.
        // Wait, bracket counting extracts the outer object.
        // Outer object has key "wrapper". Not "agent_id". So it should be ignored.
        let calls_nested = heuristic_json_tool_calls(input_nested);
        assert_eq!(
            calls_nested.len(),
            0,
            "Should ignore nested non-matching JSON"
        );

        // Case 4: Malformed/Partial JSON (should be ignored)
        let input_malformed = r#"
            {"agent_id": "Xue", "round": 2
        "#;
        let calls_malformed = heuristic_json_tool_calls(input_malformed);
        assert_eq!(calls_malformed.len(), 0, "Should ignore malformed JSON");
    }

    #[test]
    fn test_extract_command_style_tool_calls() {
        let input = r#"
```
submit_proposal
thought_process: "I have solved it."
solution_content: 70
```
"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.function.name, "submit_proposal");

        let args: serde_json::Value = serde_json::from_str(&call.function.arguments).unwrap();
        assert_eq!(args["thought_process"], "I have solved it.");
        assert_eq!(args["solution_content"], 70); // Parsed as number
    }

    #[test]
    fn test_python_literal_parsing_robustness() {
        let input = r#"
        ```python
        submit_batch_evaluation(evaluations="[{'id': 'test1', 'valid': True, 'msg': 'It\'s working'}, {'id': 'test2', 'valid': False, 'data': None}]")
        ```
        "#;

        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);

        let args: Value = serde_json::from_str(&calls[0].function.arguments)
            .expect("Arguments should be valid JSON");
        let evals = args["evaluations"]
            .as_array()
            .expect("evaluations should be an array");

        assert_eq!(evals.len(), 2);

        // Check first item
        assert_eq!(evals[0]["id"], "test1");
        assert_eq!(evals[0]["valid"], true);
        assert_eq!(evals[0]["msg"], "It's working"); // Check escaped quote handling

        // Check second item
        assert_eq!(evals[1]["id"], "test2");
        assert_eq!(evals[1]["valid"], false);
        assert_eq!(evals[1]["data"], Value::Null);
    }

    #[test]
    fn test_extract_read_proposal_variants() {
        // Case 1: Positional args (round, agent_id)
        let input_pos = "[read_proposal(1, \"agent_0\")]";
        let calls = extract_python_tool_calls(input_pos);
        assert_eq!(calls.len(), 1);
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["round"], 1);
        assert_eq!(args["agent_id"], "agent_0");

        // Case 2: Named args (agent_id="agent_0") - Default round
        let input_named = "[read_proposal(agent_id=\"agent_0\")]";
        let calls = extract_python_tool_calls(input_named);
        assert_eq!(calls.len(), 1);
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["agent_id"], "agent_0");
        assert!(args.get("round").is_none());

        // Case 3: Named args with round (agent_id="agent_0", round=2)
        let input_named_round = "[read_proposal(agent_id=\"agent_0\", round=2)]";
        let calls = extract_python_tool_calls(input_named_round);
        assert_eq!(calls.len(), 1);
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["agent_id"], "agent_0");
        assert_eq!(args["round"], 2);

        // Case 4: Single positional arg (agent_id only) - Should be supported per fix
        let input_single_pos = "[read_proposal(\"agent_0\")]";
        let calls = extract_python_tool_calls(input_single_pos);
        assert_eq!(calls.len(), 1);
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["agent_id"], "agent_0");
    }

    #[test]
    fn test_python_parsing_edge_cases() {
        // Mixed quotes
        let input_mixed = r#"
        ```python
        submit_batch_evaluation(evaluations="[{'k1': "v1's"}, {"k2": 'v2"s'}]")
        ```
        "#;
        let calls = extract_python_tool_calls(input_mixed);
        assert_eq!(calls.len(), 1);
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["evaluations"][0]["k1"], "v1's");
        assert_eq!(args["evaluations"][1]["k2"], "v2\"s");

        // Invalid Python syntax (should gracefully fail/warn but return string)
        let input_invalid = r#"
        ```python
        submit_batch_evaluation(evaluations="[{'k1': 'unclosed string...]")
        ```
        "#;
        let calls_inv = extract_python_tool_calls(input_invalid);
        assert_eq!(calls_inv.len(), 1);
        let args_inv: Value = serde_json::from_str(&calls_inv[0].function.arguments).unwrap();
        // Should fall back to raw string because parsing failed
        assert!(args_inv["evaluations"].is_string());
        assert!(
            args_inv["evaluations"]
                .as_str()
                .unwrap()
                .contains("unclosed string")
        );
    }

    #[test]
    fn test_read_critiques_json_not_overwritten_by_positional() {
        // JSON-style arguments should be preserved, not overwritten by positional parsing.
        // This is a regression test: previously, positional parsing would run unconditionally
        // and clobber the JSON-parsed arguments even when JSON parsing succeeded.
        // The bug was in extract_python_tool_calls where read_critiques({...}) had the
        // JSON args parsed, but then the positional fallback ran unconditionally and overwrote them.
        let text = r#"read_critiques({"round": 2, "target_agent_id": "agent-1"})"#;
        let calls = extract_python_tool_calls(text);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["round"], 2);
        assert_eq!(args["target_agent_id"], "agent-1");
    }

    #[test]
    fn test_heuristic_json_tool_calls_alic_style() {
        let input = r#"
Here is my evaluation:
{
  "evaluations": "[{'candidate_id': 'Xue', 'endorsement_weight': 95.0, 'is_final_solution': True, 'justification': 'Correct logic and safe because it doesn\\'t modify the input matrix.'}, {'candidate_id': 'Jaya', 'endorsement_weight': 80.0, 'is_final_solution': False, 'justification': 'Correct logic, but modifies the input matrix in place.'}]"
}
"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.function.name, "submit_batch_evaluation");

        // Verify arguments are valid JSON
        let args: serde_json::Value =
            serde_json::from_str(&call.function.arguments).expect("Arguments should be valid JSON");

        // Verify evaluations is an array, not a string
        let evals = args.get("evaluations").expect("Should have evaluations");
        assert!(evals.is_array(), "Evaluations should be parsed as array");

        let list = evals.as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["candidate_id"], "Xue");
        assert_eq!(list[0]["endorsement_weight"], 95.0);
        assert_eq!(list[0]["is_final_solution"], true);
        // The parser logic for string replacement handles delimiters, so inner escaped quotes might remain or need handling.
        // If the original string had `doesn\'t`, JSON string will contain `doesn't` if parsed correctly, or `doesn\'t`.
        let justification = list[0]["justification"].as_str().unwrap();
        assert!(justification.contains("Correct logic"));
        assert_eq!(list[1]["is_final_solution"], false);
    }

    /// Regression test: `found_any` must only be set when a *valid* tool call is
    /// actually pushed. Previously, any `word(...)` match (e.g. `print(...)`)
    /// would set `found_any = true` even though the name wasn't a known tool,
    /// blocking the backtick_label_re and cmd_re fallback parsers.
    #[test]
    fn test_found_any_not_set_by_unknown_function_calls() {
        // The text has a generic function call `print("hello")` which will match
        // `call_start_re` but is NOT a known tool. The actual tool call is in
        // backtick-label format, which should still be found.
        let input = r#"
I'll analyze the data using print("hello") for debugging.

`update_scratchpad`:
```json
{"content": "My analysis notes", "mode": "append"}
```
"#;
        let calls = extract_python_tool_calls(input);
        assert!(
            !calls.is_empty(),
            "Should find the backtick-label update_scratchpad call even though print(...) matched call_start_re"
        );
        assert_eq!(calls[0].function.name, "update_scratchpad");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["content"], "My analysis notes");
    }

    /// Regression: malformed endorsement_weight (e.g. "N/A") should skip the
    /// row instead of silently defaulting to 0.0 which is a valid score.
    #[test]
    fn test_malformed_endorsement_weight_skips_row() {
        let markdown = r#"
| agent_id | endorsement_weight | justification | is_final_solution |
|----------|-------------------|---------------|-------------------|
| Alice    | 85                | Good logic    | true              |
| Bob      | N/A               | Incomplete    | false             |
| Carol    | 70.5              | Decent work   | false             |
"#;
        let result = extract_evaluations_from_markdown(markdown).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let evals = parsed["evaluations"].as_array().expect("should be array");
        // Bob's row should be skipped due to malformed weight
        assert_eq!(evals.len(), 2, "Malformed weight row should be skipped");
        assert_eq!(evals[0]["agent_id"], "Alice");
        assert_eq!(evals[0]["endorsement_weight"], 85.0);
        assert_eq!(evals[1]["agent_id"], "Carol");
        assert_eq!(evals[1]["endorsement_weight"], 70.5);
    }

    /// Regression: `parse_json_or_python_literal` should not corrupt strings
    /// that happen to contain "True" or "False" as substrings. The old code did
    /// a blind `.replace("True", "true")` which would mangle quoted values.
    #[test]
    fn test_python_literal_preserves_true_in_strings() {
        // Python dict with boolean True AND the word "True" inside a string value
        let input = r#"{'status': True, 'message': 'TrueNorth is the best', 'flag': False}"#;
        let parsed = parse_json_or_python_literal(input).expect("Should parse Python literal");
        assert_eq!(
            parsed["status"], true,
            "Boolean True should become JSON true"
        );
        assert_eq!(
            parsed["flag"], false,
            "Boolean False should become JSON false"
        );
        assert_eq!(
            parsed["message"].as_str().unwrap(),
            "TrueNorth is the best",
            "String containing 'True' must not be corrupted"
        );
    }

    // ---------------------------------------------------------------
    // extract_code_block tests
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_code_block_json() {
        let input = "Here is the config:\n```json\n{\"key\": \"value\", \"num\": 42}\n```\nDone.";
        let result = extract_code_block(input);
        assert_eq!(result, "{\"key\": \"value\", \"num\": 42}");
    }

    #[test]
    fn test_extract_code_block_python() {
        let input = "Solution:\n```python\ndef solve():\n    return 42\n```\nEnd.";
        let result = extract_code_block(input);
        assert_eq!(result, "def solve():\n    return 42");
    }

    #[test]
    fn test_extract_code_block_generic() {
        let input = "Output:\n```\nsome plain text\nwith multiple lines\n```\nTrailing.";
        let result = extract_code_block(input);
        assert_eq!(result, "some plain text\nwith multiple lines");
    }

    #[test]
    fn test_extract_code_block_no_block() {
        let input = "Just some plain text without any code fences.";
        let result = extract_code_block(input);
        // When no code block is found, extract_code_block returns the trimmed input
        assert_eq!(result, input.trim());
    }

    #[test]
    fn test_extract_code_block_nested() {
        // Python block comes first and is prioritized by the regex
        let input = "First:\n```python\nprint('hello')\n```\nSecond:\n```json\n{\"a\": 1}\n```";
        let result = extract_code_block(input);
        assert_eq!(result, "print('hello')");
    }

    // ---------------------------------------------------------------
    // extract_proposal_from_markdown tests
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_proposal_from_markdown_with_solution() {
        let input = r#"## Thought Process
I analyzed the problem carefully.

## Solution
```python
def solve(n):
    return n * 2
```
"#;
        let result = extract_proposal_from_markdown(input).expect("Should extract proposal");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        // solution_content should be the code block content
        assert_eq!(
            parsed["solution_content"].as_str().unwrap(),
            "def solve(n):\n    return n * 2"
        );
        // thought_process should contain the markdown text with code replaced
        let thought = parsed["thought_process"].as_str().unwrap();
        assert!(thought.contains("Thought Process"));
        assert!(thought.contains("[Code Solution Provided]"));
        // The actual code should NOT appear in thought_process
        assert!(!thought.contains("def solve(n)"));
    }

    #[test]
    fn test_extract_proposal_from_markdown_with_code_block() {
        let input = r#"Here is my answer:
```json
{"result": 42, "explanation": "The answer to everything"}
```
"#;
        let result = extract_proposal_from_markdown(input).expect("Should extract proposal");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        let solution = parsed["solution_content"].as_str().unwrap();
        assert!(solution.contains("\"result\": 42"));
        assert!(solution.contains("\"explanation\""));
    }

    // ---------------------------------------------------------------
    // extract_evaluations_from_markdown tests
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_evaluations_from_markdown_basic() {
        let markdown = r#"
| agent_id | endorsement_weight | justification        | is_final_solution |
|----------|-------------------|----------------------|-------------------|
| Xue      | 95                | Excellent solution   | true              |
| Jaya     | 60                | Partial correctness  | false             |
| Alic     | 80.5              | Good but incomplete  | yes               |
"#;
        let result = extract_evaluations_from_markdown(markdown).expect("Should parse table");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let evals = parsed["evaluations"].as_array().unwrap();

        assert_eq!(evals.len(), 3);

        assert_eq!(evals[0]["agent_id"], "Xue");
        assert_eq!(evals[0]["endorsement_weight"], 95.0);
        assert_eq!(evals[0]["justification"], "Excellent solution");
        assert_eq!(evals[0]["is_final_solution"], true);

        assert_eq!(evals[1]["agent_id"], "Jaya");
        assert_eq!(evals[1]["endorsement_weight"], 60.0);
        assert_eq!(evals[1]["justification"], "Partial correctness");
        assert_eq!(evals[1]["is_final_solution"], false);

        assert_eq!(evals[2]["agent_id"], "Alic");
        assert_eq!(evals[2]["endorsement_weight"], 80.5);
        assert_eq!(evals[2]["is_final_solution"], true); // "yes" maps to true
    }

    #[test]
    fn markdown_pipe_in_justification_preserves_is_final() {
        // A literal `|` inside the justification cell must NOT shift is_final_solution
        // right (which silently dropped the finalize vote). Columns are anchored from
        // both ends; the middle rejoins with the pipe preserved.
        let markdown = "\
| agent_id | endorsement_weight | justification | is_final_solution |
|---|---|---|---|
| Xue | 95 | Good, but risky | maybe | true |
";
        let result = extract_evaluations_from_markdown(markdown).expect("parse");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let e = &parsed["evaluations"][0];
        assert_eq!(
            e["is_final_solution"], true,
            "the real finalize signal (last column) is read, not the mid-cell"
        );
        assert_eq!(
            e["justification"], "Good, but risky | maybe",
            "the embedded pipe is preserved in the justification"
        );
        assert_eq!(e["endorsement_weight"], 95.0);
    }

    #[test]
    fn test_extract_evaluations_from_markdown_missing_columns() {
        // Table with too few columns (only 3 cells instead of required 4+)
        let markdown = r#"
| agent_id | score |
|----------|-------|
| Xue      | 95    |
| Jaya     | 60    |
"#;
        let result = extract_evaluations_from_markdown(markdown);
        // Should return None because rows have fewer than 6 pipe-delimited parts
        assert!(
            result.is_none(),
            "Should return None for table with insufficient columns"
        );
    }

    // ---------------------------------------------------------------
    // extract_xml_tool_calls tests
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_xml_tool_calls_basic() {
        let input = r#"<tool_call>{"name":"foo","arguments":{"bar":"baz"}}</tool_call>"#;
        let calls = extract_xml_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "foo");

        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["bar"], "baz");
    }

    #[test]
    fn test_extract_xml_tool_calls_multiple() {
        let input = r#"
Some preamble text.
<tool_call>{"name":"read_proposal","arguments":{"agent_id":"Xue","round":1}}</tool_call>
Middle text.
<tool_call>{"name":"submit_proposal","arguments":{"thought_process":"Analyzed.","solution_content":"42"}}</tool_call>
Trailing text.
"#;
        let calls = extract_xml_tool_calls(input);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "read_proposal");
        assert_eq!(calls[1].function.name, "submit_proposal");

        let args0: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args0["agent_id"], "Xue");
        assert_eq!(args0["round"], 1);

        let args1: serde_json::Value = serde_json::from_str(&calls[1].function.arguments).unwrap();
        assert_eq!(args1["thought_process"], "Analyzed.");
        assert_eq!(args1["solution_content"], "42");
    }

    #[test]
    fn test_extract_xml_tool_calls_none() {
        let input = "This is just regular text with no tool calls at all.";
        let calls = extract_xml_tool_calls(input);
        assert!(
            calls.is_empty(),
            "Should return empty vec for input without tool_call XML"
        );
    }

    // ---------------------------------------------------------------
    // clean_json_string tests (via super::utils, re-exported)
    // ---------------------------------------------------------------

    #[test]
    fn test_clean_json_string_extracts_json() {
        let input = r#"Here is my analysis. The answer is {"thought_process": "I reasoned carefully", "solution_content": "42"} and that concludes it."#;
        let result = clean_json_string(input, false, None);
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("Extracted string should be valid JSON");
        assert_eq!(parsed["thought_process"], "I reasoned carefully");
        assert_eq!(parsed["solution_content"], "42");
    }

    #[test]
    fn test_clean_json_string_no_json() {
        let input = "Pure text without any JSON braces at all.";
        let result = clean_json_string(input, false, None);
        // When no braces are found, clean_json_string returns the original string
        assert_eq!(result, input);
    }

    // ---------------------------------------------------------------
    // extract_code_block fallback (no code block => returns trim)
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_code_block_returns_trimmed_when_no_fences() {
        let input = "   No code blocks here, just whitespace-padded text.   ";
        let result = extract_code_block(input);
        assert_eq!(result, "No code blocks here, just whitespace-padded text.");
    }

    // ---------------------------------------------------------------
    // extract_proposal_from_markdown — solution_content == content.trim()
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_proposal_from_markdown_no_code_block() {
        // No ``` delimiters at all => returns None
        let input = "Just some plain text without any code blocks.";
        let result = extract_proposal_from_markdown(input);
        assert!(
            result.is_none(),
            "Should return None when no code blocks present"
        );
    }

    #[test]
    fn test_extract_proposal_from_markdown_whole_content_is_code_block() {
        // The entire content is one code block. extract_code_block returns the inner
        // which should differ from content.trim() since delimiters are stripped.
        let input = "```python\ndef solve():\n    return 42\n```";
        let result = extract_proposal_from_markdown(input);
        assert!(
            result.is_some(),
            "Should extract when code block covers whole content"
        );
        let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(
            parsed["solution_content"].as_str().unwrap(),
            "def solve():\n    return 42"
        );
    }

    // ---------------------------------------------------------------
    // extract_evaluations_from_markdown — is_final_solution = false
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_evaluations_all_non_final() {
        let markdown = r#"
| agent_id | endorsement_weight | justification | is_final_solution |
|----------|-------------------|---------------|-------------------|
| Agent1   | 70                | Solid work    | false             |
| Agent2   | 85                | Great job     | no                |
| Agent3   | 50                | Needs work    | 0                 |
"#;
        let result = extract_evaluations_from_markdown(markdown).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let evals = parsed["evaluations"].as_array().unwrap();
        assert_eq!(evals.len(), 3);
        // "false", "no", and "0" should all map to false
        assert_eq!(evals[0]["is_final_solution"], false);
        assert_eq!(evals[1]["is_final_solution"], false);
        assert_eq!(evals[2]["is_final_solution"], false);
    }

    #[test]
    fn test_extract_evaluations_empty_table() {
        let markdown = r#"
| agent_id | endorsement_weight | justification | is_final_solution |
|----------|-------------------|---------------|-------------------|
"#;
        let result = extract_evaluations_from_markdown(markdown);
        assert!(
            result.is_none(),
            "Empty table (header only) should return None"
        );
    }

    #[test]
    fn test_extract_evaluations_non_table_content() {
        let markdown = "This is just a paragraph of text with no table formatting.";
        let result = extract_evaluations_from_markdown(markdown);
        assert!(result.is_none());
    }

    // ---------------------------------------------------------------
    // extract_xml_tool_calls — non-string args
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_xml_tool_calls_args_as_object() {
        // When arguments is an object (not a string), it should be serialized
        let input = r#"<tool_call>{"name":"submit_proposal","arguments":{"thought_process":"test","solution_content":"42"}}</tool_call>"#;
        let calls = extract_xml_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_proposal");
        // Arguments should be serialized JSON
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["thought_process"], "test");
        assert_eq!(args["solution_content"], "42");
    }

    #[test]
    fn test_extract_xml_tool_calls_args_as_number() {
        // When arguments is a number (non-string, non-object)
        let input = r#"<tool_call>{"name":"foo","arguments": 42}</tool_call>"#;
        let calls = extract_xml_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "foo");
        assert_eq!(calls[0].function.arguments, "42");
    }

    #[test]
    fn test_extract_xml_tool_calls_truncated_json() {
        // Truncated JSON inside tool_call — should be repaired
        let input = r#"<tool_call>{"name":"submit_proposal","arguments":{"thought_process":"test","solution_content":"ans</tool_call>"#;
        let calls = extract_xml_tool_calls(input);
        // Should attempt repair
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_proposal");
    }

    #[test]
    fn test_extract_xml_tool_calls_missing_name() {
        // JSON without "name" key => name defaults to "unknown"
        let input = r#"<tool_call>{"arguments":{"key":"val"}}</tool_call>"#;
        let calls = extract_xml_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "unknown");
    }

    // ---------------------------------------------------------------
    // extract_python_tool_calls — truncated/recovery path
    // ---------------------------------------------------------------

    #[test]
    fn test_python_tool_calls_truncated_recovery() {
        // Truncated call that reaches end of content without finding closing ')'
        let input =
            r#"submit_proposal(thought_process="thinking", solution_content="the answer is"#;
        let calls = extract_python_tool_calls(input);
        // Should recover by treating end-of-content as truncation point
        assert_eq!(calls.len(), 1, "Should recover truncated tool call");
        assert_eq!(calls[0].function.name, "submit_proposal");
    }

    // ---------------------------------------------------------------
    // extract_python_tool_calls — read_critiques positional args
    // ---------------------------------------------------------------

    #[test]
    fn test_read_critiques_positional_args() {
        // read_critiques(round, target_agent_id)
        let input = r#"read_critiques(1, "agent-A")"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_critiques");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["round"], 1);
        assert_eq!(args["target_agent_id"], "agent-A");
    }

    #[test]
    fn test_read_critiques_single_positional() {
        // read_critiques with single positional arg => target_agent_id
        let input = r#"read_critiques("agent-B")"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["target_agent_id"], "agent-B");
    }

    #[test]
    fn test_read_critiques_named_args() {
        let input = r#"read_critiques(target_agent_id="agent-C", round=3)"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["target_agent_id"], "agent-C");
        assert_eq!(args["round"], 3);
    }

    // ---------------------------------------------------------------
    // submit_proposal and update_scratchpad positional handling
    // ---------------------------------------------------------------

    #[test]
    fn test_submit_proposal_kwargs() {
        let input = r#"submit_proposal(thought_process="my thoughts", solution_content="42")"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["thought_process"], "my thoughts");
        assert_eq!(args["solution_content"], "42");
    }

    #[test]
    fn test_update_scratchpad_positional() {
        // update_scratchpad with a single positional arg should set content + mode=append
        let input = r#"update_scratchpad("My notes here")"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "update_scratchpad");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["content"], "My notes here");
        assert_eq!(args["mode"], "append");
    }

    #[test]
    fn test_update_scratchpad_kwargs() {
        let input = r#"update_scratchpad(content="notes", mode="overwrite")"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["content"], "notes");
        assert_eq!(args["mode"], "overwrite");
    }

    // ---------------------------------------------------------------
    // parse_json_or_python_literal
    // ---------------------------------------------------------------

    #[test]
    fn test_parse_json_or_python_literal_valid_json() {
        let input = r#"{"key": "value", "num": 42}"#;
        let result = parse_json_or_python_literal(input);
        assert!(result.is_some());
        let val = result.unwrap();
        assert_eq!(val["key"], "value");
        assert_eq!(val["num"], 42);
    }

    #[test]
    fn test_parse_json_or_python_literal_python_dict() {
        let input = r#"{'key': 'value', 'flag': True, 'nothing': None}"#;
        let result = parse_json_or_python_literal(input);
        assert!(result.is_some());
        let val = result.unwrap();
        assert_eq!(val["key"], "value");
        assert_eq!(val["flag"], true);
        assert_eq!(val["nothing"], serde_json::Value::Null);
    }

    #[test]
    fn test_parse_json_or_python_literal_python_list() {
        let input = "[1, 2, 'three', True]";
        let result = parse_json_or_python_literal(input);
        assert!(result.is_some());
        let arr = result.unwrap();
        assert!(arr.is_array());
        let list = arr.as_array().unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list[2], "three");
        assert_eq!(list[3], true);
    }

    #[test]
    fn test_parse_json_or_python_literal_invalid() {
        let input = "not json or python at all";
        let result = parse_json_or_python_literal(input);
        assert!(result.is_none());
    }

    #[test]
    fn python_literal_deep_nesting_is_capped_not_overflowed() {
        // Invalid JSON (bare `True`) forces the rustpython path; nesting past
        // MAX_PY_AST_DEPTH (50) must error out (→ None), not recurse the traversal
        // one frame per level into a stack overflow.
        let deep = format!("{}True{}", "[".repeat(60), "]".repeat(60));
        let result = parse_json_or_python_literal(&deep);
        assert!(result.is_none(), "deep nesting must be rejected, not crash");
    }

    // ---------------------------------------------------------------
    // submit_proposal with JSON-valued kwargs
    // ---------------------------------------------------------------

    #[test]
    fn test_submit_proposal_with_json_valued_arg() {
        let input =
            r#"submit_proposal(thought_process="reasoning", solution_content={"data": [1, 2]})"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["thought_process"], "reasoning");
        // solution_content should be parsed as JSON object
        assert!(args["solution_content"].is_object() || args["solution_content"].is_string());
    }

    // ---------------------------------------------------------------
    // heuristic_json_tool_calls — wrapped tool call format
    // ---------------------------------------------------------------

    #[test]
    fn test_heuristic_json_tool_calls_wrapped_format() {
        let input = r#"{"tool": "submit_proposal", "arguments": {"thought_process": "thinking", "solution_content": "42"}}"#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_proposal");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["thought_process"], "thinking");
    }

    #[test]
    fn test_heuristic_json_tool_calls_submit_proposal_direct() {
        let input = r#"{"thought_process": "analysis", "solution_content": "result"}"#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_proposal");
    }

    #[test]
    fn heuristic_proposal_echoing_agent_id_is_not_misclassified_as_read() {
        // Some models echo their own agent_id/round inside the proposal object. It
        // carries the proposal payload, so it must classify as submit_proposal — not
        // read_proposal (which would discard the payload).
        let input = r#"{"agent_id": "Xue", "round": 2, "thought_process": "reasoning", "solution_content": "answer"}"#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].function.name, "submit_proposal",
            "proposal payload wins over the read_proposal agent_id+round signature"
        );
        // A genuine read (no payload) still classifies as read_proposal.
        let read = r#"{"agent_id": "Xue", "round": 2}"#;
        let read_calls = heuristic_json_tool_calls(read);
        assert_eq!(read_calls.len(), 1);
        assert_eq!(read_calls[0].function.name, "read_proposal");
    }

    // ---------------------------------------------------------------
    // read_own_proposal (no args needed)
    // ---------------------------------------------------------------

    #[test]
    fn test_read_own_proposal() {
        let input = r#"read_own_proposal()"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_own_proposal");
    }

    // ---------------------------------------------------------------
    // extract_code_block — edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_code_block_empty_input() {
        let result = extract_code_block("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_extract_code_block_empty_code_block() {
        let input = "```python\n```";
        let result = extract_code_block(input);
        assert_eq!(result, "");
    }

    #[test]
    fn test_extract_code_block_rust_language() {
        let input = "```rust\nfn main() {\n    println!(\"hello\");\n}\n```";
        let result = extract_code_block(input);
        assert!(result.contains("fn main()"));
        assert!(result.contains("println!"));
    }

    #[test]
    fn test_extract_code_block_whitespace_only() {
        let input = "```\n   \n\t\n```";
        let result = extract_code_block(input);
        assert_eq!(result, "");
    }

    // ---------------------------------------------------------------
    // extract_proposal_from_markdown — edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_proposal_from_markdown_generic_code_block() {
        let input = "Analysis:\nI thought about it.\n\n```\nThe answer is 42\n```\n";
        let result = extract_proposal_from_markdown(input).expect("Should extract");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed["solution_content"].as_str().unwrap(),
            "The answer is 42"
        );
        let thought = parsed["thought_process"].as_str().unwrap();
        assert!(thought.contains("Analysis:"));
    }

    #[test]
    fn test_extract_proposal_from_markdown_returns_none_without_backticks() {
        let input = "Regular text without any code fences.";
        assert!(extract_proposal_from_markdown(input).is_none());
    }

    // ---------------------------------------------------------------
    // extract_evaluations_from_markdown — edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_evaluations_empty_agent_id() {
        let markdown = r#"
| agent_id | endorsement_weight | justification | is_final_solution |
|----------|-------------------|---------------|-------------------|
|          | 50                | Some work     | false             |
"#;
        let result = extract_evaluations_from_markdown(markdown);
        assert!(
            result.is_none(),
            "Empty agent_id rows should be skipped, resulting in None"
        );
    }

    #[test]
    fn test_extract_evaluations_is_final_solution_variants() {
        let markdown = r#"
| agent_id | endorsement_weight | justification | is_final_solution |
|----------|-------------------|---------------|-------------------|
| A        | 90                | Excellent     | true              |
| B        | 85                | Great         | yes               |
| C        | 80                | Good          | 1                 |
| D        | 75                | OK            | TRUE              |
| E        | 70                | Fine          | maybe             |
"#;
        let result = extract_evaluations_from_markdown(markdown).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let evals = parsed["evaluations"].as_array().unwrap();
        assert_eq!(evals.len(), 5);
        assert_eq!(evals[0]["is_final_solution"], true); // "true"
        assert_eq!(evals[1]["is_final_solution"], true); // "yes"
        assert_eq!(evals[2]["is_final_solution"], true); // "1"
        // "TRUE" lowered to "true" matches
        assert_eq!(evals[3]["is_final_solution"], true);
        // "maybe" doesn't match true/yes/1
        assert_eq!(evals[4]["is_final_solution"], false);
    }

    // ---------------------------------------------------------------
    // heuristic_json_tool_calls — additional patterns
    // ---------------------------------------------------------------

    #[test]
    fn test_heuristic_json_tool_calls_update_scratchpad() {
        let input = r#"{"content": "My notes", "mode": "append"}"#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "update_scratchpad");
    }

    #[test]
    fn test_heuristic_json_tool_calls_batch_eval() {
        let input = r#"{"evaluations": [{"agent_id": "A", "endorsement_weight": 80}]}"#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_batch_evaluation");
    }

    #[test]
    fn test_heuristic_json_tool_calls_read_proposal() {
        let input = r#"{"agent_id": "Xue", "round": 2}"#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_proposal");
    }

    #[test]
    fn test_heuristic_json_tool_calls_empty_object() {
        let input = "{}";
        let calls = heuristic_json_tool_calls(input);
        assert!(
            calls.is_empty(),
            "Empty JSON object should not match any tool"
        );
    }

    #[test]
    fn test_heuristic_json_tool_calls_unrecognized_keys() {
        let input = r#"{"some_random_key": "value", "another": 42}"#;
        let calls = heuristic_json_tool_calls(input);
        assert!(
            calls.is_empty(),
            "Unrecognized keys should not produce tool calls"
        );
    }

    // ---------------------------------------------------------------
    // extract_python_tool_calls — empty and edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_python_tool_calls_empty_input() {
        let calls = extract_python_tool_calls("");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_extract_python_tool_calls_no_tools() {
        let calls = extract_python_tool_calls("Just some regular text with no function calls.");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_extract_python_tool_calls_multiple_tools() {
        let input = r#"
read_proposal(agent_id="Xue", round=1)
update_scratchpad(content="Notes", mode="append")
submit_proposal(thought_process="Done thinking", solution_content="42")
"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].function.name, "read_proposal");
        assert_eq!(calls[1].function.name, "update_scratchpad");
        assert_eq!(calls[2].function.name, "submit_proposal");
    }

    // ---------------------------------------------------------------
    // extract_code_block — no closing fence
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_code_block_no_closing_fence() {
        let input = "```json\n{\"key\": \"value\"}";
        let result = extract_code_block(input);
        // Without closing fence, regex doesn't match — returns trimmed input as-is
        assert_eq!(
            result,
            input.trim(),
            "Unclosed fence should return the full trimmed input"
        );
    }

    #[test]
    fn test_extract_code_block_no_opening_fence() {
        let input = "Just some text without any code blocks";
        let result = extract_code_block(input);
        assert_eq!(result, input.trim());
    }

    #[test]
    fn test_extract_code_block_multiple_blocks() {
        let input = "```\nfirst block\n```\nSome text\n```\nsecond block\n```";
        let result = extract_code_block(input);
        // Should extract the first code block
        assert!(result.contains("first block"));
    }

    // ---------------------------------------------------------------
    // extract_proposal_from_markdown — with json fence
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_proposal_from_markdown_json_code_block() {
        let input = "Here is my thought.\n\n```json\n{\"solution_content\": \"the answer\", \"thought_process\": \"thinking\"}\n```";
        let result = extract_proposal_from_markdown(input);
        // Has valid code block — should extract a proposal
        let result = result.expect("Should extract a proposal from JSON code block");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed["solution_content"].as_str().unwrap(),
            "{\"solution_content\": \"the answer\", \"thought_process\": \"thinking\"}"
        );
        assert!(
            parsed["thought_process"]
                .as_str()
                .unwrap()
                .contains("Here is my thought.")
        );
    }

    // ---------------------------------------------------------------
    // extract_evaluations_from_markdown — no table
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_evaluations_no_table() {
        let input = "Agent A did well with a score of 90. Agent B scored 85.";
        let result = extract_evaluations_from_markdown(input);
        assert!(
            result.is_none(),
            "Should return None when there's no markdown table"
        );
    }

    #[test]
    fn test_extract_evaluations_single_row() {
        let markdown = r#"
| agent_id | endorsement_weight | justification | is_final_solution |
|----------|-------------------|---------------|-------------------|
| Agent1   | 92                | Very thorough | false             |
"#;
        let result = extract_evaluations_from_markdown(markdown).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let evals = parsed["evaluations"].as_array().unwrap();
        assert_eq!(evals.len(), 1);
        assert_eq!(evals[0]["agent_id"], "Agent1");
        assert_eq!(evals[0]["endorsement_weight"], 92.0);
        assert_eq!(evals[0]["justification"], "Very thorough");
    }

    // ---------------------------------------------------------------
    // heuristic_json_tool_calls — proposal detection
    // ---------------------------------------------------------------

    #[test]
    fn test_heuristic_json_tool_calls_submit_proposal() {
        let input =
            r#"{"thought_process": "Let me think...", "solution_content": "The answer is 42"}"#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_proposal");
    }

    #[test]
    fn test_heuristic_json_tool_calls_non_json_input() {
        let input = "This is just regular text, not JSON at all.";
        let calls = heuristic_json_tool_calls(input);
        assert!(calls.is_empty());
    }

    // ---------------------------------------------------------------
    // extract_xml_tool_calls — basic patterns
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_xml_tool_calls_empty() {
        let calls = extract_xml_tool_calls("");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_extract_xml_tool_calls_no_xml() {
        let calls = extract_xml_tool_calls("Just plain text without XML tags");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_extract_xml_tool_calls_submit_proposal() {
        let input = r#"<tool_call>{"name": "submit_proposal", "arguments": {"thought_process": "think", "solution_content": "42"}}</tool_call>"#;
        let calls = extract_xml_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_proposal");
    }

    #[test]
    fn test_extract_xml_tool_calls_update_scratchpad() {
        let input = r#"<tool_call>{"name": "update_scratchpad", "arguments": {"content": "My notes", "mode": "append"}}</tool_call>"#;
        let calls = extract_xml_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "update_scratchpad");
    }

    #[test]
    fn test_extract_xml_tool_calls_with_text_between() {
        let input = r#"Let me process this.
<tool_call>{"name": "read_proposal", "arguments": {"agent_id": "A"}}</tool_call>
Some reasoning text in between calls.
<tool_call>{"name": "submit_proposal", "arguments": {"solution_content": "result"}}</tool_call>
Done."#;
        let calls = extract_xml_tool_calls(input);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "read_proposal");
        assert_eq!(calls[1].function.name, "submit_proposal");
    }

    #[test]
    fn test_extract_xml_tool_calls_string_arguments() {
        // Arguments can be a string instead of an object
        let input =
            r#"<tool_call>{"name": "my_tool", "arguments": "{\"key\": \"val\"}"}</tool_call>"#;
        let calls = extract_xml_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "my_tool");
        assert!(calls[0].function.arguments.contains("key"));
    }

    #[test]
    fn test_extract_xml_tool_calls_no_arguments_field() {
        // No "arguments" field => should default to null
        let input = r#"<tool_call>{"name": "my_tool"}</tool_call>"#;
        let calls = extract_xml_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "my_tool");
    }

    // ---------------------------------------------------------------
    // heuristic_json_tool_calls — deep unwrap paths (string values)
    // ---------------------------------------------------------------

    #[test]
    fn test_heuristic_json_tool_calls_deep_unwrap_string_value() {
        // Model wraps a recognizable tool call JSON inside a string value of an outer object
        let inner = r#"{"agent_id": "Xue", "round": 1}"#;
        let input = format!(r#"{{"wrapper_key": "{}"}}"#, inner.replace('"', "\\\""));
        let calls = heuristic_json_tool_calls(&input);
        assert_eq!(calls.len(), 1, "Should deep-unwrap JSON from string values");
        assert_eq!(calls[0].function.name, "read_proposal");
    }

    #[test]
    fn test_heuristic_json_tool_calls_depth_limit() {
        // Build deeply nested JSON that exceeds recursion depth limit of 50
        // This tests the `depth > 50` early return
        let mut json = r#"{"agent_id": "X", "round": 1}"#.to_string();
        for _ in 0..60 {
            json = format!(r#"{{"nested": "{}"}}"#, json.replace('"', "\\\""));
        }
        let calls = heuristic_json_tool_calls(&json);
        // Should not panic or stack overflow; may or may not find the deeply buried call
        // The depth limit prevents infinite recursion
        assert!(calls.len() <= 1);
    }

    #[test]
    fn test_heuristic_json_tool_calls_wrapped_with_args_key() {
        // "args" key variant (not "arguments")
        let input = r#"{"tool": "read_proposal", "args": {"agent_id": "Xue", "round": 1}}"#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_proposal");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["agent_id"], "Xue");
    }

    #[test]
    fn test_heuristic_json_tool_calls_wrapped_with_string_args() {
        // When args value is a string instead of object
        let input = r#"{"tool": "submit_proposal", "args": "{\"thought_process\": \"t\", \"solution_content\": \"s\"}"}"#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_proposal");
        // The args should be the string value directly
        assert!(calls[0].function.arguments.contains("thought_process"));
    }

    #[test]
    fn test_heuristic_json_tool_calls_update_scratchpad_overwrite() {
        // mode: "overwrite" should also be recognized
        let input = r#"{"content": "New plan", "mode": "overwrite"}"#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "update_scratchpad");
    }

    #[test]
    fn test_heuristic_json_tool_calls_update_scratchpad_invalid_mode() {
        // mode: "replace" should NOT match update_scratchpad
        let input = r#"{"content": "data", "mode": "replace"}"#;
        let calls = heuristic_json_tool_calls(input);
        assert!(
            calls.is_empty(),
            "Invalid mode should not match update_scratchpad"
        );
    }

    #[test]
    fn test_heuristic_json_tool_calls_unbalanced_braces() {
        // Unbalanced closing brace before opening — stack goes negative, should reset
        let input = r#"} some text {"agent_id": "A", "round": 1} more"#;
        let calls = heuristic_json_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_proposal");
    }

    // ---------------------------------------------------------------
    // parse_json_or_python_literal — edge cases
    // ---------------------------------------------------------------

    #[test]
    #[allow(clippy::approx_constant)] // 3.14 is test data, not an attempt at PI
    fn test_parse_json_or_python_literal_python_float() {
        let input = "3.14";
        let result = parse_json_or_python_literal(input);
        assert!(result.is_some());
        let val = result.unwrap();
        assert!((val.as_f64().unwrap() - 3.14).abs() < 0.001);
    }

    #[test]
    fn test_parse_json_or_python_literal_python_none() {
        let input = "None";
        let result = parse_json_or_python_literal(input);
        assert!(result.is_some());
        assert!(result.unwrap().is_null());
    }

    #[test]
    fn test_parse_json_or_python_literal_nested_python() {
        let input = "{'outer': {'inner': [1, True, None, 'text']}}";
        let result = parse_json_or_python_literal(input);
        assert!(result.is_some());
        let val = result.unwrap();
        let inner = &val["outer"]["inner"];
        assert!(inner.is_array());
        let arr = inner.as_array().unwrap();
        assert_eq!(arr[0], 1);
        assert_eq!(arr[1], true);
        assert!(arr[2].is_null());
        assert_eq!(arr[3], "text");
    }

    #[test]
    fn test_parse_json_or_python_literal_empty_dict() {
        let input = "{}";
        let result = parse_json_or_python_literal(input);
        assert!(result.is_some());
        assert!(result.unwrap().is_object());
    }

    #[test]
    fn test_parse_json_or_python_literal_empty_list() {
        let input = "[]";
        let result = parse_json_or_python_literal(input);
        assert!(result.is_some());
        assert!(result.unwrap().is_array());
    }

    // ---------------------------------------------------------------
    // extract_python_tool_calls — backtick label format edge cases
    // ---------------------------------------------------------------

    #[test]
    fn test_backtick_label_update_scratchpad_non_json() {
        // update_scratchpad with non-JSON content in backtick-label format
        // should use the whole content block as the "content" field
        let input = r#"
`update_scratchpad`:
```
This is my analysis and notes in plain text format.
It spans multiple lines.
```
"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "update_scratchpad");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert!(
            args["content"]
                .as_str()
                .unwrap()
                .contains("analysis and notes")
        );
        assert_eq!(args["mode"], "append");
    }

    // ---------------------------------------------------------------
    // extract_python_tool_calls — command style with booleans/floats
    // ---------------------------------------------------------------

    #[test]
    #[allow(clippy::approx_constant)] // 3.14 is test data, not an attempt at PI
    fn test_command_style_with_boolean_and_float_values() {
        let input = r#"
submit_proposal
thought_process: "Deep analysis"
solution_content: 3.14
is_final: true
debug: false
"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_proposal");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["thought_process"], "Deep analysis");
        // 3.14 contains '.', should be parsed as float
        assert!((args["solution_content"].as_f64().unwrap() - 3.14).abs() < 0.001);
        assert_eq!(args["is_final"], true);
        assert_eq!(args["debug"], false);
    }

    #[test]
    fn test_command_style_with_integer_value() {
        let input = r#"
submit_proposal
thought_process: "thinking"
solution_content: 42
"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        // Integer without decimal point
        assert_eq!(args["solution_content"], 42);
    }

    // ---------------------------------------------------------------
    // extract_python_tool_calls — heuristic JSON fallback
    // ---------------------------------------------------------------

    #[test]
    fn test_heuristic_json_fallback_for_raw_scratchpad() {
        // No function calls or backtick labels, but valid JSON matching update_scratchpad
        let input = r#"
I'm going to update my scratchpad now.

{"content": "Analysis complete", "mode": "append"}
"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "update_scratchpad");
    }

    #[test]
    fn test_heuristic_json_fallback_for_raw_proposal() {
        // Raw JSON matching submit_proposal schema
        let input =
            r#"{"solution_content": "The answer", "thought_process": "I analyzed carefully"}"#;
        let calls = extract_python_tool_calls(input);
        // This should be caught by the heuristic JSON extraction at step 5
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_proposal");
    }

    // ---------------------------------------------------------------
    // extract_python_tool_calls — read_proposal with JSON args
    // ---------------------------------------------------------------

    #[test]
    fn test_read_proposal_json_args() {
        let input = r#"read_proposal({"round": 3, "agent_id": "Bob"})"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["round"], 3);
        assert_eq!(args["agent_id"], "Bob");
    }

    // ---------------------------------------------------------------
    // extract_evaluations_from_markdown — lines without pipes
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_evaluations_mixed_content() {
        // Mix of table rows and non-table text — only table rows should be parsed
        let markdown = r#"
Here are my evaluations:

| agent_id | endorsement_weight | justification | is_final_solution |
|----------|-------------------|---------------|-------------------|
| Alice    | 88                | Brilliant     | true              |

The above evaluation is my final assessment.
"#;
        let result = extract_evaluations_from_markdown(markdown).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let evals = parsed["evaluations"].as_array().unwrap();
        assert_eq!(evals.len(), 1);
        assert_eq!(evals[0]["agent_id"], "Alice");
    }

    // ---------------------------------------------------------------
    // extract_proposal_from_markdown — code block equals entire content
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_proposal_from_markdown_code_block_is_entire_content() {
        // When the content is entirely within a code block, extract_code_block
        // strips fences, so extracted != content.trim() → proceeds to extract.
        let input = "```\nThe entire content is a code block\n```";
        let result = extract_proposal_from_markdown(input);
        let result = result.expect("Should extract proposal when whole content is a code block");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed["solution_content"].as_str().unwrap(),
            "The entire content is a code block"
        );
    }

    #[test]
    fn test_extract_proposal_from_markdown_code_block_content_matches_trim() {
        // When the ONLY content is inside a code block, extract_code_block returns
        // the extracted content. If that content == content.trim(), the function
        // returns None to avoid false positives. This triggers line 63.
        // Build input where extract_code_block returns exactly content.trim()
        // This is hard to trigger because extract_code_block strips the fences.
        // The easiest case: content IS a code block whose body is empty.
        let input = "```\n```";
        let result = extract_proposal_from_markdown(input);
        // extract_code_block returns "" which is empty → line 57 triggers (is_empty)
        assert!(result.is_none());
    }

    // ---------------------------------------------------------------
    // command-style: JSON-like string value in quotes triggers parsing
    // ---------------------------------------------------------------

    #[test]
    fn test_command_style_with_quoted_json_value() {
        // A command-style call where the value is a JSON object inside quotes
        // This triggers line 425 / 431-436 path
        let input = r#"
submit_proposal
thought_process: "reasoning here"
solution_content: '{"nested": [1, 2, 3]}'
"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["thought_process"], "reasoning here");
        // The value inside quotes that looks like JSON should be parsed
        // It may be a string or parsed JSON depending on the path
        assert!(args.get("solution_content").is_some());
    }

    // ---------------------------------------------------------------
    // parse_json_or_python_literal — Python AST error paths
    // ---------------------------------------------------------------

    #[test]
    fn test_parse_json_or_python_literal_python_ast_conversion_error() {
        // A Python expression that the AST parser accepts but python_ast_to_json
        // cannot convert (unsupported AST node) — covers line 689
        // Tuples are not supported by our converter
        let input = "(1, 2, 3)";
        let result = parse_json_or_python_literal(input);
        // Tuples are unsupported AST nodes, should return None
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_json_or_python_literal_non_expression_parse() {
        // A Python statement (not an expression) — covers lines 637-638
        // Statements like `x = 5` parse as Module, not Expression
        let input = "x = 5";
        let result = parse_json_or_python_literal(input);
        // Should fail because it's not a valid expression/literal
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_json_or_python_literal_syntax_error_not_structure() {
        // Invalid syntax that doesn't look like a structure
        // (no leading { or [), covers the else branch at line 644+
        let input = "definitely not valid";
        let result = parse_json_or_python_literal(input);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_json_or_python_literal_syntax_error_looking_like_structure() {
        // Input that starts with { but has invalid Python syntax
        // Covers lines 642-646 (warn branch)
        let input = "{invalid python syntax!@#$%}";
        let result = parse_json_or_python_literal(input);
        assert!(result.is_none());
    }

    // ---------------------------------------------------------------
    // heuristic_json — deep unwrap from object KEYS
    // ---------------------------------------------------------------

    #[test]
    fn test_heuristic_json_tool_calls_deep_unwrap_key() {
        // Model puts a JSON string as a key in the object (weird but happens)
        // This tests lines 830-836 (key unwrap path)
        // The key itself is a JSON string containing a recognizable tool call
        let inner_json = r#"{"agent_id": "Xue", "round": 1}"#;
        // Build an outer object where the KEY is a JSON string
        let input = format!(r#"{{"{}" : "value"}}"#, inner_json.replace('"', "\\\""));
        let calls = heuristic_json_tool_calls(&input);
        // Should find the tool call inside the key via deep unwrap
        // The key starts with `{` so it tries to parse it
        assert!(
            !calls.is_empty(),
            "Should find tool call from JSON key unwrap, got {} calls for input: {}",
            calls.len(),
            input
        );
    }

    // ---------------------------------------------------------------
    // python_ast_to_json — non-string dict key
    // ---------------------------------------------------------------

    #[test]
    fn test_parse_json_or_python_literal_dict_non_string_key() {
        // Dict with integer key — covers line 683 (non-string key error)
        let input = "{1: 'value'}";
        let result = parse_json_or_python_literal(input);
        // Integer keys in dicts are not valid JSON, should return None or handle
        assert!(result.is_none());
    }

    // ---------------------------------------------------------------
    // python_ast_to_json — bare identifier dict keys (Gemma family)
    // ---------------------------------------------------------------

    #[test]
    fn test_parse_python_bare_identifier_keys() {
        // Gemma 4 outputs `{agent_id: "value"}` instead of `{"agent_id": "value"}`.
        // The Python parser reads `agent_id` as `Expr::Name`. We convert
        // it to a JSON string key.
        let input = r#"[{agent_id: "ARCHIT", endorsement_weight: 85, justification: "Good analysis", is_final_solution: false}]"#;
        let result = parse_json_or_python_literal(input);
        assert!(result.is_some(), "Should parse Python dict with bare keys");
        let arr = result.unwrap();
        let evals = arr.as_array().expect("Should be array");
        assert_eq!(evals.len(), 1);
        assert_eq!(evals[0]["agent_id"], "ARCHIT");
        assert_eq!(evals[0]["endorsement_weight"], 85);
        assert_eq!(evals[0]["justification"], "Good analysis");
        assert_eq!(evals[0]["is_final_solution"], false);
    }

    #[test]
    fn test_parse_python_bare_keys_nested() {
        // Deeper nesting: bare-key dict inside a list inside a dict
        let input = r#"{evaluations: [{agent_id: "A", endorsement_weight: 90, justification: "ok", is_final_solution: true}]}"#;
        let result = parse_json_or_python_literal(input);
        assert!(result.is_some(), "Should parse nested bare-key dict");
        let obj = result.unwrap();
        let evals = obj["evaluations"]
            .as_array()
            .expect("Should have evaluations array");
        assert_eq!(evals.len(), 1);
        assert_eq!(evals[0]["agent_id"], "A");
    }

    // ---------------------------------------------------------------
    // command style — string value that looks like structure after stripping
    // ---------------------------------------------------------------

    #[test]
    fn test_command_style_json_in_bare_braces() {
        // A command-style tool call where the value is a bare JSON object (no quotes)
        let input = r#"
submit_proposal
thought_process: "thinking"
solution_content: {"answer": 42, "steps": [1, 2, 3]}
"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["thought_process"], "thinking");
        // The JSON value should be parsed as an object
        let solution = &args["solution_content"];
        assert!(solution.is_object() || solution.is_string());
    }

    // ---------------------------------------------------------------
    // extract_python_tool_calls — heuristic JSON fallback for batch eval
    // ---------------------------------------------------------------

    #[test]
    fn test_heuristic_json_fallback_for_raw_batch_evaluation() {
        // Raw JSON matching submit_batch_evaluation schema (has "evaluations" key).
        // No function calls, backtick labels, or command-style — falls through to step 5.
        let input = r#"{"evaluations": [{"agent_id": "Xue", "endorsement_weight": 90, "justification": "Good", "is_final_solution": false}]}"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_batch_evaluation");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert!(args["evaluations"].is_array());
    }

    #[test]
    fn test_heuristic_json_fallback_batch_eval_with_stringified_evaluations() {
        // Model outputs {"evaluations": "[{'agent_id': 'Xue', ...}]"} — evaluations
        // is a STRING containing Python-style array. The heuristic should parse and
        // unwrap it into a proper JSON array.
        let input = r#"{"evaluations": "[{'agent_id': 'Xue', 'endorsement_weight': 85, 'justification': 'Solid', 'is_final_solution': True}]"}"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_batch_evaluation");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        // After repair, evaluations should be an array (not a string)
        assert!(
            args["evaluations"].is_array(),
            "Stringified evaluations should be parsed into array: {:?}",
            args["evaluations"]
        );
        let evals = args["evaluations"].as_array().unwrap();
        assert_eq!(evals[0]["agent_id"], "Xue");
        assert_eq!(evals[0]["endorsement_weight"], 85);
    }

    // ---------------------------------------------------------------
    // extract_evaluations_from_markdown — is_final_solution "1"
    // ---------------------------------------------------------------

    #[test]
    fn test_extract_evaluations_is_final_solution_numeric_one() {
        // The "1" variant for is_final_solution
        let markdown = r#"
| agent_id | endorsement_weight | justification | is_final_solution |
|----------|-------------------|---------------|-------------------|
| Alice    | 92                | Perfect       | 1                 |
| Bob      | 50                | Mediocre      | 0                 |
"#;
        let result = extract_evaluations_from_markdown(markdown).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let evals = parsed["evaluations"].as_array().unwrap();
        assert_eq!(evals.len(), 2);
        assert_eq!(
            evals[0]["is_final_solution"], true,
            "\"1\" should map to true"
        );
        assert_eq!(
            evals[1]["is_final_solution"], false,
            "\"0\" should map to false"
        );
    }

    // ---------------------------------------------------------------
    // parse_json_or_python_literal — Python dict with float values
    // ---------------------------------------------------------------

    #[test]
    #[allow(clippy::approx_constant)] // 3.14 is test data, not an attempt at PI
    fn test_parse_json_or_python_literal_python_dict_with_float() {
        // Python dict with float values — exercises the ast::Constant::Float
        // branch in python_ast_to_json (lines 660-662)
        let input = "{'score': 3.14, 'weight': 0.5, 'flag': True}";
        let result = parse_json_or_python_literal(input);
        assert!(result.is_some(), "Should parse Python dict with floats");
        let val = result.unwrap();
        assert!((val["score"].as_f64().unwrap() - 3.14).abs() < 0.001);
        assert!((val["weight"].as_f64().unwrap() - 0.5).abs() < 0.001);
        assert_eq!(val["flag"], true);
    }

    #[test]
    fn parse_python_literal_deep_nesting_hits_depth_cap_gracefully() {
        // `True` is Python, not JSON, so this routes to the python-AST path.
        // Nesting past MAX_PY_AST_DEPTH must stop the recursion and return None
        // — never overflow the stack on adversarial `[[[[…]]]]` input.
        let deep = format!("{}True{}", "[".repeat(60), "]".repeat(60));
        assert!(
            parse_json_or_python_literal(&deep).is_none(),
            "nesting past the depth cap must fail gracefully to None"
        );
        // Sanity: the cap doesn't reject ordinary shallow python literals.
        assert!(parse_json_or_python_literal("[True, False, None]").is_some());
    }

    // ---------------------------------------------------------------
    // extract_python_tool_calls — backtick label with JSON content
    // ---------------------------------------------------------------

    #[test]
    fn test_backtick_label_submit_proposal_json() {
        // Backtick-label style where content is a JSON object — exercises
        // the JSON parsing branch inside the backtick label fallback (lines 491-497).
        let input = r#"
`submit_proposal`:
```json
{"solution_content": "42", "thought_process": "The answer is 42"}
```
"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(
            calls.len(),
            1,
            "Should parse backtick-labeled JSON tool call"
        );
        assert_eq!(calls[0].function.name, "submit_proposal");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["solution_content"], "42");
        assert_eq!(args["thought_process"], "The answer is 42");
    }

    // ---------------------------------------------------------------
    // extract_python_tool_calls — submit_proposal kwargs with
    // quoted-JSON-structure args
    // ---------------------------------------------------------------

    #[test]
    fn test_submit_proposal_kwargs_with_quoted_json_structure() {
        // submit_proposal(thought_process="thinking", solution_content='{"answer": 42}')
        // The inner value is a JSON object inside quotes — after stripping quotes,
        // it should be parsed as JSON (lines 431-436).
        let input = r#"submit_proposal(thought_process="Analyzed carefully", solution_content='[1, 2, 3]')"#;
        let calls = extract_python_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "submit_proposal");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["thought_process"], "Analyzed carefully");
        // The solution_content should be parsed as a JSON array
        assert!(
            args["solution_content"].is_array(),
            "Quoted JSON structure should be parsed: {:?}",
            args["solution_content"]
        );
    }
}
