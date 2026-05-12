//! This module implements tools for interacting with a sandboxed environment.
//!
//! These tools provide generic capabilities like file system operations and
//! command execution. They are designed to be instantiated by a `Sandbox`
//! implementation, which will equip them with the necessary context (e.g., a
//! Docker container ID) to perform their actions.

use crate::tools::Tool;
use async_openai::types::{ChatCompletionTool, ChatCompletionToolType, FunctionObject};
use async_trait::async_trait;
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::error::Error;

// -----------------------------------------------------------------------------
// CommandOutput Data Structure
// -----------------------------------------------------------------------------

/// Represents the output of a command executed in the sandbox.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

// -----------------------------------------------------------------------------
// ListFilesTool Implementation
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ListFilesTool; // In a real implementation, this would hold a client/ID for the sandbox.

#[derive(Deserialize, schemars::JsonSchema)]
struct ListFilesArgs {
    /// The path to list contents of (e.g., './' or '/src').
    path: String,
}

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> String {
        "list_files".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some(
                    "Lists files and directories at a given path in the sandbox.".to_string(),
                ),
                parameters: Some(schema_for!(ListFilesArgs).into()),
                strict: None,
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let args: ListFilesArgs = serde_json::from_value(args)?;
        // Placeholder logic: In a real implementation, this would execute `ls` in the container.
        let dummy_files = vec!["README.md", "src/", "Cargo.toml"];
        tracing::debug!(path = %args.path, "Listing files (placeholder)");
        Ok(serde_json::to_string(&dummy_files)?)
    }
}

// -----------------------------------------------------------------------------
// ReadFileTool Implementation
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ReadFileTool;

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadFileArgs {
    /// The path of the file to read.
    path: String,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> String {
        "read_file".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some(
                    "Reads the entire content of a file from the sandbox.".to_string(),
                ),
                parameters: Some(schema_for!(ReadFileArgs).into()),
                strict: None,
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let args: ReadFileArgs = serde_json::from_value(args)?;
        // Placeholder logic:
        let dummy_content = format!(
            "// Contents of file: {}\nfn main() {{ println!(\"Hello, world!\"); }}",
            args.path
        );
        tracing::debug!(path = %args.path, "Reading file (placeholder)");
        Ok(serde_json::to_string(&dummy_content)?)
    }
}

// -----------------------------------------------------------------------------
// WriteFileTool Implementation
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct WriteFileTool;

#[derive(Deserialize, schemars::JsonSchema)]
struct WriteFileArgs {
    /// The path of the file to write.
    path: String,
    /// The content to write to the file.
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> String {
        "write_file".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some("Writes or overwrites a file in the sandbox.".to_string()),
                parameters: Some(schema_for!(WriteFileArgs).into()),
                strict: None,
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let args: WriteFileArgs = serde_json::from_value(args)?;
        // Placeholder logic:
        tracing::debug!(path = %args.path, bytes = args.content.len(), "Writing file (placeholder)");
        Ok(json!({"status": "success", "bytes_written": args.content.len()}).to_string())
    }
}

// -----------------------------------------------------------------------------
// ExecuteCommandTool Implementation
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ExecuteCommandTool;

#[derive(Deserialize, schemars::JsonSchema)]
struct ExecuteCommandArgs {
    /// The command to execute (e.g., 'cargo', 'ls').
    command: String,
    /// A list of arguments for the command.
    args: Vec<String>,
}

#[async_trait]
impl Tool for ExecuteCommandTool {
    fn name(&self) -> String {
        "execute_command".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some("Executes a shell command in the sandbox.".to_string()),
                parameters: Some(schema_for!(ExecuteCommandArgs).into()),
                strict: None,
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let args: ExecuteCommandArgs = serde_json::from_value(args)?;
        // Placeholder logic:
        tracing::debug!(command = %args.command, args_count = args.args.len(), "Executing command (placeholder)");
        let output = if args.command == "cargo" && args.args.contains(&"test".to_string()) {
            CommandOutput {
                exit_code: 0,
                stdout: "running 3 tests\ntest tests::test_one ... ok\ntest tests::test_two ... ok\ntest tests::test_three ... ok\n\ntest result: ok. 3 passed; 0 failed"
                    .to_string(),
                stderr: "".to_string(),
            }
        } else {
            CommandOutput {
                exit_code: 1,
                stdout: "".to_string(),
                stderr: "Command not found or failed".to_string(),
            }
        };
        Ok(serde_json::to_string(&output)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_list_files_tool() {
        let tool = ListFilesTool;

        assert_eq!(tool.name(), "list_files");

        let schema = tool.schema();
        assert_eq!(schema.function.name, "list_files");
        assert!(schema.function.description.is_some());

        let args = json!({ "path": "./src" });
        let result = tool.call(args).await.unwrap();

        // Should return a list of dummy files
        let files: Vec<String> = serde_json::from_str(&result).unwrap();
        assert!(!files.is_empty());
        assert!(files.contains(&"README.md".to_string()));
    }

    #[tokio::test]
    async fn test_read_file_tool() {
        let tool = ReadFileTool;

        assert_eq!(tool.name(), "read_file");

        let schema = tool.schema();
        assert_eq!(schema.function.name, "read_file");
        assert!(schema.function.description.is_some());

        let args = json!({ "path": "./src/main.rs" });
        let result = tool.call(args).await.unwrap();

        // Should return dummy content
        let content: String = serde_json::from_str(&result).unwrap();
        assert!(content.contains("Contents of file"));
        assert!(content.contains("./src/main.rs"));
    }

    #[tokio::test]
    async fn test_write_file_tool() {
        let tool = WriteFileTool;

        assert_eq!(tool.name(), "write_file");

        let schema = tool.schema();
        assert_eq!(schema.function.name, "write_file");
        assert!(schema.function.description.is_some());

        let content = "fn main() { println!(\"test\"); }";
        let args = json!({ "path": "./test.rs", "content": content });
        let result = tool.call(args).await.unwrap();

        // Should return success status with bytes written
        let response: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(response["status"], "success");
        assert_eq!(response["bytes_written"], content.len());
    }

    #[tokio::test]
    async fn test_execute_command_tool_cargo_test() {
        let tool = ExecuteCommandTool;

        assert_eq!(tool.name(), "execute_command");

        let schema = tool.schema();
        assert_eq!(schema.function.name, "execute_command");
        assert!(schema.function.description.is_some());

        // Test cargo test command (should succeed)
        let args = json!({
            "command": "cargo",
            "args": ["test"]
        });
        let result = tool.call(args).await.unwrap();

        let output: CommandOutput = serde_json::from_str(&result).unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("3 passed"));
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    async fn test_execute_command_tool_unknown_command() {
        let tool = ExecuteCommandTool;

        // Test unknown command (should fail)
        let args = json!({
            "command": "unknown_cmd",
            "args": ["--help"]
        });
        let result = tool.call(args).await.unwrap();

        let output: CommandOutput = serde_json::from_str(&result).unwrap();
        assert_eq!(output.exit_code, 1);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.contains("Command not found"));
    }

    #[tokio::test]
    async fn test_list_files_invalid_args() {
        let tool = ListFilesTool;

        // Missing required "path" field
        let args = json!({});
        let result = tool.call(args).await;

        assert!(result.is_err(), "Should fail with missing path argument");
    }

    #[tokio::test]
    async fn test_read_file_invalid_args() {
        let tool = ReadFileTool;

        // Missing required "path" field
        let args = json!({});
        let result = tool.call(args).await;

        assert!(result.is_err(), "Should fail with missing path argument");
    }

    #[tokio::test]
    async fn test_write_file_invalid_args() {
        let tool = WriteFileTool;

        // Missing required fields
        let args = json!({ "path": "./test.rs" });
        let result = tool.call(args).await;

        assert!(result.is_err(), "Should fail with missing content argument");
    }

    #[tokio::test]
    async fn test_execute_command_invalid_args() {
        let tool = ExecuteCommandTool;

        // Missing required fields
        let args = json!({ "command": "ls" });
        let result = tool.call(args).await;

        assert!(result.is_err(), "Should fail with missing args argument");
    }

    #[test]
    fn test_command_output_serialization() {
        let output = CommandOutput {
            exit_code: 0,
            stdout: "Hello".to_string(),
            stderr: "".to_string(),
        };

        let json = serde_json::to_string(&output).unwrap();
        let deserialized: CommandOutput = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.exit_code, 0);
        assert_eq!(deserialized.stdout, "Hello");
        assert_eq!(deserialized.stderr, "");
    }
}
