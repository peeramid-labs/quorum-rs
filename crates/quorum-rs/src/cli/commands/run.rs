use std::path::Path;
use std::process::ExitCode;

use crate::cli::remote::{DiscoveredPolicy, JobOutcome, RemoteOrchestrator};
use crate::cli::request::{DeliberationRequest, build_request, build_request_raw_policy_id};
use crate::cli::thread::{Message, Thread, ThreadStore};
use crate::cli::workspace::{OrchestratorConfig, OrchestratorMode, PolicyConfig, WorkspaceConfig};
use crate::config::resolve_env_token;

/// Resolved run context: the request, orchestrator info, and optionally the
/// role-based policy that needs to be pushed to the remote before submission.
#[derive(Debug)]
struct ResolvedRun<'a> {
    req: DeliberationRequest,
    orch_name: String,
    orch: &'a OrchestratorConfig,
    /// `(policy_name, policy_config)` — present when policy is role-based
    /// and must be registered on the remote orchestrator before submitting.
    role_policy: Option<(String, &'a PolicyConfig)>,
}

/// How `run` ties this invocation to a stored thread.
pub enum ResumeMode {
    /// Resume a specific thread by id; error if it does not exist.
    ById(String),
    /// Resume the most recent thread, or start a fresh one if none exist.
    Latest,
}

/// A short thread subject from the first line of the task, truncated.
fn thread_subject(task: &str) -> String {
    let first = task.lines().next().unwrap_or("").trim();
    let title: String = first.chars().take(50).collect();
    if title.is_empty() {
        "thread".to_string()
    } else {
        title
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    config_path: &Path,
    task: &str,
    room_flag: Option<&str>,
    policy_flag: Option<&str>,
    files: &[std::path::PathBuf],
    output_dir: Option<&Path>,
    output_file: Option<&Path>,
    force_output: bool,
    resume: Option<ResumeMode>,
) -> ExitCode {
    if !files.is_empty() {
        eprintln!(
            "error: --files flag is not yet supported (context delivery is agent-side, see #131)"
        );
        return ExitCode::FAILURE;
    }

    if let Some(dir) = output_dir
        && let Err(e) = prepare_output_dir(dir, force_output)
    {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    let config = match WorkspaceConfig::load_or_remote_default(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Thread continuation: when resuming, load the thread, assemble the
    // full conversation into the submitted query, and inherit the thread's
    // policy when none was passed explicitly. `task` stays the raw user message
    // (recorded as the user turn); `submit_task` is what the deliberation sees.
    let store = ThreadStore::new();
    let mut thread: Option<Thread> = match &resume {
        Some(ResumeMode::ById(id)) => match store.load(id) {
            Some(s) => Some(s),
            None => {
                eprintln!("error: no thread '{id}'");
                return ExitCode::FAILURE;
            }
        },
        Some(ResumeMode::Latest) => Some(
            store
                .latest()
                .unwrap_or_else(|| Thread::new(thread_subject(task))),
        ),
        None => None,
    };
    let assembled = thread.as_ref().map(|s| s.to_deliberation_query(task));
    let submit_task = assembled.as_deref().unwrap_or(task);
    // Owned so it doesn't borrow `thread`, which is mutated after the result.
    let effective_policy: Option<String> = policy_flag
        .map(String::from)
        .or_else(|| thread.as_ref().and_then(|s| s.active_policy.clone()));

    // Resolve policy + orchestrator depending on flags
    let resolved = if let Some(policy_ref) = effective_policy.as_deref() {
        match resolve_adhoc_run(&config, policy_ref, submit_task, room_flag) {
            Ok(result) => result,
            // Not a local policy name or hex id — try resolving it as a
            // remote policy label via the orchestrator's /policies catalog.
            Err(local_err) => {
                match resolve_remote_policy(&config, policy_ref, submit_task, room_flag).await {
                    Ok(Some(result)) => {
                        eprintln!("Resolved remote policy '{policy_ref}'");
                        result
                    }
                    Ok(None) => {
                        eprintln!("error: {local_err}");
                        return ExitCode::FAILURE;
                    }
                    Err(e) => {
                        eprintln!("error: resolving remote policy '{policy_ref}': {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
        }
    } else {
        match resolve_room_run(&config, room_flag, submit_task) {
            Ok(result) => result,
            Err(msg) => {
                eprintln!("error: {msg}");
                return ExitCode::FAILURE;
            }
        }
    };

    if resolved.orch.mode.as_ref() != Some(&OrchestratorMode::Remote) {
        eprintln!(
            "error: orchestrator '{}' is not in remote mode; embedded mode is not yet supported (see #130)",
            resolved.orch_name
        );
        return ExitCode::FAILURE;
    }

    let address = match resolve_orch_field(&resolved.orch.address, "address", &resolved.orch_name) {
        Ok(a) => a,
        Err(code) => return code,
    };
    let token = match resolve_orch_field(&resolved.orch.token, "token", &resolved.orch_name) {
        Ok(t) => t,
        Err(code) => return code,
    };

    let client = match RemoteOrchestrator::new(&address, &token) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Auto-push role-based policies to the remote orchestrator before submission.
    // Without this, the orchestrator returns 404 because it doesn't know the policy hash.
    if let Some((policy_name, policy_config)) = resolved.role_policy {
        match client.push_policy(&policy_name, policy_config).await {
            Ok(result) => {
                if result.created {
                    eprintln!(
                        "Policy '{}' registered (id: {})",
                        policy_name, result.policy_id
                    );
                } else {
                    eprintln!(
                        "Policy '{}' already registered (id: {})",
                        policy_name, result.policy_id
                    );
                }
            }
            Err(e) => {
                eprintln!("error: failed to register policy '{}': {e}", policy_name);
                return ExitCode::FAILURE;
            }
        }
    }

    let job_id = match client.submit(&resolved.req).await {
        Ok(id) => {
            eprintln!("Job submitted: {id}");
            id
        }
        Err(e) => {
            eprintln!("error: failed to submit job: {e}");
            return ExitCode::FAILURE;
        }
    };

    match client.stream_events(&job_id).await {
        Ok(JobOutcome::Success(payload)) => {
            eprintln!(
                "\nResult ({} rounds, score {:.2}, by {}):",
                payload.rounds_completed, payload.best_proposal_score, payload.best_proposal_author
            );
            // Where the agreed answer lives, when the deliberation kept one. What is
            // printed below is a copy; this is the thing itself, and the only form
            // that can be fetched again or checked later.
            if let Some(at) = &payload.consensus {
                if at.hosted {
                    eprintln!(
                        "Answer: {} in {} ({} @ {})",
                        at.file, at.repo, at.branch, at.commit
                    );
                    eprintln!("  git clone {} && git checkout {}", at.repo, at.commit);
                } else {
                    eprintln!(
                        "Answer: {} at {} ({} @ {}) — local to the agent host, not fetchable from here",
                        at.file, at.repo, at.branch, at.commit
                    );
                }
            }
            if let Some(dir) = output_dir {
                let dispatch = serde_json::json!({
                    "job_id": job_id,
                    "rounds_completed": payload.rounds_completed,
                    "best_proposal_author": payload.best_proposal_author,
                    "best_proposal_score": payload.best_proposal_score,
                });
                if let Err(e) =
                    write_run_artifacts(dir, submit_task, &payload.best_proposal_content, &dispatch)
                {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
                eprintln!("Artifacts written to {}", dir.display());
            }
            if let Some(path) = output_file {
                if let Err(e) = std::fs::write(path, &payload.best_proposal_content) {
                    eprintln!(
                        "error: failed to write --output-file {}: {e}",
                        path.display()
                    );
                    return ExitCode::FAILURE;
                }
                eprintln!("Verdict written to {}", path.display());
            }
            if output_dir.is_none() && output_file.is_none() {
                println!("{}", payload.best_proposal_content);
            }
            // Record this exchange into the thread and persist it, so the
            // conversation can be resumed. `task` is the raw user message; the
            // assistant turn carries the answer + which policy produced it.
            if let Some(sess) = thread.as_mut() {
                if effective_policy.is_some() {
                    sess.active_policy = effective_policy.clone();
                }
                sess.push_message(Message::now("user", task));
                let mut answer = Message::now("assistant", &payload.best_proposal_content);
                answer.policy_id = effective_policy.clone();
                answer.job_id = Some(job_id.clone());
                sess.push_message(answer);
                match store.save(sess) {
                    Ok(()) => eprintln!(
                        "Session saved: {} (resume with --resume {})",
                        sess.id, sess.id
                    ),
                    Err(e) => eprintln!("warning: failed to save thread: {e}"),
                }
            }
            ExitCode::SUCCESS
        }
        Ok(JobOutcome::Failed(status)) => {
            eprintln!("error: deliberation failed: {status}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: streaming failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve the task input from positional argv or `--task-file`.
/// `task_file` resolves relative to `config_dir` (the workspace
/// config's parent), matching how `context_files` resolves and
/// preventing the "ran from the wrong directory" trap.
pub fn resolve_task_input(
    positional: Option<&str>,
    task_file: Option<&Path>,
    config_dir: &Path,
) -> Result<String, String> {
    match (positional, task_file) {
        (Some(t), None) => Ok(t.to_string()),
        (None, Some(p)) => {
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                config_dir.join(p)
            };
            let raw = std::fs::read_to_string(&abs)
                .map_err(|e| format!("failed to read --task-file {}: {e}", abs.display()))?;
            let trimmed = raw.trim_end().to_string();
            if trimmed.is_empty() {
                return Err(format!("--task-file {} is empty", abs.display()));
            }
            Ok(trimmed)
        }
        (Some(_), Some(_)) => Err("positional task and --task-file are mutually exclusive".into()),
        (None, None) => Err("either a positional task or --task-file is required".into()),
    }
}

/// Refuse to overwrite an existing `verdict.md` unless `force` is
/// set. Creates the directory if missing.
pub(crate) fn prepare_output_dir(dir: &Path, force: bool) -> Result<(), String> {
    let verdict = dir.join("verdict.md");
    if verdict.exists() && !force {
        return Err(format!(
            "{} already exists; pass --force-output to overwrite",
            verdict.display()
        ));
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("failed to create output dir {}: {e}", dir.display()))?;
    Ok(())
}

/// Write the post-run artifacts (verdict.md, prompt.txt,
/// dispatch.json) to `dir`. Caller must have prepared the dir
/// via [`prepare_output_dir`] first.
pub(crate) fn write_run_artifacts(
    dir: &Path,
    prompt: &str,
    verdict: &str,
    dispatch: &serde_json::Value,
) -> Result<(), String> {
    std::fs::write(dir.join("verdict.md"), verdict)
        .map_err(|e| format!("failed to write verdict.md: {e}"))?;
    std::fs::write(dir.join("prompt.txt"), prompt)
        .map_err(|e| format!("failed to write prompt.txt: {e}"))?;
    let dispatch_str = serde_json::to_string_pretty(dispatch)
        .map_err(|e| format!("failed to serialize dispatch.json: {e}"))?;
    std::fs::write(dir.join("dispatch.json"), dispatch_str)
        .map_err(|e| format!("failed to write dispatch.json: {e}"))?;
    Ok(())
}

/// Check if a string looks like a raw SHA-256 hex hash (64 hex chars).
fn is_hex_hash(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Resolve ad-hoc `--policy` run: lookup policy by name or use raw hash.
fn resolve_adhoc_run<'a>(
    config: &'a WorkspaceConfig,
    policy_ref: &str,
    task: &str,
    room_flag: Option<&str>,
) -> Result<ResolvedRun<'a>, String> {
    let (orch_name, orch) = resolve_orchestrator(config, room_flag)?;

    if let Some(policy) = config.policies.get(policy_ref) {
        let req = build_request("adhoc", policy, task)?;
        let role_policy = if policy.roles.is_some() {
            Some((policy_ref.to_string(), policy))
        } else {
            None
        };
        Ok(ResolvedRun {
            req,
            orch_name,
            orch,
            role_policy,
        })
    } else if is_hex_hash(policy_ref) {
        let req = build_request_raw_policy_id(policy_ref, task);
        Ok(ResolvedRun {
            req,
            orch_name,
            orch,
            role_policy: None,
        })
    } else {
        let available: Vec<&str> = config.policies.keys().map(|s| s.as_str()).collect();
        Err(format!(
            "unknown policy '{}' (available: {})",
            policy_ref,
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        ))
    }
}

/// Find a remote policy's hash id by its human label (`name`), falling back
/// to an exact `policy_id` match. Returns the `policy_id` to submit with.
fn find_policy_id_by_label(policies: &[DiscoveredPolicy], policy_ref: &str) -> Option<String> {
    policies
        .iter()
        .find(|p| p.name == policy_ref || p.policy_id == policy_ref)
        .map(|p| p.policy_id.clone())
}

/// Resolve `--policy <ref>` as a REMOTE policy label: query the
/// orchestrator's grant-filtered `/policies` and match `ref` against a
/// registered policy's name. Lets operators submit to an orchestrator-
/// registered policy by its human label without defining it locally.
///
/// Returns `Ok(None)` when no remote policy matches (caller falls back to
/// the local "unknown policy" error).
async fn resolve_remote_policy<'a>(
    config: &'a WorkspaceConfig,
    policy_ref: &str,
    task: &str,
    room_flag: Option<&str>,
) -> Result<Option<ResolvedRun<'a>>, String> {
    let (orch_name, orch) = resolve_orchestrator(config, room_flag)?;
    if orch.mode.as_ref() != Some(&OrchestratorMode::Remote) {
        return Ok(None);
    }
    let address = resolve_orch_field(&orch.address, "address", &orch_name)
        .map_err(|_| format!("orchestrator '{orch_name}' has no address configured"))?;
    let token = resolve_orch_field(&orch.token, "token", &orch_name)
        .map_err(|_| format!("orchestrator '{orch_name}' has no token configured"))?;
    let client = RemoteOrchestrator::new(&address, &token).map_err(|e| e.to_string())?;
    let policies = client
        .discover_policies()
        .await
        .map_err(|e| e.to_string())?;
    Ok(
        find_policy_id_by_label(&policies, policy_ref).map(|policy_id| ResolvedRun {
            req: build_request_raw_policy_id(&policy_id, task),
            orch_name,
            orch,
            role_policy: None,
        }),
    )
}

/// Resolve room-based run: standard room → policy → orchestrator flow.
fn resolve_room_run<'a>(
    config: &'a WorkspaceConfig,
    room_flag: Option<&str>,
    task: &str,
) -> Result<ResolvedRun<'a>, String> {
    let (room_name, room) = config.resolve_room(room_flag).map_err(|e| e.to_string())?;

    let policy_name = &room.policy;
    let policy = config.policies.get(policy_name).ok_or_else(|| {
        format!(
            "room '{}' references unknown policy '{}'",
            room_name, policy_name
        )
    })?;

    let orch_name = resolve_orch_name_from_room(config, room_name, room.orchestrator.as_deref())?;
    let orch = config
        .orchestrators
        .get(&orch_name)
        .ok_or_else(|| format!("orchestrator '{}' not found", orch_name))?;

    let req = build_request(room_name, policy, task)?;
    let role_policy = if policy.roles.is_some() {
        Some((policy_name.clone(), policy))
    } else {
        None
    };
    Ok(ResolvedRun {
        req,
        orch_name,
        orch,
        role_policy,
    })
}

/// Pick orchestrator for ad-hoc runs: if --room also given, use that room's
/// orchestrator. Otherwise, use the first remote orchestrator available.
fn resolve_orchestrator<'a>(
    config: &'a WorkspaceConfig,
    room_flag: Option<&str>,
) -> Result<(String, &'a OrchestratorConfig), String> {
    if let Some(room_name) = room_flag {
        let room = config
            .rooms
            .get(room_name)
            .ok_or_else(|| format!("unknown room '{room_name}'"))?;
        let orch_name =
            resolve_orch_name_from_room(config, room_name, room.orchestrator.as_deref())?;
        let orch = config
            .orchestrators
            .get(&orch_name)
            .ok_or_else(|| format!("orchestrator '{}' not found", orch_name))?;
        return Ok((orch_name, orch));
    }

    // Collect all remote orchestrators, sorted by name for determinism.
    let mut remotes: Vec<_> = config
        .orchestrators
        .iter()
        .filter(|(_, o)| o.mode.as_ref() == Some(&OrchestratorMode::Remote))
        .collect();
    remotes.sort_by_key(|(name, _)| (*name).clone());

    match remotes.len() {
        0 => Err(
            "no remote orchestrator found in config; specify --room or add a remote orchestrator"
                .into(),
        ),
        1 => {
            let (name, orch) = remotes.into_iter().next().unwrap();
            Ok((name.clone(), orch))
        }
        _ => {
            let names: Vec<&str> = remotes.iter().map(|(n, _)| n.as_str()).collect();
            Err(format!(
                "multiple remote orchestrators found ({}); use --room to disambiguate",
                names.join(", ")
            ))
        }
    }
}

/// Resolve orchestrator name from room config or fallback to single orchestrator.
fn resolve_orch_name_from_room(
    config: &WorkspaceConfig,
    room_name: &str,
    orch_ref: Option<&str>,
) -> Result<String, String> {
    match orch_ref {
        Some(name) => Ok(name.to_string()),
        None => {
            if config.orchestrators.len() == 1 {
                Ok(config.orchestrators.keys().next().unwrap().clone())
            } else {
                Err(format!(
                    "room '{}' does not pin an orchestrator and {} are defined; use orchestrator field in room config",
                    room_name,
                    config.orchestrators.len()
                ))
            }
        }
    }
}

/// Resolve an orchestrator field (address/token), expanding env vars.
fn resolve_orch_field(
    field: &Option<String>,
    field_name: &str,
    orch_name: &str,
) -> Result<String, ExitCode> {
    match field {
        Some(val) => {
            let resolved = resolve_env_token(field_name, val);
            if resolved.trim().is_empty() {
                eprintln!(
                    "error: orchestrator '{}' has empty {} after env expansion of '{}'",
                    orch_name, field_name, val
                );
                Err(ExitCode::FAILURE)
            } else {
                Ok(resolved)
            }
        }
        None => {
            eprintln!(
                "error: orchestrator '{}' is missing {}",
                orch_name, field_name
            );
            Err(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::workspace::{
        OrchestratorConfig, OrchestratorMode, PolicyConfig, RoleConfig, RoomConfig, WorkspaceConfig,
    };

    #[test]
    fn thread_subject_takes_first_line_truncated() {
        assert_eq!(thread_subject("hello world"), "hello world");
        assert_eq!(thread_subject("  spaced  \nsecond"), "spaced");
        assert_eq!(thread_subject(""), "thread");
        assert_eq!(thread_subject("\n\n"), "thread");
        let long = "x".repeat(80);
        assert_eq!(thread_subject(&long).chars().count(), 50);
    }

    fn discovered(policy_id: &str, name: &str) -> DiscoveredPolicy {
        DiscoveredPolicy {
            policy_id: policy_id.to_string(),
            name: name.to_string(),
            tags: vec![],
        }
    }

    #[test]
    fn label_resolves_to_policy_id_by_name() {
        let policies = vec![
            discovered("aaaa", "nsed:mid:0v1"),
            discovered("bbbb", "noosphera:0v1"),
        ];
        assert_eq!(
            find_policy_id_by_label(&policies, "noosphera:0v1").as_deref(),
            Some("bbbb")
        );
    }

    #[test]
    fn label_resolves_by_exact_policy_id() {
        let policies = vec![discovered("bbbb", "noosphera:0v1")];
        assert_eq!(
            find_policy_id_by_label(&policies, "bbbb").as_deref(),
            Some("bbbb")
        );
    }

    #[test]
    fn unknown_label_is_none() {
        let policies = vec![discovered("bbbb", "noosphera:0v1")];
        assert!(find_policy_id_by_label(&policies, "does-not-exist").is_none());
    }
    use std::collections::HashMap;

    // --- task-file / output-dir helpers ----------------------------------

    #[test]
    fn task_input_positional_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let out = resolve_task_input(Some("hello"), None, dir.path()).unwrap();
        assert_eq!(out, "hello");
    }

    #[test]
    fn task_input_file_resolves_relative_to_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("prompt.txt");
        std::fs::write(&p, "review this patch\n\n").unwrap();
        let out =
            resolve_task_input(None, Some(std::path::Path::new("prompt.txt")), dir.path()).unwrap();
        assert_eq!(out, "review this patch");
    }

    #[test]
    fn task_input_file_absolute_path_works() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("prompt.txt");
        std::fs::write(&p, "abs prompt").unwrap();
        let out = resolve_task_input(None, Some(&p), std::path::Path::new("/")).unwrap();
        assert_eq!(out, "abs prompt");
    }

    #[test]
    fn task_input_rejects_positional_and_file_together() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_task_input(
            Some("inline"),
            Some(std::path::Path::new("p.txt")),
            dir.path(),
        )
        .unwrap_err();
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn task_input_rejects_neither() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_task_input(None, None, dir.path()).unwrap_err();
        assert!(err.contains("required"));
    }

    #[test]
    fn task_input_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.txt");
        std::fs::write(&p, "   \n  \n").unwrap();
        let err = resolve_task_input(None, Some(std::path::Path::new("empty.txt")), dir.path())
            .unwrap_err();
        assert!(err.contains("empty"), "got {err:?}");
    }

    #[test]
    fn task_input_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_task_input(
            None,
            Some(std::path::Path::new("does-not-exist.txt")),
            dir.path(),
        )
        .unwrap_err();
        assert!(err.contains("failed to read"), "got {err:?}");
    }

    #[test]
    fn prepare_output_dir_creates_missing_dir() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("new-run");
        prepare_output_dir(&target, false).unwrap();
        assert!(target.is_dir());
    }

    #[test]
    fn prepare_output_dir_refuses_existing_verdict_without_force() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("verdict.md"), "old verdict").unwrap();
        let err = prepare_output_dir(dir.path(), false).unwrap_err();
        assert!(err.contains("--force-output"), "got {err:?}");
    }

    #[test]
    fn prepare_output_dir_force_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("verdict.md"), "old verdict").unwrap();
        prepare_output_dir(dir.path(), true).unwrap();
    }

    #[test]
    fn write_run_artifacts_writes_three_files() {
        let dir = tempfile::tempdir().unwrap();
        let dispatch = serde_json::json!({"job_id": "abc", "policy_id": "p"});
        write_run_artifacts(dir.path(), "the prompt", "the verdict", &dispatch).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("verdict.md")).unwrap(),
            "the verdict"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("prompt.txt")).unwrap(),
            "the prompt"
        );
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("dispatch.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(json["job_id"], "abc");
    }

    fn remote_orch() -> OrchestratorConfig {
        OrchestratorConfig {
            mode: Some(OrchestratorMode::Remote),
            address: Some("https://api.example.com".into()),
            token: Some("test-token".into()),
            nats_url: None,
            config_file: None,
        }
    }

    fn static_policy() -> PolicyConfig {
        PolicyConfig {
            agents: Some(vec!["agent-a".into(), "agent-b".into()]),
            roles: None,
            max_rounds: 3,
            effort: 0.85,
            sla: None,
            capabilities: None,
            tags: None,
            mode: Default::default(),
        }
    }

    fn role_policy() -> PolicyConfig {
        PolicyConfig {
            agents: None,
            roles: Some(vec![
                RoleConfig {
                    role: "analyst".into(),
                    count: 1,
                    capabilities: vec!["*".into()],
                    context: None,
                    pinned_agents: None,
                    moderator: false,
                },
                RoleConfig {
                    role: "reviewer".into(),
                    count: 1,
                    capabilities: vec!["*".into()],
                    context: None,
                    pinned_agents: None,
                    moderator: false,
                },
            ]),
            max_rounds: 3,
            effort: 0.85,
            sla: None,
            capabilities: None,
            tags: None,
            mode: Default::default(),
        }
    }

    fn config_with(
        policies: HashMap<String, PolicyConfig>,
        default_room_policy: &str,
    ) -> WorkspaceConfig {
        let mut orchestrators = HashMap::new();
        orchestrators.insert("remote".into(), remote_orch());

        let mut rooms = HashMap::new();
        rooms.insert(
            "main".into(),
            RoomConfig {
                policy: default_room_policy.into(),
                orchestrator: Some("remote".into()),
            },
        );

        WorkspaceConfig {
            policies,
            orchestrators,
            rooms,
            shared: None,
            default_room: Some("main".into()),
            agents: None,
        }
    }

    // ── resolve_room_run ──

    #[test]
    fn room_run_static_policy_has_no_role_policy() {
        let mut policies = HashMap::new();
        policies.insert("default".into(), static_policy());
        let config = config_with(policies, "default");

        let resolved = resolve_room_run(&config, None, "hello").unwrap();
        assert!(
            resolved.role_policy.is_none(),
            "static policy should not need push"
        );
        assert!(resolved.req.agent_names.is_some());
        assert!(resolved.req.policy_id.is_none());
    }

    #[test]
    fn room_run_role_policy_sets_role_policy_for_push() {
        let mut policies = HashMap::new();
        policies.insert("default".into(), role_policy());
        let config = config_with(policies, "default");

        let resolved = resolve_room_run(&config, None, "hello").unwrap();
        assert!(
            resolved.role_policy.is_some(),
            "role-based policy should be set for push"
        );
        let (name, _policy) = resolved.role_policy.unwrap();
        assert_eq!(name, "default");
        assert!(resolved.req.policy_id.is_some());
        assert!(resolved.req.agent_names.is_none());
    }

    // ── resolve_adhoc_run ──

    #[test]
    fn adhoc_run_named_static_policy_no_push() {
        let mut policies = HashMap::new();
        policies.insert("my-static".into(), static_policy());
        let config = config_with(policies, "my-static");

        let resolved = resolve_adhoc_run(&config, "my-static", "task", None).unwrap();
        assert!(resolved.role_policy.is_none());
        assert!(resolved.req.agent_names.is_some());
    }

    #[test]
    fn adhoc_run_named_role_policy_needs_push() {
        let mut policies = HashMap::new();
        policies.insert("my-roles".into(), role_policy());
        let config = config_with(policies, "my-roles");

        let resolved = resolve_adhoc_run(&config, "my-roles", "task", None).unwrap();
        assert!(resolved.role_policy.is_some());
        let (name, _) = resolved.role_policy.unwrap();
        assert_eq!(name, "my-roles");
    }

    #[test]
    fn adhoc_run_raw_hash_no_push() {
        let config = config_with(HashMap::new(), "none");
        let hash = "a".repeat(64);

        let resolved = resolve_adhoc_run(&config, &hash, "task", None).unwrap();
        assert!(
            resolved.role_policy.is_none(),
            "raw hash should not trigger push"
        );
        assert_eq!(resolved.req.policy_id.as_deref(), Some(hash.as_str()));
    }

    #[test]
    fn adhoc_run_unknown_policy_errors() {
        let config = config_with(HashMap::new(), "none");
        let err = resolve_adhoc_run(&config, "nonexistent", "task", None).unwrap_err();
        assert!(err.contains("unknown policy"));
    }

    // ── is_hex_hash ──

    #[test]
    fn hex_hash_valid() {
        assert!(is_hex_hash(&"a".repeat(64)));
        assert!(is_hex_hash(&"0123456789abcdef".repeat(4)));
    }

    #[test]
    fn hex_hash_wrong_length() {
        assert!(!is_hex_hash(&"a".repeat(63)));
        assert!(!is_hex_hash(&"a".repeat(65)));
    }

    #[test]
    fn hex_hash_non_hex_chars() {
        let mut s = "a".repeat(63);
        s.push('g');
        assert!(!is_hex_hash(&s));
    }

    // ── resolve_orch_field ──

    #[test]
    fn resolve_field_none_is_error() {
        assert!(resolve_orch_field(&None, "address", "orch1").is_err());
    }

    #[test]
    fn resolve_field_literal_value() {
        let result = resolve_orch_field(&Some("http://localhost".into()), "address", "orch1");
        assert_eq!(result.unwrap(), "http://localhost");
    }
}
