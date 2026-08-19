# Changelog

All notable changes to this project are documented in this file. The
format is loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Per-crate releases use the `<crate>-vX.Y.Z` tag scheme; workspace-wide
releases use the bare `vX.Y.Z` shape. Section headers below mirror that:
`## [<crate>-vX.Y.Z]` for per-crate, `## [X.Y.Z]` for workspace.

The release-prepare workflow prepends new sections automatically via
`git-cliff`. Edit `cliff.toml` to tune the generated content.

<!-- new sections inserted above this line by release-prepare -->



## [quorum-rs-v0.8.0] - 2026-08-17

### 🚀 Features

- *(openrouter)* Per-agent web-search plugin config
- *(llms)* Preserve HTTP status on non-400 4xx via LlmError::Api
- *(heartbeat)* Add model_down flag so a dead remote model benches the agent
- *(worker)* Report model_down on a 404 task failure, self-heal after cooldown
- *(agent-health)* Escalate model-down bench backoff, reset on success
- *(agents)* Resolve claim citations to exact proposal spans, error/retry on miss
- *(agents)* Ground claim cites in the exec-agent path too (non-destructive)
- *(dashboard)* Add agent diagnostics API (metrics + latest errors)
- *(dashboard)* Surface paused/flagged state in agent diagnostics
- *(providers)* Model-availability probe over a provider model catalog
- *(agent-health)* Proactive model-availability self-bench via provider catalog
- *(dashboard)* Guard the agent control plane with a bearer token
- *(agents)* Self-reported agent health (healthy/degraded/down) on heartbeat
- *(dashboard)* Fleet-wide API errors per agent (last 24h) view
- *(dashboard)* NATS-backed agent event log powering 24h errors, tasks, and tool-calls views
- *(agents)* One grounding seam for claim citations, with spans clients can highlight by

### 🐛 Bug Fixes

- *(remote)* SSE auth via Authorization header, not ?token= query param
- *(crypto-core)* Compile on wasm32-unknown-unknown
- *(tui)* Clamp thread/nested scroll at bottom — no wrap to top
- *(prompt-exposure)* Redact leaks post-recovery instead of retry-looping to a hard fail
- *(prompt-exposure)* Re-scan after redact; block+retry if leak persists (H5)
- *(claude-recovery)* Skip unreadable-dir sweep test when running as root
- *(cite)* Stop whitespace-tolerant resolve panicking on a multibyte tail
- *(cite)* Ground evaluation claims against the shown thought_process window
- *(context-tools)* Resolve current-round anonymized candidates in read_proposal
- *(context-tools)* Resolve current-round anonymized candidates in search_deliberation
- *(mcp-tools)* Resolve current-round anonymized candidates in nsed_read_proposal/nsed_search
- *(llm-repair)* Stop silently mis-repairing truncated numbers + cap python literal depth
- *(llm-repair)* Anchor markdown eval columns + don't misclassify a proposal as a read
- *(llm-repair)* Parity-check the incomplete-unicode trim so an escaped backslash survives
- *(xml-regex)* Bound the tool-regex cache to stop unbounded growth
- *(scoring)* Guard non-finite endorsement weights so a NaN can't mis-decide the winner
- *(middleware)* Fail closed when a middleware guard can't be built
- *(dashboard)* Stream job feed over fetch under auth + same-origin token guard
- *(dashboard)* Fail closed — refuse a non-loopback bind with no token
- *(agents)* Spell out café so the spell checker stops reading it as a typo
- *(agents)* Say why a structured-output parse found nothing to parse
- *(dashboard)* Stop a finished task reading as a failure, and reporting another phase's result
- *(agents)* Give the tool-call bucket name a single owner
- *(agents)* Do not ask a question there is no time to answer
- *(agents)* Offer only the tools that have something to serve, and say why one was refused
- *(agents)* Report a tool call that was abandoned instead of leaving it pending
- *(agents)* Treat a retired model as down, not as a transient failure
- *(deps)* Vendor the Swagger UI assets so docs.rs can build

### 💼 Other

- *(claims)* Require verbatim claim quotes so the UI highlighter can locate them
- *(claims)* Force verbatim claim quotes — no paraphrase (client substring-match)

### 🚜 Refactor

- *(agent-health)* Abstract model-down detection behind a trait
- *(llm-repair)* Dedup read_proposal/read_critiques positional arg parsing

### 📚 Documentation

- *(portal)* Add frontmatter, short titles, mermaid diagrams for the /docs portal
- *(explanation)* Add claim-citation grounding rationale
- *(openapi)* Register orchestrator-budgets endpoint in agent dashboard spec
- *(reference)* Add the openrouter agent-config block reference
- *(how-to)* Add self-serve device registration guide
- *(portal)* Frontmatter + short titles for device + provider-config guides
- Tighten docs + make README an index (no PR/issue refs, less verbosity)
- Link the hosted docs portal and MCP endpoint from the README
- Drop the framework-name line from the docs index
- Fix stale propose/evaluate log sample in first-agent tutorial
- Repair cross-references that only resolved inside the superproject
- Why a threaded conversation loses its live question
- A tutorial for threading a conversation
- Thread on the endpoint that can pick its council
- Show the server-rendered form of a threaded turn
- What the thread key is, and the ways a thread loses continuity
- Say what a plain-LLM council actually needs from a thread

### 🧪 Testing

- *(tui)* Fix stale selection assertion after the Down-clamp change
- *(llms)* Lock chat_completion 4xx→LlmError classification end-to-end
- *(exec)* Cover claim-cite grounding through evaluate()
- *(cite)* Lock dash-label, list-bullet, and smart-single-quote forms
- *(context)* Cover render_proposal offset-overflow and windowing
- *(context)* Cover keyword filter on the candidate search branch
- *(llm-repair)* Cover bare and max-hex incomplete unicode truncation
- *(llm-repair)* Cover the python-AST depth-cap stack-overflow guard
- *(nsed-agent)* Cover the no-op-redact fail-closed re-scan gate
- *(dashboard)* HTTP-level coverage for the agent diagnostics endpoint
- *(openapi)* Guard annotated-but-unregistered endpoints in agent dashboard
- *(agent-manager)* Cover persist_agents_to_yaml config persistence
- *(crypto-core)* Cover from_seed malformed-seed rejection
- *(native)* Assert web-search search_prompt is emitted
- *(dashboard)* Guard the bearer-auth audit fixes against regression
- *(agents)* Stamp the phase on the completion this fixture pairs

### ⚙️ Miscellaneous Tasks

- Classify a feature release as a minor bump
## [quorum-crypto-core-v0.7.2] - 2026-07-30

### 🚀 Features

- *(serve)* Register role-based workspace policies at boot
- *(middleware)* Dylib entries accept `config`, injected into context metadata
- *(agents)* Add `middleware` field to AgentConfig (deserialize-only)
- *(worker)* Invoke before_prompt / on_provider_response / on_completion pipelines
- On_job_complete hook — fire once at job-final via orchestrator's job_complete event
- *(agents)* Middleware-declared proposal schema → forced structured tool call
- *(claude)* Forceful MissingTerminalCall retry feedback — directive 'MUST call the tool now, no prose' so the subprocess complies with terminal-tool submission
- *(mcp)* Nest the middleware-declared schema into nsed_propose's inputSchema
- *(mcp)* Log nsed_propose input_schema override (runtime proof the nested schema is advertised)
- *(sdk)* Branching thread model, session resume, delta-task efficiency
- *(tui)* Email-style branching thread TUI + ask_user HITL
- *(serve)* Inherit orchestrator address/token from ~/.nsed when config blank
- *(tui)* Auto-open deliberation detail on send, ^C stop, full-editor steering
- *(tui)* Resolve a thread's running job from the orchestrator, not local disk
- *(tui)* Enter opens the live detail while deliberating; louder "no deliberation"
- *(tui)* Multiline steering input + shared text-editing primitives
- *(workers)* Per-agent max_concurrent_jobs to serialize racing jobs
- *(workers)* ReAct retry on a provider_response reviewer block instead of failing the task
- *(observability)* Opt-in outgoing prompt/request dumps (NSED_DUMP_PROMPTS_DIR)
- *(project-registry)* Map project id -> agents holding the epic (pure core)
- *(project-registry)* ProjectAdvertisement::from_verdict — build advert from before_prompt content
- *(project-registry)* Publish project_advanced on job_complete (pull trigger)
- *(project-sync)* Client epic replica — clone once, git pull on project_advanced
- *(project-registry)* Worker advertises ProjectAdvertisement on before_prompt
- *(workers)* Thread conversation_id to patch-deliberation for stable-worktree keying
- *(tui)* App-level ask_user overlay — question surfaces + answerable on any screen
- *(tui)* Inbox 'needs answer' CTA + fix ask_user answer never sending on reopen
- *(mcp-tools)* Read-only file/line history tools + block raw git in deliberation
- *(crypto-core)* DeviceIdentity — NATS user-nkey wrapper for self-serve register (T015)

### 🐛 Bug Fixes

- *(mcp)* Unify proposal content-resolution across handler + recovery
- *(clippy)* Restore #[allow(too_many_arguments)] on generate_structured_output
- *(mcp)* Enforce the forced proposal schema on nsed_propose + retry on violation
- *(mcp)* Detect rate-limit when claude exits before reading the prompt
- *(tui)* Surface a failed deliberation instead of silently dropping the spinner
- *(tui)* Readable thread tree — front indent, timestamp column, visible cursor on entry
- *(tui)* Mark branch-offs as their own thread + drop the redundant re: tag
- *(mcp)* Expose user tools (ask_user) to the claude agent — HITL clarify was dead
- *(mcp)* Make ask_user unambiguous + integration-test the claude tool list
- *(tui)* Resolve a thread's running job from persisted pending_job
- *(tui)* Don't pin a stale "Fetching…" footer for background fetches
- *(tui)* ^C stops the deliberation even without a locally-known job
- *(tui)* Detect Ctrl+C under tmux (drop the Press-only gate) + show "Stopping…"
- *(tui)* Revert ^D-breaking is_ctrl change; add tmux-proof `x` stop alias
- *(tui)* Roll back the optimistic turn when the orchestrator rejects the submit
- *(agents)* Run claude subprocess with cwd = per-job worktree override
- *(agents)* Restore too_many_arguments allow on execute_phase_attempt
- *(claude)* Resume (keep cache) after a quota-kill instead of restarting fresh
- *(react-loop)* Validate the forced-schema envelope, not solution_content
- *(project-sync)* Clear inherited git env so ops target the replica, not GIT_DIR
- *(claude)* Mint a fresh uuid on post-collision restart, not the collided one
- *(claude)* Don't clamp rate-limit wait to the phase budget — wait for reset, then continue
- *(mcp)* Fresh-restart reuses the deterministic session uuid — stop the every-turn re-send
- *(serve)* Wire the user-tool handler factory in build_worker so ask_user is advertised

### 💼 Other

- *(quorum-rs)* Add fmt-check, clippy, audit make targets
- *(claude)* Surface session-recovery outcome + retry failure for the lock race

### 🚜 Refactor

- *(agents)* Rename content key pd_worktree → agent_working_dir
- *(workers)* Reuse the react loop's retry for reviewer blocks — drop the second budget

### 📚 Documentation

- On_provider_response idempotency note (reruns on retry) + debug log for dropped before_prompt transform
- Middleware-declared proposal schema (forced structured output) + on_job_complete hook
- Thread TUI — how-to, reference, explanation
- *(explanation)* Add "Understanding rooms and policies"
- *(explanation)* Add policy-as-model + session/thread explainer; forward-note rooms
- *(explanation)* Document the agent working-directory override

### ⚡ Performance

- *(prompts)* Static system message + per-turn header — restore prompt-cache reuse

### 🧪 Testing

- *(worker)* Hook-invocation integration tests via mock middleware
- *(user-tools)* Prove ask_user threads build_request -> AgentContext -> claude MCP surface
- *(ask_user)* Prove claude_cli gets ask_user as user_ask_user over MCP

### ⚙️ Miscellaneous Tasks

- Gitignore stray .git-worktrees/ dirs
- TODO comments for deferred gaps (disable_native_tools enforcement, rmcp version pin)
- *(supply-chain)* Fast-track crossbeam-epoch 0.9.20 (RUSTSEC-2026-0204)
- *(perf)* Disable debuginfo for external deps to cut build RAM

## [0.7.1] - 2026-07-03

### 🐛 Bug Fixes

- *(validate)* Accept unified quorum.yml in `quorum validate`

### 📚 Documentation

- *(readme)* Revamp — quickstart, badges, 0.7 versions, current doc links
- *(readme)* Punch up tone — hook line, mermaid deliberation diagram, drop diataxis
- *(readme)* Simplify the verdict node in the deliberation diagram
- *(readme)* Drop hard-coded crate versions (table column + install pins + status)

## [quorum-rs-v0.7.0] - 2026-06-28

### 🚀 Features

- *(smoke)* Show the real backend (provider/model/base_url/engine) being tested
- *(smoke)* NSED stage runs N deliberations × R rounds (propose+evaluate) with full per-round details
- *(telemetry)* DeliberationContextAssembled event — prior-context + scratchpad signals per propose/evaluate
- *(smoke)* Surface every failure with full breakdown, 400 reason, and progress bars

### 🐛 Bug Fixes

- *(release)* Bound changelog at latest stable tag, not latest rc

### 📚 Documentation

- *(telemetry)* Document deliberation_context_assembled event
- *(init)* Document provider engine field (vllm) in the fleet boilerplate
