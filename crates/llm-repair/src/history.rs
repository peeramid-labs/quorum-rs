//! Conversation-history hygiene.
//!
//! OpenAI's chat completion spec requires every `{role: "assistant",
//! tool_calls: [...]}` message to be immediately followed by one
//! `{role: "tool", tool_call_id: <id>}` message per tool_call id.
//!
//! Strict providers (observed: Cerebras) reject with HTTP 422
//! `wrong_api_format` when this invariant is broken. Lenient providers
//! (Parasail, Fireworks, OpenAI itself at times) accept the imperfect
//! history and silently continue, so the bug only surfaces intermittently
//! depending on OpenRouter's routing decision.
//!
//! NSED's retry loop can introduce the violation in three ways:
//!
//! 1. **Terminal-tool parse failure** — when the terminal tool's JSON
//!    arguments fail schema validation, the retry path reuses
//!    `agent_response.history` which ends with the assistant turn that
//!    *made* the bad tool_call. Terminal tools never get a `role: "tool"`
//!    follow-up (the loop exits on them), so the history now contains
//!    an orphan tool_call on the next LLM send.
//! 2. **Streaming truncation** — `finish_reason=Length` cuts the
//!    assistant turn mid-tool_call; any partial calls still end up in
//!    history without corresponding responses.
//! 3. **Tool-execution error mid-loop** — a tool throws, the loop
//!    propagates the error before appending the `role: "tool"` response
//!    message, leaving the assistant turn orphaned.
//!
//! [`pair_orphan_tool_calls`] walks the message vec and inserts stub
//! `role: "tool"` responses for any unmatched tool_call_ids. This is
//! preferable to dropping the orphan assistant turn because it preserves
//! the reasoning context the model used when deciding to call the tool —
//! which often contains useful context the retry prompt can build on.

use async_openai::types::{
    ChatCompletionRequestMessage, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent,
};
use std::collections::HashSet;

/// Default stub content inserted when a `tool_calls` id has no
/// matching `role: "tool"` response. The text deliberately hints that
/// the tool did not run so the model doesn't treat the absence as a
/// successful no-op result.
pub const ORPHAN_TOOL_STUB_CONTENT: &str = "Tool response unavailable (upstream interruption or parse failure). \
     Continue from prior state; do not rely on this tool's output.";

/// Walk `messages` and insert stub `role: "tool"` responses for any
/// `tool_calls` id in an assistant turn that lacks a matching
/// follow-up tool message. The stub is inserted immediately after the
/// last contiguous tool message (or immediately after the assistant
/// turn if no tool messages follow it).
///
/// Idempotent: running the function twice on the same `messages`
/// produces the same result as a single run.
///
/// Back-compatible: if all `tool_calls` already have matching
/// responses, `messages` is unchanged.
pub fn pair_orphan_tool_calls(messages: &mut Vec<ChatCompletionRequestMessage>) {
    let mut i = 0;
    while i < messages.len() {
        let ChatCompletionRequestMessage::Assistant(asst) = &messages[i] else {
            i += 1;
            continue;
        };

        // Snapshot the tool_call ids from this assistant turn. Clone
        // out so we can drop the immutable borrow before mutating.
        let Some(tcs) = &asst.tool_calls else {
            i += 1;
            continue;
        };
        if tcs.is_empty() {
            i += 1;
            continue;
        }
        let expected_ids: Vec<String> = tcs.iter().map(|tc| tc.id.clone()).collect();

        // Scan contiguous `Tool` messages following this assistant
        // turn to collect the ids we already have responses for.
        let mut j = i + 1;
        let mut seen: HashSet<String> = HashSet::new();
        while j < messages.len() {
            let ChatCompletionRequestMessage::Tool(tm) = &messages[j] else {
                break;
            };
            seen.insert(tm.tool_call_id.clone());
            j += 1;
        }

        // Insert stubs for missing ids, preserving tool_call order.
        // `j` points at the first non-tool message (or past the end).
        for id in &expected_ids {
            if !seen.contains(id) {
                messages.insert(j, stub_tool_response(id));
                j += 1;
            }
        }

        i = j;
    }
}

/// Build a synthetic `role: "tool"` message that pairs a given
/// tool_call_id. Exposed for callers that want to inject stubs
/// manually with different content (tests, custom retry prompts).
pub fn stub_tool_response(tool_call_id: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Tool(ChatCompletionRequestToolMessage {
        content: ChatCompletionRequestToolMessageContent::Text(
            ORPHAN_TOOL_STUB_CONTENT.to_string(),
        ),
        tool_call_id: tool_call_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::{
        ChatCompletionMessageToolCall, ChatCompletionRequestAssistantMessage,
        ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent, ChatCompletionToolType, FunctionCall,
    };

    fn user(text: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
            content: ChatCompletionRequestUserMessageContent::Text(text.to_string()),
            name: None,
        })
    }

    fn assistant_text(text: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
            content: Some(ChatCompletionRequestAssistantMessageContent::Text(
                text.to_string(),
            )),
            ..Default::default()
        })
    }

    fn assistant_with_tool_calls(ids: &[&str]) -> ChatCompletionRequestMessage {
        let tool_calls: Vec<ChatCompletionMessageToolCall> = ids
            .iter()
            .map(|id| ChatCompletionMessageToolCall {
                id: id.to_string(),
                r#type: ChatCompletionToolType::Function,
                function: FunctionCall {
                    name: "search_deliberation".to_string(),
                    arguments: "{}".to_string(),
                },
            })
            .collect();
        ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
            tool_calls: Some(tool_calls),
            ..Default::default()
        })
    }

    fn tool(id: &str, content: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestMessage::Tool(ChatCompletionRequestToolMessage {
            content: ChatCompletionRequestToolMessageContent::Text(content.to_string()),
            tool_call_id: id.to_string(),
        })
    }

    fn tool_call_id_of(msg: &ChatCompletionRequestMessage) -> Option<&str> {
        if let ChatCompletionRequestMessage::Tool(t) = msg {
            Some(t.tool_call_id.as_str())
        } else {
            None
        }
    }

    #[test]
    fn empty_messages_unchanged() {
        let mut messages: Vec<ChatCompletionRequestMessage> = vec![];
        pair_orphan_tool_calls(&mut messages);
        assert!(messages.is_empty());
    }

    #[test]
    fn assistant_without_tool_calls_unchanged() {
        let mut messages = vec![user("hi"), assistant_text("hello")];
        let before = messages.len();
        pair_orphan_tool_calls(&mut messages);
        assert_eq!(messages.len(), before);
    }

    #[test]
    fn fully_paired_tool_calls_unchanged() {
        let mut messages = vec![
            user("query"),
            assistant_with_tool_calls(&["id_a", "id_b"]),
            tool("id_a", "result_a"),
            tool("id_b", "result_b"),
            assistant_text("done"),
        ];
        let len_before = messages.len();
        pair_orphan_tool_calls(&mut messages);
        assert_eq!(messages.len(), len_before, "no stubs should be inserted");
    }

    #[test]
    fn single_orphan_gets_stub() {
        // Assistant with one tool_call, zero responses — terminal tool
        // parse failure scenario.
        let mut messages = vec![
            user("propose something"),
            assistant_with_tool_calls(&["d0a738497"]),
        ];
        pair_orphan_tool_calls(&mut messages);
        assert_eq!(messages.len(), 3);
        assert_eq!(tool_call_id_of(&messages[2]), Some("d0a738497"));
    }

    #[test]
    fn partial_pairing_fills_missing_ids_only() {
        // Two tool_calls, only one has a response — sanitizer must
        // insert exactly one stub matching the missing id.
        let mut messages = vec![
            user("q"),
            assistant_with_tool_calls(&["id_a", "id_b"]),
            tool("id_a", "result_a"),
        ];
        pair_orphan_tool_calls(&mut messages);
        assert_eq!(messages.len(), 4);
        assert_eq!(tool_call_id_of(&messages[2]), Some("id_a"));
        assert_eq!(tool_call_id_of(&messages[3]), Some("id_b"));
    }

    #[test]
    fn stub_inserted_before_next_non_tool_message() {
        // Orphan assistant tool_call followed by a user message —
        // stub must land BETWEEN the orphan and the user message
        // (immediately after the assistant turn), not at the end.
        let mut messages = vec![
            assistant_with_tool_calls(&["id_x"]),
            user("follow-up from the operator"),
        ];
        pair_orphan_tool_calls(&mut messages);
        assert_eq!(messages.len(), 3);
        assert_eq!(tool_call_id_of(&messages[1]), Some("id_x"));
        // User message pushed to index 2.
        match &messages[2] {
            ChatCompletionRequestMessage::User(_) => (),
            _ => panic!("user message should be at index 2"),
        }
    }

    #[test]
    fn multiple_assistant_groups_independently_validated() {
        // Two separate assistant-turn groups with tool_calls. First is
        // fully paired; second is orphaned. Only the second should
        // get stubs inserted.
        let mut messages = vec![
            assistant_with_tool_calls(&["id_a"]),
            tool("id_a", "res_a"),
            assistant_text("intermediate thought"),
            user("another turn"),
            assistant_with_tool_calls(&["id_b", "id_c"]),
            // no tool responses for id_b/id_c
        ];
        pair_orphan_tool_calls(&mut messages);
        // 2 stubs inserted at the end.
        assert_eq!(messages.len(), 7);
        assert_eq!(tool_call_id_of(&messages[5]), Some("id_b"));
        assert_eq!(tool_call_id_of(&messages[6]), Some("id_c"));
    }

    #[test]
    fn idempotent_second_run_is_noop() {
        let mut messages = vec![
            user("q"),
            assistant_with_tool_calls(&["id_a"]),
            // orphan
        ];
        pair_orphan_tool_calls(&mut messages);
        let after_first = messages.clone();
        pair_orphan_tool_calls(&mut messages);
        assert_eq!(
            messages.len(),
            after_first.len(),
            "second run must not add more stubs"
        );
    }

    #[test]
    fn stub_tool_response_has_expected_shape() {
        let stub = stub_tool_response("call_abc");
        match stub {
            ChatCompletionRequestMessage::Tool(t) => {
                assert_eq!(t.tool_call_id, "call_abc");
                match t.content {
                    ChatCompletionRequestToolMessageContent::Text(s) => {
                        assert!(s.contains("Tool response unavailable"));
                    }
                    _ => panic!("stub content should be text"),
                }
            }
            _ => panic!("stub must be a Tool message"),
        }
    }

    #[test]
    fn empty_tool_calls_vec_ignored() {
        // `tool_calls: Some(vec![])` — legit shape some SDKs emit.
        // Must be treated like no tool_calls at all.
        let mut messages = vec![
            user("q"),
            ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
                tool_calls: Some(vec![]),
                ..Default::default()
            }),
        ];
        let before = messages.len();
        pair_orphan_tool_calls(&mut messages);
        assert_eq!(messages.len(), before);
    }
}
