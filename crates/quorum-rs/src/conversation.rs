//! Canonical conversation → task-string flattening.
//!
//! A deliberation consumes a single task *string*, not a message array (agents
//! are N proposers/evaluators, provider-agnostic — see
//! `docs/explanation/policy-as-model-and-sessions.md`). Both the server's
//! OpenAI-compat layer (which starts from `messages[]`) and the interactive
//! client (which starts from stored thread messages) must render a multi-turn
//! conversation into that one string **identically**, or a resumed thread reads
//! differently depending on which side assembled it. This module is the single
//! source of truth for that rendering so the two can never drift.

/// Flatten an ordered, role-tagged conversation into the single `user_query`
/// string the deliberation API takes.
///
/// Rules (kept stable — both client and server depend on them):
/// - Empty / whitespace-only messages are dropped.
/// - A lone remaining `user` message is returned bare, with no prefix
///   (backward-compatible with prompts that don't expect a role label).
/// - Otherwise every message is rendered `[role] content`, joined by blank
///   lines, so a model can tell a user's question from a prior answer.
///
/// Callers filter roles they don't want flattened (e.g. the compat layer drops
/// `system` messages first and folds them into separate instructions).
pub fn flatten_conversation<'a, I>(messages: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let non_empty: Vec<(&str, &str)> = messages
        .into_iter()
        .filter(|(_, content)| !content.trim().is_empty())
        .collect();

    if non_empty.len() == 1 && non_empty[0].0 == "user" {
        return non_empty[0].1.to_string();
    }

    non_empty
        .iter()
        .map(|(role, content)| format!("[{role}] {content}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lone_user_is_bare() {
        assert_eq!(
            flatten_conversation([("user", "what is rust?")]),
            "what is rust?"
        );
    }

    #[test]
    fn lone_non_user_is_prefixed() {
        // A single assistant/tool message must still carry its origin.
        assert_eq!(
            flatten_conversation([("assistant", "hi")]),
            "[assistant] hi"
        );
    }

    #[test]
    fn multi_turn_prefixes_each_role() {
        let out = flatten_conversation([
            ("user", "what is rust?"),
            ("assistant", "a systems language"),
            ("user", "vs go?"),
        ]);
        assert_eq!(
            out,
            "[user] what is rust?\n\n[assistant] a systems language\n\n[user] vs go?"
        );
    }

    #[test]
    fn empty_messages_are_dropped() {
        let out = flatten_conversation([("assistant", "   "), ("user", "real"), ("user", "next")]);
        assert_eq!(out, "[user] real\n\n[user] next");
    }

    #[test]
    fn all_empty_yields_empty_string() {
        assert_eq!(
            flatten_conversation([("user", ""), ("assistant", "  ")]),
            ""
        );
    }
}
