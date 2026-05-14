# llm-repair

[![Crates.io](https://img.shields.io/crates/v/llm-repair.svg)](https://crates.io/crates/llm-repair)
[![Docs.rs](https://docs.rs/llm-repair/badge.svg)](https://docs.rs/llm-repair)
[![License](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue.svg)](#license)

JSON-repair, markdown-extraction, and tool-call recovery for malformed LLM output.

Real-world LLMs — especially smaller and open-weight ones — don't always return clean JSON or perfectly-formatted tool calls. They truncate. They wrap output in markdown fences. They use Python syntax for tool calls. They invent LaTeX escapes. They prose at you when you wanted a function call.

`llm-repair` is a focused set of helpers for getting structured data out of that mess. No dependency on any specific model SDK — it operates on strings and gives you back strings and parsed values.

## Install

```toml
[dependencies]
llm-repair = "0.6"
```

MSRV: Rust 1.85 (uses Edition 2024).

## What it does

### Recover truncated JSON

```rust
use llm_repair::repair_truncated_json;

let raw = r#"{"name":"search","arguments":{"q":"hello"#;  // model stopped mid-string
let repaired = repair_truncated_json(raw);
// → {"name":"search","arguments":{"q":"hello"}}
```

### Extract JSON from markdown / conversational wrappers

```rust
use llm_repair::clean_json_string;

let dirty = "Sure, here is the result:\n```json\n{\"answer\": 42}\n```";
let clean = clean_json_string(dirty, /* allow_array */ false, /* tool_name */ None);
// → {"answer": 42}
```

### Pull tool calls out of free-form text

```rust
use llm_repair::{extract_python_tool_calls, extract_xml_tool_calls};

// Model emitted Python-style tool calls
let py = r#"search(q="hello", limit=10)"#;
let calls = extract_python_tool_calls(py, &["search"]);

// Model emitted XML-tagged tool calls (Nous Hermes, Anthropic, etc.)
let xml = r#"<tool_call>{"name":"search","arguments":{"q":"hello"}}</tool_call>"#;
let calls = extract_xml_tool_calls(xml);
```

### Heuristic tool-call extraction from anything-shaped output

```rust
use llm_repair::heuristic_json_tool_calls;

// When the model just gives you a function-call-looking thing somewhere
// in a wall of text, try to pull it out.
let calls = heuristic_json_tool_calls(text, &["search", "calc"]);
```

### Extract proposal / evaluation blocks from markdown

```rust
use llm_repair::{extract_proposal_from_markdown, extract_evaluations_from_markdown};

// For deliberation-style prompts where the model returns a labeled
// proposal block inside otherwise free-form analysis.
let proposal = extract_proposal_from_markdown(response);
```

### Repair conversational tool responses

```rust
use llm_repair::{repair_conversational_response, repair_tool_calls};

// Model said "let me search for that" instead of just calling the tool.
// Extract the implied tool call.
let fixed = repair_conversational_response(text);

// Repair a malformed ChatCompletionResponseMessage in-place.
repair_tool_calls(&mut message);
```

### Pair orphan tool calls in conversation history

```rust
use llm_repair::{pair_orphan_tool_calls, stub_tool_response};

// Before sending history back to the model, make sure every assistant
// tool_call has a matching tool message. Sticks stubs in for missing
// pairs to keep providers like OpenAI happy.
pair_orphan_tool_calls(&mut messages);
```

## Failure modes covered

- Truncation (model hit max_tokens mid-output)
- Markdown / code-fence wrapping
- Conversational prefixes ("Sure, here is...")
- Invalid escapes — including LaTeX (`\frac`, `\sum`) inside string values
- Python-syntax tool calls (`fn(arg="val")`) instead of JSON
- XML-tagged tool calls (`<tool_call>...</tool_call>`)
- Unbalanced braces/brackets
- Empty arguments objects (`"arguments":""`)
- Orphan tool calls (assistant calls a tool but no tool message follows)
- Non-string `arguments` fields when the schema expects an object

## Non-goals

- Not a JSON parser. Use [`serde_json`](https://crates.io/crates/serde_json) for that. `llm-repair` runs *before* the parser, to make sure the parser succeeds.
- Not a prompt library. It deals only with cleanup of what the model already returned.
- Not tied to any specific LLM SDK. Inputs are strings or `async-openai` `ChatCompletionResponseMessage` (for the in-place repair functions). No reqwest, no provider-specific calls.

## License

Dual-licensed under either:

- Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/peeramid-labs/quorum-rs/blob/HEAD/LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](https://github.com/peeramid-labs/quorum-rs/blob/HEAD/LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
