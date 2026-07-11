//! Canonical conversation model + rendering.
//!
//! The deliberation's native representation is an ordered, role-tagged
//! [`Message`] array (an extended-OpenAI shape with a Mixture-of-Minds
//! `noosphera` consensus role). Providers can't all consume that array — the
//! OpenAI-compat and exec backends take a single task *string* and don't resume
//! a session — so each provider **renders** the array to what it can send via
//! [`render`]: a resumed claude session gets only the newest turn (it already
//! holds the rest), while a fresh session / stateless backend gets the whole
//! conversation flattened. [`flatten_conversation`] is the shared, stable
//! string primitive both the client and the server's OpenAI-compat layer render
//! through, so a resumed thread can never read differently depending on which
//! side assembled it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A role in a deliberation conversation — the standard user/assistant pair
/// extended with the Mixture-of-Minds consensus role (`noosphera`) and an
/// operator mid-deliberation injection.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The operator's turn.
    User,
    /// The deliberation's consensus reply (the assistant, on the wire).
    Noosphera,
    /// An operator message injected mid-deliberation.
    UserInjection,
}

impl Role {
    /// The wire label used when flattening to the task string. `Noosphera` maps
    /// to `assistant` so the flattened form stays OpenAI-legible.
    pub fn label(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Noosphera => "assistant",
            Role::UserInjection => "user_injection",
        }
    }
}

/// One turn of a deliberation conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, utoipa::ToSchema)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn noosphera(content: impl Into<String>) -> Self {
        Self {
            role: Role::Noosphera,
            content: content.into(),
        }
    }
}

/// Render the conversation to the single task string a provider sends this call.
///
/// - `resumed == true`: only the **newest** turn — the resumed claude session
///   already holds the earlier turns, so re-sending them is pure token waste.
/// - `resumed == false`: the **whole** conversation flattened — a fresh session
///   or a stateless backend (OpenAI-compat / exec) has no prior context.
///
/// Both paths go through [`flatten_conversation`] so the string form is
/// identical regardless of how much of the array is sent.
pub fn render(messages: &[Message], resumed: bool) -> String {
    let slice = if resumed {
        &messages[messages.len().saturating_sub(1)..]
    } else {
        messages
    };
    flatten_conversation(slice.iter().map(|m| (m.role.label(), m.content.as_str())))
}

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

    #[test]
    fn render_fresh_flattens_the_whole_conversation() {
        let msgs = vec![
            Message::user("q1"),
            Message::noosphera("a1"),
            Message::user("q2"),
        ];
        assert_eq!(
            render(&msgs, false),
            "[user] q1\n\n[assistant] a1\n\n[user] q2"
        );
    }

    #[test]
    fn render_resumed_sends_only_the_newest_turn_bounded_by_length() {
        let mut msgs: Vec<Message> = (0..50)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("turn {i}"))
                } else {
                    Message::noosphera(format!("reply {i}"))
                }
            })
            .collect();
        msgs.push(Message::user("the newest question"));
        // Resumed → only the last turn (a lone user turn renders bare), so the
        // output is independent of how long the thread grew.
        assert_eq!(render(&msgs, true), "the newest question");
        assert!(!render(&msgs, true).contains("turn 0"));
    }
}
