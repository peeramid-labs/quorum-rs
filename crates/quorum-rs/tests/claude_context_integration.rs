//! Integration tests for Claude agent context file injection, directory access
//! isolation, and sub-agent configuration.
//!
//! These tests exercise `ClaudeAgent::build_command()` against real filesystem
//! layouts to verify:
//! - Context files are correctly read and inlined
//! - No unintended directory access is granted (security)
//! - `add_dirs` grants explicit access and deduplicates
//! - Sub-agent definitions serialize correctly for `--agents`
//! - YAML deserialization round-trips for all config fields

use quorum_rs::ClaudeAgent;
use quorum_rs::DeliberationPhase;
use quorum_rs::agents::config::{ClaudeProviderConfig, ClaudeSubAgentDef};
use quorum_rs::agents::{
    AgentConfig, CandidateProposal, Proposal, StructuredFeedback, UserInjection,
};
use quorum_rs::prompts::PromptSet;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

// ── Helpers ─────────────────────────────────────────────────────────────

fn agent_config(name: &str) -> AgentConfig {
    AgentConfig {
        name: name.to_string(),
        provider_id: "claude_cli".to_string(),
        model_name: "sonnet".to_string(),
        ..AgentConfig::default()
    }
}

fn context() -> quorum_rs::agents::AgentContext {
    quorum_rs::agents::AgentContext {
        task_description: "test task".to_string(),
        round_number: 1,
        total_rounds: 3,
        phase: DeliberationPhase::Proposing,
        phase_budget_remaining_secs: 60.0,
        ..Default::default()
    }
}

fn mcp_config_path() -> PathBuf {
    PathBuf::from("/tmp/nsed_test_mcp.json")
}

/// Stub PromptSet returning minimal prompts.
#[derive(Debug, Clone)]
struct StubPromptSet;

impl PromptSet for StubPromptSet {
    fn get_system_message(
        &self,
        _agent_name: &str,
        _round: usize,
        _total: usize,
        _phase: DeliberationPhase,
    ) -> String {
        "system".into()
    }

    fn get_proposer_prompt(
        &self,
        _task: &str,
        _matrix: Option<String>,
        _prev: Option<&Proposal>,
        _score: Option<f32>,
        _critiques: Vec<String>,
        _injections: &[UserInjection],
        _feedback: Option<&StructuredFeedback>,
    ) -> String {
        "propose".into()
    }

    fn get_batch_evaluator_prompt(
        &self,
        _task: &str,
        _candidates: &[CandidateProposal],
        _own: Option<&Proposal>,
        _round: usize,
        _injections: &[UserInjection],
    ) -> String {
        "evaluate".into()
    }

    fn get_summarizer_prompt(&self, _task: &str, _content: &str) -> String {
        "summary".into()
    }
}

fn stub_prompts() -> Arc<dyn PromptSet> {
    Arc::new(StubPromptSet)
}

/// Build a ClaudeAgent and return the generated command vec.
/// Uses `build_command_fresh` to test fresh-session behavior (system prompts,
/// context files, etc.). The default `build_command()` returns resumed mode
/// which omits these for token efficiency.
fn build(cfg: ClaudeProviderConfig) -> Vec<String> {
    let agent = ClaudeAgent::new(agent_config("test"), cfg, stub_prompts());
    let (cmd, _sandbox) = agent.build_command_fresh(&context(), &mcp_config_path());
    cmd
}

/// Build a ClaudeAgent using the default `build_command()` (resumed mode).
/// In resumed mode, Claude's persistent session already holds the base system
/// prompt and context files, so those must NOT be re-injected each call.
fn build_resumed(cfg: ClaudeProviderConfig) -> Vec<String> {
    let agent = ClaudeAgent::new(agent_config("test"), cfg, stub_prompts());
    let (cmd, _sandbox) = agent.build_command(&context(), &mcp_config_path());
    cmd
}

/// Count occurrences of `needle` as an exact element in `cmd`.
fn count_exact(cmd: &[String], needle: &str) -> usize {
    cmd.iter().filter(|a| a.as_str() == needle).count()
}

/// Find the argument immediately after `flag` in `cmd`.
fn arg_after(cmd: &[String], flag: &str) -> Option<String> {
    cmd.iter()
        .position(|a| a == flag)
        .and_then(|i| cmd.get(i + 1).cloned())
}

/// Create a temporary directory tree for context file tests.
///
/// ```text
/// root/
/// ├── docs/
/// │   ├── architecture.md
/// │   └── security.md
/// ├── specs/
/// │   └── api.json
/// ├── secrets/
/// │   └── .env
/// └── empty/
/// ```
fn create_project_tree() -> TempDir {
    let root = tempfile::tempdir().unwrap();
    let docs = root.path().join("docs");
    let specs = root.path().join("specs");
    let secrets = root.path().join("secrets");
    let empty = root.path().join("empty");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::create_dir_all(&specs).unwrap();
    std::fs::create_dir_all(&secrets).unwrap();
    std::fs::create_dir_all(&empty).unwrap();

    std::fs::write(
        docs.join("architecture.md"),
        "# Architecture\n\nMicroservices with NATS messaging.",
    )
    .unwrap();
    std::fs::write(
        docs.join("security.md"),
        "# Security Policy\n\nAll endpoints require auth.",
    )
    .unwrap();
    std::fs::write(
        specs.join("api.json"),
        r#"{"openapi":"3.0","paths":{"/health":{}}}"#,
    )
    .unwrap();
    std::fs::write(secrets.join(".env"), "SECRET_KEY=hunter2\nDB_PASS=letmein").unwrap();
    root
}

// ═══════════════════════════════════════════════════════════════════════
// ISOLATION TESTS — verify no unintended access leaks
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn context_files_never_leak_parent_directory() {
    let root = create_project_tree();
    let docs_dir = root.path().join("docs").display().to_string();
    let specs_dir = root.path().join("specs").display().to_string();
    let root_dir = root.path().display().to_string();

    let cfg = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![
            PathBuf::from("docs/architecture.md"),
            PathBuf::from("specs/api.json"),
        ],
        ..Default::default()
    };
    let cmd = build(cfg);

    // Files should be inlined
    let ctx_blocks: Vec<_> = cmd.iter().filter(|a| a.contains("<context_file")).collect();
    assert_eq!(ctx_blocks.len(), 2, "both context files should be inlined");

    // Only the sandbox --add-dir should appear, not context file parent dirs
    assert!(
        cmd.contains(&"--add-dir".to_string()),
        "sandbox --add-dir must always be present"
    );
    assert!(
        !cmd.contains(&docs_dir),
        "docs/ dir must not leak from context_files"
    );
    assert!(
        !cmd.contains(&specs_dir),
        "specs/ dir must not leak from context_files"
    );
    assert!(
        !cmd.contains(&root_dir),
        "root dir must not leak from context_files"
    );
}

#[test]
fn context_files_do_not_leak_sibling_directories() {
    let root = create_project_tree();
    // Only request docs/ file — specs/ and secrets/ must not be accessible
    let cfg = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![PathBuf::from("docs/architecture.md")],
        ..Default::default()
    };
    let cmd = build(cfg);

    let cmd_str = cmd.join(" ");
    assert!(
        !cmd_str.contains("specs"),
        "sibling directory 'specs' must not appear in command"
    );
    assert!(
        !cmd_str.contains("secrets"),
        "sibling directory 'secrets' must not appear in command"
    );
    assert!(
        !cmd_str.contains(".env"),
        "secret files must never appear in command"
    );
}

#[test]
fn secret_files_only_appear_when_explicitly_listed() {
    let root = create_project_tree();

    // Without secrets in context_files
    let cfg_safe = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![PathBuf::from("docs/architecture.md")],
        ..Default::default()
    };
    let cmd_safe = build(cfg_safe);
    assert!(
        !cmd_safe.iter().any(|a| a.contains("hunter2")),
        "secrets must not leak when not listed in context_files"
    );

    // With secrets explicitly listed — user's choice, should work
    let cfg_explicit = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![PathBuf::from("secrets/.env")],
        ..Default::default()
    };
    let cmd_explicit = build(cfg_explicit);
    assert!(
        cmd_explicit.iter().any(|a| a.contains("hunter2")),
        "explicitly listed secret file should be inlined"
    );
}

#[test]
fn add_dirs_do_not_appear_from_context_files_parent() {
    let root = create_project_tree();
    let docs_dir = root.path().join("docs").display().to_string();
    let specs_dir = root.path().join("specs").display().to_string();

    let cfg = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![
            PathBuf::from("docs/architecture.md"),
            PathBuf::from("docs/security.md"),
            PathBuf::from("specs/api.json"),
        ],
        ..Default::default()
    };
    let cmd = build(cfg);

    // Neither docs/ nor specs/ should be in --add-dir
    assert_eq!(count_exact(&cmd, &docs_dir), 0);
    assert_eq!(count_exact(&cmd, &specs_dir), 0);
}

#[test]
fn missing_context_file_does_not_leak_path() {
    let root = create_project_tree();
    let cfg = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![
            PathBuf::from("docs/architecture.md"),   // exists
            PathBuf::from("nonexistent/phantom.md"), // does not exist
        ],
        ..Default::default()
    };
    let cmd = build(cfg);

    // Only 1 context block (the existing file)
    let ctx_count = cmd.iter().filter(|a| a.contains("<context_file")).count();
    assert_eq!(
        ctx_count, 1,
        "only existing files should produce context blocks"
    );

    // The missing path should not appear anywhere in the command
    assert!(
        !cmd.iter().any(|a| a.contains("phantom")),
        "missing file path must not leak into command"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// ACCESS TESTS — verify intended access works correctly
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn context_file_contents_fully_inlined() {
    let root = create_project_tree();
    let cfg = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![PathBuf::from("docs/architecture.md")],
        ..Default::default()
    };
    let cmd = build(cfg);

    let block = cmd
        .iter()
        .find(|a| a.contains("<context_file"))
        .expect("context_file block should exist");

    // Full content present
    assert!(block.contains("# Architecture"));
    assert!(block.contains("Microservices with NATS messaging."));
    // Filename attribute
    assert!(block.contains("name=\"architecture.md\""));
    // Wrapped in XML tags
    assert!(block.starts_with("<context_file"));
    assert!(block.ends_with("</context_file>"));
}

#[test]
fn context_file_preserves_json_content() {
    let root = create_project_tree();
    let cfg = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![PathBuf::from("specs/api.json")],
        ..Default::default()
    };
    let cmd = build(cfg);

    let block = cmd.iter().find(|a| a.contains("<context_file")).unwrap();
    assert!(block.contains(r#""openapi":"3.0""#));
    assert!(block.contains(r#""/health""#));
}

#[test]
fn multiple_context_files_from_different_dirs() {
    let root = create_project_tree();
    let cfg = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![
            PathBuf::from("docs/architecture.md"),
            PathBuf::from("docs/security.md"),
            PathBuf::from("specs/api.json"),
        ],
        ..Default::default()
    };
    let cmd = build(cfg);

    let blocks: Vec<_> = cmd.iter().filter(|a| a.contains("<context_file")).collect();
    assert_eq!(blocks.len(), 3);

    // Each file's content is present
    assert!(blocks.iter().any(|b| b.contains("Microservices")));
    assert!(
        blocks
            .iter()
            .any(|b| b.contains("All endpoints require auth"))
    );
    assert!(blocks.iter().any(|b| b.contains("openapi")));

    // Each is preceded by --append-system-prompt
    for (i, a) in cmd.iter().enumerate() {
        if a.contains("<context_file") {
            assert_eq!(cmd[i - 1], "--append-system-prompt");
        }
    }
}

#[test]
fn relative_context_files_resolve_from_working_dir() {
    let root = create_project_tree();
    let cfg = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![PathBuf::from("docs/architecture.md")],
        ..Default::default()
    };
    let cmd = build(cfg);

    // Should work — resolved relative to working_dir
    assert!(cmd.iter().any(|a| a.contains("Microservices")));
}

#[test]
fn absolute_context_files_work() {
    let root = create_project_tree();
    let abs_path = root.path().join("docs/architecture.md");
    let cfg = ClaudeProviderConfig {
        context_files: vec![abs_path],
        ..Default::default()
    };
    let cmd = build(cfg);

    assert!(cmd.iter().any(|a| a.contains("Microservices")));
}

#[test]
fn add_dirs_grants_explicit_access() {
    let root = create_project_tree();
    let docs_dir = root.path().join("docs");
    let specs_dir = root.path().join("specs");

    let cfg = ClaudeProviderConfig {
        add_dirs: vec![docs_dir.clone(), specs_dir.clone()],
        ..Default::default()
    };
    let cmd = build(cfg);

    assert!(cmd.contains(&"--add-dir".to_string()));
    assert!(cmd.contains(&docs_dir.display().to_string()));
    assert!(cmd.contains(&specs_dir.display().to_string()));
}

#[test]
fn add_dirs_deduplicates() {
    let root = create_project_tree();
    let docs_dir = root.path().join("docs");

    let cfg = ClaudeProviderConfig {
        add_dirs: vec![docs_dir.clone(), docs_dir.clone(), docs_dir],
        ..Default::default()
    };
    let cmd = build(cfg);

    let count = cmd.iter().filter(|a| a.as_str() == "--add-dir").count();
    assert_eq!(
        count, 1,
        "duplicate add_dirs should be deduplicated to one --add-dir"
    );
}

#[test]
fn add_dirs_and_context_files_are_independent() {
    let root = create_project_tree();
    let docs_dir = root.path().join("docs");

    // Context file from docs/ + explicit add_dir for specs/ only
    let cfg = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![PathBuf::from("docs/architecture.md")],
        add_dirs: vec![root.path().join("specs")],
        ..Default::default()
    };
    let cmd = build(cfg);

    // Context file inlined
    assert!(cmd.iter().any(|a| a.contains("<context_file")));

    // Only specs/ granted via --add-dir, NOT docs/
    let specs_str = root.path().join("specs").display().to_string();
    let docs_str = docs_dir.display().to_string();
    assert!(
        cmd.contains(&specs_str),
        "explicitly listed add_dir should be present"
    );
    assert!(
        !cmd.contains(&docs_str),
        "context_file parent should NOT be auto-added"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// SUB-AGENT SERIALIZATION TESTS
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn subagent_minimal_config_serializes() {
    let mut agents = HashMap::new();
    agents.insert(
        "reviewer".to_string(),
        ClaudeSubAgentDef {
            description: "Reviews code".to_string(),
            prompt: "You are a code reviewer".to_string(),
            ..Default::default()
        },
    );
    let cfg = ClaudeProviderConfig {
        agents,
        ..Default::default()
    };
    let cmd = build(cfg);

    let json_str = arg_after(&cmd, "--agents").expect("--agents should be present");
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed["reviewer"]["description"], "Reviews code");
    assert_eq!(parsed["reviewer"]["prompt"], "You are a code reviewer");
}

#[test]
fn subagent_full_config_serializes_all_fields() {
    let mut agents = HashMap::new();
    agents.insert(
        "db-reader".to_string(),
        ClaudeSubAgentDef {
            description: "Read-only DB".to_string(),
            prompt: "SELECT only".to_string(),
            tools: vec!["Bash".into()],
            disallowed_tools: vec!["Write".into(), "Edit".into()],
            model: Some("haiku".into()),
            permission_mode: Some("dontAsk".into()),
            max_turns: Some(5),
            mcp_servers: vec![
                serde_json::json!("github"),
                serde_json::json!({
                    "playwright": {
                        "type": "stdio",
                        "command": "npx",
                        "args": ["-y", "@playwright/mcp@latest"]
                    }
                }),
            ],
            effort: Some("medium".into()),
            background: Some(true),
            isolation: Some("worktree".into()),
            memory: Some("project".into()),
            skills: vec!["sql-patterns".into()],
            initial_prompt: Some("Start by listing tables".into()),
        },
    );
    let cfg = ClaudeProviderConfig {
        agents,
        ..Default::default()
    };
    let cmd = build(cfg);

    let json_str = arg_after(&cmd, "--agents").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let db = &parsed["db-reader"];

    assert_eq!(db["description"], "Read-only DB");
    assert_eq!(db["prompt"], "SELECT only");
    assert_eq!(db["tools"], serde_json::json!(["Bash"]));
    assert_eq!(db["disallowedTools"], serde_json::json!(["Write", "Edit"]));
    assert_eq!(db["model"], "haiku");
    assert_eq!(db["permissionMode"], "dontAsk");
    assert_eq!(db["maxTurns"], 5);
    assert_eq!(db["effort"], "medium");
    assert_eq!(db["background"], true);
    assert_eq!(db["isolation"], "worktree");
    assert_eq!(db["memory"], "project");
    assert_eq!(db["skills"], serde_json::json!(["sql-patterns"]));
    assert_eq!(db["initialPrompt"], "Start by listing tables");
    // MCP servers: string ref + inline definition
    assert_eq!(db["mcpServers"][0], "github");
    assert!(db["mcpServers"][1]["playwright"].is_object());
}

#[test]
fn subagent_empty_optional_fields_omitted_from_json() {
    let mut agents = HashMap::new();
    agents.insert(
        "simple".to_string(),
        ClaudeSubAgentDef {
            description: "Simple agent".to_string(),
            prompt: "Do stuff".to_string(),
            ..Default::default()
        },
    );
    let cfg = ClaudeProviderConfig {
        agents,
        ..Default::default()
    };
    let cmd = build(cfg);

    let json_str = arg_after(&cmd, "--agents").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let simple = &parsed["simple"];

    // Optional fields with skip_serializing_if should not be present
    assert!(
        simple.get("model").is_none(),
        "model should be omitted when None"
    );
    assert!(simple.get("permissionMode").is_none());
    assert!(simple.get("maxTurns").is_none());
    assert!(simple.get("effort").is_none());
    assert!(simple.get("background").is_none());
    assert!(simple.get("isolation").is_none());
    assert!(simple.get("memory").is_none());
    assert!(simple.get("initialPrompt").is_none());
    // Empty vecs should also be omitted
    assert!(
        simple.get("tools").is_none(),
        "empty tools should be omitted"
    );
    assert!(simple.get("disallowedTools").is_none());
    assert!(simple.get("skills").is_none());
    assert!(simple.get("mcpServers").is_none());
}

#[test]
fn multiple_subagents_all_present() {
    let mut agents = HashMap::new();
    agents.insert(
        "researcher".to_string(),
        ClaudeSubAgentDef {
            description: "Research".to_string(),
            prompt: "Research stuff".to_string(),
            model: Some("haiku".into()),
            ..Default::default()
        },
    );
    agents.insert(
        "writer".to_string(),
        ClaudeSubAgentDef {
            description: "Write".to_string(),
            prompt: "Write stuff".to_string(),
            model: Some("sonnet".into()),
            ..Default::default()
        },
    );
    agents.insert(
        "reviewer".to_string(),
        ClaudeSubAgentDef {
            description: "Review".to_string(),
            prompt: "Review stuff".to_string(),
            tools: vec!["Read".into()],
            ..Default::default()
        },
    );
    let cfg = ClaudeProviderConfig {
        agents,
        ..Default::default()
    };
    let cmd = build(cfg);

    let json_str = arg_after(&cmd, "--agents").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert!(parsed["researcher"].is_object());
    assert!(parsed["writer"].is_object());
    assert!(parsed["reviewer"].is_object());
    assert_eq!(parsed["researcher"]["model"], "haiku");
    assert_eq!(parsed["writer"]["model"], "sonnet");
    assert_eq!(parsed["reviewer"]["tools"], serde_json::json!(["Read"]));
}

#[test]
fn no_agents_flag_when_empty() {
    let cfg = ClaudeProviderConfig::default();
    let cmd = build(cfg);
    assert!(
        !cmd.contains(&"--agents".to_string()),
        "--agents must not appear when no sub-agents defined"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// YAML ROUND-TRIP — verify config deserializes from YAML correctly
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn yaml_config_round_trip() {
    let yaml = r#"
model: opus
permission_mode: acceptEdits
max_budget_usd: 1.50
context_files:
  - docs/arch.md
  - specs/api.json
add_dirs:
  - ./src
  - /data/shared
agents:
  researcher:
    description: "Searches documentation"
    prompt: "You research topics"
    tools: ["Read", "Grep", "Glob"]
    model: haiku
    maxTurns: 10
    effort: medium
  security-reviewer:
    description: "Reviews for vulnerabilities"
    prompt: "You find security issues"
    disallowedTools: ["Write", "Bash"]
    permissionMode: dontAsk
    background: true
extra_args:
  - "--verbose"
"#;

    let cfg: ClaudeProviderConfig = serde_yaml::from_str(yaml).unwrap();

    assert_eq!(cfg.model, Some("opus".to_string()));
    assert_eq!(cfg.permission_mode, "acceptEdits");
    assert_eq!(cfg.max_budget_usd, Some(1.5));
    assert_eq!(cfg.context_files.len(), 2);
    assert_eq!(cfg.add_dirs.len(), 2);
    assert_eq!(cfg.agents.len(), 2);

    let researcher = &cfg.agents["researcher"];
    assert_eq!(researcher.description, "Searches documentation");
    assert_eq!(researcher.tools, vec!["Read", "Grep", "Glob"]);
    assert_eq!(researcher.model, Some("haiku".into()));
    assert_eq!(researcher.max_turns, Some(10));
    assert_eq!(researcher.effort, Some("medium".into()));

    let security = &cfg.agents["security-reviewer"];
    assert_eq!(security.disallowed_tools, vec!["Write", "Bash"]);
    assert_eq!(security.permission_mode, Some("dontAsk".into()));
    assert_eq!(security.background, Some(true));

    assert_eq!(cfg.extra_args, vec!["--verbose"]);
}

#[test]
fn yaml_minimal_config_uses_defaults() {
    let yaml = "{}";
    let cfg: ClaudeProviderConfig = serde_yaml::from_str(yaml).unwrap();

    assert_eq!(cfg.model, None);
    assert_eq!(cfg.permission_mode, "bypassPermissions");
    assert!(cfg.context_files.is_empty());
    assert!(cfg.add_dirs.is_empty());
    assert!(cfg.agents.is_empty());
    assert!(cfg.extra_args.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════
// COMBINED SCENARIO — full realistic config
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn full_realistic_config_produces_correct_command() {
    let root = create_project_tree();
    let docs_dir = root.path().join("docs");
    let specs_dir = root.path().join("specs");

    let mut agents = HashMap::new();
    agents.insert(
        "researcher".to_string(),
        ClaudeSubAgentDef {
            description: "Searches docs".to_string(),
            prompt: "Research topics".to_string(),
            tools: vec!["Read".into(), "Grep".into()],
            model: Some("haiku".into()),
            max_turns: Some(5),
            ..Default::default()
        },
    );

    let cfg = ClaudeProviderConfig {
        model: Some("opus".to_string()),
        working_dir: Some(root.path().to_path_buf()),
        permission_mode: "acceptEdits".to_string(),
        max_budget_usd: Some(2.0),
        context_files: vec![
            PathBuf::from("docs/architecture.md"),
            PathBuf::from("specs/api.json"),
        ],
        add_dirs: vec![docs_dir.clone(), specs_dir.clone()],
        agents,
        allowed_tools: vec!["Read".into(), "Grep".into(), "Bash(git:*)".into()],
        extra_args: vec!["--verbose".into()],
        ..Default::default()
    };
    let cmd = build(cfg);

    // Model
    assert!(cmd.contains(&"opus".to_string()));

    // Permission mode
    assert!(cmd.contains(&"acceptEdits".to_string()));

    // Budget
    assert!(cmd.contains(&"2".to_string()));

    // Context files inlined (2 blocks)
    let ctx_count = cmd.iter().filter(|a| a.contains("<context_file")).count();
    assert_eq!(ctx_count, 2);

    // Explicit add_dirs present
    assert!(cmd.contains(&docs_dir.display().to_string()));
    assert!(cmd.contains(&specs_dir.display().to_string()));

    // Sub-agents present
    let agents_json = arg_after(&cmd, "--agents").unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&agents_json).unwrap();
    assert_eq!(parsed["researcher"]["model"], "haiku");

    // Allowed tools
    assert!(cmd.contains(&"Read,Grep,Bash(git:*)".to_string()));

    // Extra args
    assert!(cmd.contains(&"--verbose".to_string()));

    // Security: no auto --add-dir from context_files
    // The only --add-dir entries should be from explicit add_dirs
    let add_dir_indices: Vec<_> = cmd
        .iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--add-dir")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        add_dir_indices.len(),
        2,
        "exactly 2 --add-dir flags expected"
    );
    for idx in add_dir_indices {
        let dir_val = &cmd[idx + 1];
        assert!(
            dir_val == &docs_dir.display().to_string()
                || dir_val == &specs_dir.display().to_string(),
            "unexpected --add-dir value: {dir_val}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RESUMED-PATH TESTS — verify the default `build_command()` produces a
// `--resume` invocation and omits static context that the persisted Claude
// session already holds.
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn resumed_path_uses_resume_flag_and_omits_base_system_prompt() {
    let cfg = ClaudeProviderConfig::default();
    let cmd = build_resumed(cfg);

    assert!(
        cmd.contains(&"--resume".to_string()),
        "resumed build_command() must pass --resume"
    );
    assert!(
        !cmd.contains(&"--system-prompt".to_string()),
        "resumed build_command() must NOT re-inject --system-prompt"
    );
}

#[test]
fn resumed_path_skips_context_files() {
    let root = create_project_tree();
    let cfg = ClaudeProviderConfig {
        working_dir: Some(root.path().to_path_buf()),
        context_files: vec![
            PathBuf::from("docs/architecture.md"),
            PathBuf::from("specs/api.json"),
        ],
        ..Default::default()
    };
    let cmd = build_resumed(cfg);

    // Static context files should NOT be re-inlined on resumed calls —
    // Claude's persistent session already has them from the fresh turn.
    let ctx_blocks: Vec<_> = cmd.iter().filter(|a| a.contains("<context_file")).collect();
    assert!(
        ctx_blocks.is_empty(),
        "resumed build_command() must not re-inline context files, got: {ctx_blocks:?}"
    );
}
