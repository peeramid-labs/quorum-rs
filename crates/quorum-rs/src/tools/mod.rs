use async_openai::types::ChatCompletionTool;
use async_trait::async_trait;
use dyn_clone::DynClone;
use serde_json::Value;
use std::error::Error;

pub mod pdf_query;
pub mod sandbox;
pub mod scoped_grep;
pub mod scoped_read;

pub use pdf_query::ScopedPdfQueryTool;
pub use scoped_grep::ScopedGrepTool;
pub use scoped_read::ScopedReadFileTool;

// Export ToolDefinition alias so quorum_rs::tools::ToolDefinition works
pub type ToolDefinition = ChatCompletionTool;

/// A capability that can be described to and executed by an LLM agent.
///
/// This trait is the foundation for dynamic function calling. Each tool
/// provides its schema to the LLM and an execution method (`call`) that
/// the orchestrator can invoke with arguments provided by the LLM.
///
/// It also requires `DynClone` to allow `Box<dyn Tool>` objects to be cloned,
/// which is necessary for creating separate toolsets for the Proposer and
/// Evaluator agents from a single source of truth (e.g., the sandbox).
#[async_trait]
pub trait Tool: Send + Sync + DynClone + std::fmt::Debug {
    /// Returns the unique name of the tool (e.g., "execute_command").
    /// This name is used by the LLM to identify which tool to call.
    fn name(&self) -> String;

    /// Returns the JSON schema for the tool, compliant with OpenAI's function calling API.
    /// This schema describes the tool's purpose, parameters, and their types, allowing
    /// the LLM to understand how and when to use it.
    fn schema(&self) -> ChatCompletionTool;

    /// Executes the tool with the given arguments.
    ///
    /// # Arguments
    /// * `args` - A `serde_json::Value` containing the arguments for the function call,
    ///            as provided by the LLM. The implementer of this method is responsible
    ///            for deserializing these arguments into a strongly-typed struct.
    ///
    /// # Returns
    /// A `Result` containing a stringified JSON output from the tool, or an error if
    /// the execution fails. The string output will be sent back to the LLM as the
    /// "observation".
    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>>;
}

// Implement `Clone` for `Box<dyn Tool>` using the `dyn_clone` crate's macro.
dyn_clone::clone_trait_object!(Tool);
