//! Web search as a local tool that delegates to a search-only model call.
//!
//! A provider-executed search tool is normally declared alongside the agent's
//! own function tools, and the backend runs it. Some backends refuse that
//! combination outright — Vertex answers `Multiple tools are supported only
//! when they are all search tools` with a 400, and since a deliberating agent
//! always sends function tools (`nsed_propose`, the scratchpad, user tools),
//! such a seat fails every task rather than losing only its search.
//!
//! The constraint is on *mixing*, not on searching: a request carrying the
//! search tool and nothing else is accepted by the same endpoints. So the
//! search moves one level down. The agent is given an ordinary function tool;
//! calling it issues a second, separate completion that declares the provider's
//! search tool alone and returns what came back.
//!
//! The cost is one extra round trip per search and a model that answers the
//! query rather than the task. The benefit is that search stops depending on
//! whether a backend tolerates a mixed tool array.

use super::Tool;
use crate::agents::AgentConfig;
use crate::llms::{AiModel, RequestConfig};
use async_openai::types::{
    ChatCompletionRequestUserMessageArgs, ChatCompletionTool, ChatCompletionToolType,
    FunctionObject,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::error::Error;

/// The function name the model sees.
///
/// Namespaced, and deliberately not a name anyone else would reach for. Two
/// parties can otherwise claim it out from under us:
///
/// * a backend, whose own search tool is spelled `web_search` or
///   `$web_search` — a gateway translates a same-named function into the
///   provider tool, recreating the mixed array this type exists to avoid;
/// * a room, whose `user_tools` are named by whoever opened it and arrive per
///   task, too late for [`ProposerEvaluatorAgent::validate_tool_names`] to
///   see. A room that declared `search_web` — the name this constant used to
///   hold, and still the example name in this workspace's own job fixtures —
///   put two functions of that name in one request, and the provider answered
///   `Duplicate function declaration found` on every round.
pub const LOCAL_TOOL_NAME: &str = "nsed_delegated_search";

/// Runs a web search by asking the same model again, with only the provider's
/// search tool declared.
#[derive(Clone, Debug)]
pub struct DelegatedSearchTool {
    model: Box<dyn AiModel>,
    /// The agent config for the nested call: the provider search tool and
    /// nothing this crate would add.
    inner: AgentConfig,
}

impl DelegatedSearchTool {
    /// `provider_tool` is the backend's own name for its search tool, exactly
    /// as [`AgentConfig::provider_executed_tools`] spells it — bare for a
    /// backend that takes the tool as a type, `$`-prefixed for one that wraps
    /// it in a function envelope.
    pub fn new(model: Box<dyn AiModel>, agent: &AgentConfig, provider_tool: &str) -> Self {
        let mut inner = agent.clone();
        inner.provider_executed_tools = vec![provider_tool.to_string()];
        Self { model, inner }
    }
}

#[async_trait]
impl Tool for DelegatedSearchTool {
    /// Deliberately not the provider's own name: a gateway maps a function
    /// tool called `web_search` onto the backend's search tool, which puts the
    /// mix this indirection exists to avoid right back on the wire.
    fn name(&self) -> String {
        LOCAL_TOOL_NAME.to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: LOCAL_TOOL_NAME.to_string(),
                description: Some(
                    "Search the live web and return what was found. Use it for anything \
                     that depends on the present — prices, availability, releases, news, \
                     versions — where recalling from training data would be out of date."
                        .to_string(),
                ),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "What to search for, as a search engine query."
                        }
                    },
                    "required": ["query"]
                })),
                strict: None,
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if query.is_empty() {
            return Ok("web_search: no query given".to_string());
        }

        let message = ChatCompletionRequestUserMessageArgs::default()
            .content(query.to_string())
            .build()?;

        // No `tools` and no `tool_choice`: the provider tool is declared from
        // `inner.provider_executed_tools`, and adding a function tool here is
        // exactly the mix this type exists to avoid.
        let request = RequestConfig {
            messages: vec![message.into()],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };

        let result = self.model.chat_completion(&self.inner, request).await?;
        let answer = result
            .response
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();

        if answer.trim().is_empty() {
            // A search that found nothing is an answer; the caller should not
            // read it as a broken tool and retry.
            return Ok(format!("web_search: no results for {query:?}"));
        }
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llms::{ChatCompletionResult, LlmError, TimingMetadata};
    use async_openai::types::{
        ChatChoice, ChatCompletionResponseMessage, CreateChatCompletionResponse, Role,
    };
    use std::sync::{Arc, Mutex};

    /// Answers with fixed text, keeping the config and request it was given.
    #[derive(Clone, Debug)]
    struct SpyModel {
        seen: SeenCalls,
        answer: String,
    }

    #[async_trait]
    impl AiModel for SpyModel {
        async fn chat_completion(
            &self,
            agent: &AgentConfig,
            request: RequestConfig,
        ) -> Result<ChatCompletionResult, LlmError> {
            self.seen
                .lock()
                .unwrap()
                .push((agent.clone(), request.clone()));
            Ok(ChatCompletionResult {
                response: CreateChatCompletionResponse {
                    id: "s".into(),
                    object: "chat.completion".into(),
                    created: 0,
                    model: "m".into(),
                    system_fingerprint: None,
                    usage: None,
                    service_tier: None,
                    choices: vec![ChatChoice {
                        index: 0,
                        message: ChatCompletionResponseMessage {
                            role: Role::Assistant,
                            content: Some(self.answer.clone()),
                            tool_calls: None,
                            #[allow(deprecated)]
                            function_call: None,
                            refusal: None,
                            audio: None,
                        },
                        finish_reason: None,
                        logprobs: None,
                    }],
                },
                raw_request: String::new(),
                timing: TimingMetadata {
                    ttft_ms: None,
                    generation_ms: None,
                },
                provider_backend: None,
                shrink_info: None,
                provider_usage: Default::default(),
            })
        }
    }

    /// What the spy recorded: the config and request of each nested call.
    type SeenCalls = Arc<Mutex<Vec<(AgentConfig, RequestConfig)>>>;

    fn spy(answer: &str) -> (SpyModel, SeenCalls) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            SpyModel {
                seen: seen.clone(),
                answer: answer.to_string(),
            },
            seen,
        )
    }

    /// The whole point: the nested call declares the provider's search tool and
    /// carries no function tools, because a backend that rejects the mix is why
    /// this type exists.
    #[tokio::test]
    async fn the_nested_call_carries_the_search_tool_and_no_function_tools() {
        let (model, seen) = spy("DDR5 RDIMM 64GB is about $300");
        let agent = AgentConfig {
            provider_executed_tools: vec!["should_be_replaced".into()],
            ..AgentConfig::default()
        };

        let tool = DelegatedSearchTool::new(Box::new(model), &agent, "web_search");
        let out = tool
            .call(json!({"query": "ddr5 rdimm price"}))
            .await
            .unwrap();

        assert_eq!(out, "DDR5 RDIMM 64GB is about $300");
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 1, "one nested call per search");
        let (cfg, req) = &calls[0];
        assert_eq!(cfg.provider_executed_tools, vec!["web_search".to_string()]);
        assert!(
            req.tools.is_none(),
            "a function tool would re-create the mix"
        );
        assert!(req.tool_choice.is_none());
    }

    #[tokio::test]
    async fn an_empty_query_never_reaches_the_provider() {
        let (model, seen) = spy("unused");
        let tool = DelegatedSearchTool::new(Box::new(model), &AgentConfig::default(), "web_search");

        let out = tool.call(json!({"query": "   "})).await.unwrap();

        assert!(out.contains("no query"));
        assert!(
            seen.lock().unwrap().is_empty(),
            "no call for an empty query"
        );
    }

    /// A search that finds nothing is an answer, not a tool failure — returning
    /// an error would send the agent into a retry over a working tool.
    #[tokio::test]
    async fn no_results_reads_as_an_answer_rather_than_an_error() {
        let (model, _) = spy("");
        let tool = DelegatedSearchTool::new(Box::new(model), &AgentConfig::default(), "web_search");

        let out = tool.call(json!({"query": "zzzz"})).await.unwrap();

        assert!(out.contains("no results"), "got {out}");
    }

    #[tokio::test]
    async fn the_tool_is_offered_to_the_model_as_an_ordinary_function() {
        let (model, _) = spy("x");
        let tool = DelegatedSearchTool::new(Box::new(model), &AgentConfig::default(), "web_search");

        assert_eq!(tool.name(), LOCAL_TOOL_NAME);
        assert_ne!(
            tool.name(),
            "web_search",
            "the local name must not be a provider search tool name"
        );
        let schema = tool.schema();
        assert!(matches!(schema.r#type, ChatCompletionToolType::Function));
        assert_eq!(schema.function.name, LOCAL_TOOL_NAME);
    }

    /// The name has to be one nobody else will reach for.
    ///
    /// Two parties can claim it: a backend whose own search tool shares the
    /// spelling, and a room whose `user_tools` are named by whoever opened it
    /// and arrive per task — too late to validate. `search_web`, which this
    /// constant used to hold, is also the example user-tool name in this
    /// workspace's own job fixtures, so a room copying those fixtures put two
    /// functions of one name in a request and every round 400'd.
    #[test]
    fn the_local_tool_name_is_namespaced_and_unclaimed() {
        assert!(
            LOCAL_TOOL_NAME.starts_with("nsed_"),
            "the name must be namespaced so a room's user tool cannot collide with it, got {LOCAL_TOOL_NAME:?}"
        );
        for taken in [
            "web_search",
            "$web_search",
            "search_web",
            "search",
            "browse",
            "google_search",
        ] {
            assert_ne!(
                LOCAL_TOOL_NAME, taken,
                "{taken:?} is a name a backend or a room may already use"
            );
        }
    }
}
