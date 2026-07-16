# About the agent working-directory override

Why `AgentContext.working_dir_override` exists, how a `before_prompt` middleware
sets it, and what an agent implementation must do with it. Background for SDK users
who write middleware or custom agents.

## The problem

An agent subprocess (e.g. `ClaudeAgent` driving the Claude CLI) runs with a working
directory. By default that is the agent's static `working_dir` config, falling back
to wherever the worker process was launched. That is fine for a fixed sandbox — but
some middleware needs the agent to run in a **per-task** directory that only exists
at runtime.

The motivating case is patch-deliberation: its `before_prompt` middleware creates a
fresh git worktree per job (`<worktrees>/<job>/<agent>`), applies the agent's ops
there, and computes consensus from it. The path contains the job id, so it cannot be
a static config value. And crucially, tool-using agents run **bare `git log`,
`ls`, and relative reads** — against the process cwd, not the absolute paths in the
prompt. If cwd is the launch dir, the agent reads one tree while its recorded work
lands in another: a split brain.

## The mechanism

A single runtime-only field on `AgentContext`:

```rust
pub working_dir_override: Option<PathBuf>,
```

The flow, entirely through the existing middleware contract (no runtime reach-in):

```mermaid
flowchart LR
    MW[before_prompt middleware] -->|content.pd_worktree| WK[agent worker]
    WK -->|ctx.working_dir_override| AG[agent subprocess]
    AG -->|cwd = override| FS[per-task tree]
```

1. A `before_prompt` middleware returns a **`pd_worktree`** key on its content object
   (the same object that carries `task_description` and any `proposal_schema`). The
   stage verdict's `hook_state` is *not* surfaced to the deliberation worker, so the
   content object is the channel.
2. The worker copies `pd_worktree` into `context.working_dir_override` before
   dispatching the propose/evaluate call.
3. The agent runs with `cwd = working_dir_override ∨ static working_dir ∨ process cwd`.

For `ClaudeAgent` the override governs three things that must stay consistent: the
subprocess spawn cwd, the `--resume` session-jsonl recovery paths, and relative
context-file resolution. Get one wrong and, e.g., recovery scans a different tree
than the agent ran in.

## What this means for SDK users

- **Writing a `before_prompt` middleware** that stages files in a per-task dir:
  return `pd_worktree` (absolute path) on your content object, and the agent will run
  there. It is optional — omit it and agents use their static `working_dir`. It is a
  declaration, not a command: the worker chooses cwd from it.
- **Writing a custom agent** (`impl NsedAgent`) that shells out or reads files:
  honour `ctx.working_dir_override` first, then your config `working_dir`, then the
  process cwd. Use it for the subprocess cwd *and* any path resolution the subprocess
  won't do for you. The reference `ClaudeAgent::phase_working_dir` is the pattern to
  copy.
- **Everyone else:** nothing to do. The field is `None` in the absence of a middleware
  that sets it, and behaviour is unchanged.

## Why an override and not static config

Static `working_dir` is one directory per agent, forever. A long-lived worker serves
many jobs; each patch-deliberation job needs its own runtime-created, job-id-bearing
frozen tree. Pointing static config at a shared dir either collides across concurrent
jobs or diverges from where the middleware actually applies changes (the split brain
above). The override binds cwd to the *same* tree the middleware owns, chosen at job
time. The static field remains the correct default when no per-task override is
present — hence `override ∨ config ∨ cwd`, not override-only.

See also [the middleware system](middleware.md) for the `before_prompt` /
`provider_response` seams, and [agent internals](agent-internals.md) for the
`NsedAgent` contract.
