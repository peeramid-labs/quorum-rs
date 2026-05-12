//! Generic user-defined tool that delegates execution to the UserToolHandler.
//!
//! Each `UserToolDefinition` submitted at job creation is instantiated as a
//! `UserCallTool` with the `user_` prefix. These tools participate in the
//! standard tool dispatch pipeline — no special-casing in the react loop.

use crate::agents::{DeliberationPhase, UserToolDefinition, UserToolHandlerTrait};
use crate::tools::Tool;

use async_openai::types::{ChatCompletionTool, ChatCompletionToolType, FunctionObject};
use async_trait::async_trait;
use serde_json::Value;
use std::error::Error;
use std::sync::Arc;

/// A user-defined tool that forwards calls to the external environment via NATS KV.
/// Implements the `Tool` trait so it integrates with the standard react loop dispatch.
#[derive(Clone, Debug)]
pub struct UserCallTool {
    /// The tool name with `user_` prefix (e.g., "user_dm_user").
    prefixed_name: String,
    /// The original tool definition from the user.
    definition: UserToolDefinition,
    /// Handler for NATS KV interaction (publish call, watch for response).
    handler: Arc<dyn UserToolHandlerTrait>,
    /// Current round number (for the PendingToolCall record).
    round: u32,
    /// Current phase (for the PendingToolCall record).
    phase: DeliberationPhase,
}

impl UserCallTool {
    pub fn new(
        definition: UserToolDefinition,
        handler: Arc<dyn UserToolHandlerTrait>,
        round: u32,
        phase: DeliberationPhase,
    ) -> Self {
        let prefixed_name = format!("user_{}", definition.name);
        Self {
            prefixed_name,
            definition,
            handler,
            round,
            phase,
        }
    }
}

#[async_trait]
impl Tool for UserCallTool {
    fn name(&self) -> String {
        self.prefixed_name.clone()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.prefixed_name.clone(),
                description: Some(self.definition.description.clone()),
                parameters: self.definition.parameters.clone(),
                strict: self.definition.strict,
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let args_json = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        let result = self
            .handler
            .handle_call(&self.prefixed_name, &args_json, self.round, self.phase)
            .await;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mock handler that records calls and returns a configurable response.
    #[derive(Debug)]
    struct MockUserToolHandler {
        response: String,
        calls: Mutex<Vec<(String, String, u32, DeliberationPhase)>>,
    }

    impl MockUserToolHandler {
        fn new(response: &str) -> Self {
            Self {
                response: response.to_string(),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn last_call(&self) -> Option<(String, String, u32, DeliberationPhase)> {
            self.calls.lock().unwrap().last().cloned()
        }
    }

    #[async_trait]
    impl UserToolHandlerTrait for MockUserToolHandler {
        async fn handle_call(
            &self,
            tool_name: &str,
            arguments_json: &str,
            round: u32,
            phase: DeliberationPhase,
        ) -> String {
            self.calls.lock().unwrap().push((
                tool_name.to_string(),
                arguments_json.to_string(),
                round,
                phase,
            ));
            self.response.clone()
        }
    }

    fn make_tool(
        name: &str,
        description: &str,
        parameters: Option<serde_json::Value>,
        strict: Option<bool>,
        handler: Arc<dyn UserToolHandlerTrait>,
        round: u32,
        phase: DeliberationPhase,
    ) -> UserCallTool {
        let def = UserToolDefinition {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
            strict,
        };
        UserCallTool::new(def, handler, round, phase)
    }

    // -----------------------------------------------------------------------
    // Constructor (new) tests
    // -----------------------------------------------------------------------

    #[test]
    fn new_prefixes_name_with_user() {
        let handler: Arc<dyn UserToolHandlerTrait> = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "dm_user",
            "Send a DM",
            None,
            None,
            handler,
            1,
            DeliberationPhase::Proposing,
        );
        assert_eq!(tool.prefixed_name, "user_dm_user");
    }

    #[test]
    fn new_stores_definition_fields() {
        let handler: Arc<dyn UserToolHandlerTrait> = Arc::new(MockUserToolHandler::new("ok"));
        let params = Some(serde_json::json!({"type": "object"}));
        let tool = make_tool(
            "read_file",
            "Read a file",
            params.clone(),
            Some(true),
            handler,
            3,
            DeliberationPhase::Evaluating,
        );
        assert_eq!(tool.definition.name, "read_file");
        assert_eq!(tool.definition.description, "Read a file");
        assert_eq!(tool.definition.parameters, params);
        assert_eq!(tool.definition.strict, Some(true));
        assert_eq!(tool.round, 3);
        assert!(matches!(tool.phase, DeliberationPhase::Evaluating));
    }

    #[test]
    fn new_with_empty_name() {
        let handler: Arc<dyn UserToolHandlerTrait> = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "",
            "empty",
            None,
            None,
            handler,
            0,
            DeliberationPhase::Proposing,
        );
        assert_eq!(tool.prefixed_name, "user_");
    }

    // -----------------------------------------------------------------------
    // Tool::name() tests
    // -----------------------------------------------------------------------

    #[test]
    fn name_returns_prefixed_name() {
        let handler: Arc<dyn UserToolHandlerTrait> = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "search",
            "Search docs",
            None,
            None,
            handler,
            1,
            DeliberationPhase::Proposing,
        );
        assert_eq!(tool.name(), "user_search");
    }

    // -----------------------------------------------------------------------
    // Tool::schema() tests
    // -----------------------------------------------------------------------

    #[test]
    fn schema_returns_function_type() {
        let handler: Arc<dyn UserToolHandlerTrait> = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "action",
            "Do something",
            None,
            None,
            handler,
            1,
            DeliberationPhase::Proposing,
        );
        let schema = tool.schema();
        assert!(matches!(schema.r#type, ChatCompletionToolType::Function));
    }

    #[test]
    fn schema_uses_prefixed_name() {
        let handler: Arc<dyn UserToolHandlerTrait> = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "send_email",
            "Send email",
            None,
            None,
            handler,
            1,
            DeliberationPhase::Proposing,
        );
        let schema = tool.schema();
        assert_eq!(schema.function.name, "user_send_email");
    }

    #[test]
    fn schema_includes_description() {
        let handler: Arc<dyn UserToolHandlerTrait> = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "ping",
            "Ping a server",
            None,
            None,
            handler,
            1,
            DeliberationPhase::Proposing,
        );
        let schema = tool.schema();
        assert_eq!(
            schema.function.description,
            Some("Ping a server".to_string())
        );
    }

    #[test]
    fn schema_with_parameters_and_strict() {
        let handler: Arc<dyn UserToolHandlerTrait> = Arc::new(MockUserToolHandler::new("ok"));
        let params = serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path" }
            },
            "required": ["path"]
        });
        let tool = make_tool(
            "read_file",
            "Read file",
            Some(params.clone()),
            Some(true),
            handler,
            1,
            DeliberationPhase::Proposing,
        );
        let schema = tool.schema();
        assert_eq!(schema.function.parameters, Some(params));
        assert_eq!(schema.function.strict, Some(true));
    }

    #[test]
    fn schema_without_parameters_or_strict() {
        let handler: Arc<dyn UserToolHandlerTrait> = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "noop",
            "No-op tool",
            None,
            None,
            handler,
            1,
            DeliberationPhase::Proposing,
        );
        let schema = tool.schema();
        assert!(schema.function.parameters.is_none());
        assert!(schema.function.strict.is_none());
    }

    // -----------------------------------------------------------------------
    // Tool::call() tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn call_delegates_to_handler_and_returns_result() {
        let handler = Arc::new(MockUserToolHandler::new("tool response"));
        let tool = make_tool(
            "greet",
            "Greet user",
            None,
            None,
            handler.clone() as Arc<dyn UserToolHandlerTrait>,
            2,
            DeliberationPhase::Evaluating,
        );

        let args = serde_json::json!({"name": "Alice"});
        let result = tool.call(args).await.unwrap();
        assert_eq!(result, "tool response");
        assert_eq!(handler.call_count(), 1);
    }

    #[tokio::test]
    async fn call_passes_correct_tool_name_to_handler() {
        let handler = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "my_tool",
            "desc",
            None,
            None,
            handler.clone() as Arc<dyn UserToolHandlerTrait>,
            1,
            DeliberationPhase::Proposing,
        );

        tool.call(serde_json::json!({})).await.unwrap();
        let (name, _, _, _) = handler.last_call().unwrap();
        assert_eq!(name, "user_my_tool");
    }

    #[tokio::test]
    async fn call_serializes_args_as_json() {
        let handler = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "t",
            "d",
            None,
            None,
            handler.clone() as Arc<dyn UserToolHandlerTrait>,
            1,
            DeliberationPhase::Proposing,
        );

        let args = serde_json::json!({"key": "value", "num": 42});
        tool.call(args.clone()).await.unwrap();
        let (_, args_json, _, _) = handler.last_call().unwrap();
        // Parse back and verify
        let parsed: serde_json::Value = serde_json::from_str(&args_json).unwrap();
        assert_eq!(parsed["key"], "value");
        assert_eq!(parsed["num"], 42);
    }

    #[tokio::test]
    async fn call_passes_round_and_phase() {
        let handler = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "t",
            "d",
            None,
            None,
            handler.clone() as Arc<dyn UserToolHandlerTrait>,
            5,
            DeliberationPhase::Evaluating,
        );

        tool.call(serde_json::json!({})).await.unwrap();
        let (_, _, round, phase) = handler.last_call().unwrap();
        assert_eq!(round, 5);
        assert!(matches!(phase, DeliberationPhase::Evaluating));
    }

    #[tokio::test]
    async fn call_with_empty_object_args() {
        let handler = Arc::new(MockUserToolHandler::new("empty"));
        let tool = make_tool(
            "t",
            "d",
            None,
            None,
            handler.clone() as Arc<dyn UserToolHandlerTrait>,
            1,
            DeliberationPhase::Proposing,
        );

        let result = tool.call(serde_json::json!({})).await.unwrap();
        assert_eq!(result, "empty");
        let (_, args_json, _, _) = handler.last_call().unwrap();
        assert_eq!(args_json, "{}");
    }

    // -----------------------------------------------------------------------
    // Clone / Debug derive tests
    // -----------------------------------------------------------------------

    #[test]
    fn tool_implements_debug() {
        let handler: Arc<dyn UserToolHandlerTrait> = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "t",
            "d",
            None,
            None,
            handler,
            1,
            DeliberationPhase::Proposing,
        );
        let debug_str = format!("{:?}", tool);
        assert!(debug_str.contains("UserCallTool"));
        assert!(debug_str.contains("user_t"));
    }

    #[test]
    fn tool_is_cloneable() {
        let handler: Arc<dyn UserToolHandlerTrait> = Arc::new(MockUserToolHandler::new("ok"));
        let tool = make_tool(
            "t",
            "d",
            None,
            None,
            handler,
            1,
            DeliberationPhase::Proposing,
        );
        let cloned = tool.clone();
        assert_eq!(cloned.name(), tool.name());
        assert_eq!(cloned.round, tool.round);
    }
}
