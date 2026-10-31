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




## [quorum-rs-v0.9.0] - 2026-10-31

### 🚀 Features

- *(llm)* Let a job choose the provider service tier
- *(llm)* Keep the cost the provider reported instead of guessing it
- *(sdk)* Pipe an implementation-decided SKIP upstream as an explicit task answer
- *(sdk)* From_wrapped_agent runs a strategy wrapper while the inner agent keeps config and chat
- *(agents)* Evaluators score a conciseness axis, answerable on both transports
- *(agents)* The conciseness axis is measured and addressable, not eyeballed
- *(cli)* The smoke test reports where time actually goes
- *(agents)* An agent reports where its traffic may go, and a proposal records where it went
- *(prompts)* A structured proposal is measured as structure, and its score ships with its size
- *(agents)* A provider-run tool is declared in the shape its backend accepts
- *(prompts)* A task states the date it was issued, so a model can tell recalled facts from current ones
- *(tools)* Search can run from a nested search-only call, for a backend that will not take it beside our own tools
- *(providers)* A model is asked to answer before it is believed, not merely looked up in a catalog
- *(agents)* An agent config carries its own key and the operator keys authorised for it
- *(hitl)* An operator annotation carries a signature chain, not a single signature
- *(hitl)* An operator's signatures reach the annotation, and every edit shape is checked
- *(agents)* An agent's heartbeat announces the key its signatures should be checked against
- *(project)* The advance notification says where the answer can be fetched, not only that it moved
- *(agents)* An agent is configured with a signing key, and derives the identity it announces
- *(agents)* A configured key signs, and the resolver hands back a signer rather than a seed
- *(agents)* A configured key records a signed trail beside each result, without touching the wire
- *(agents)* A reader verifies the audit trail and reports a tampered record as tampered
- *(crypto)* A seat can attest which commit is its candidate
- *(crypto)* Bind a candidate attestation to the proposal it was judged on
- *(crypto)* A skip is a claim about the empty artifact
- *(agents)* A job's audit trail can be read and tallied, not only written
- *(project)* The caller is told where its answer is, on the subject it already watches
- *(cli)* A finished run tells you where its answer lives, not only what it said
- *(crypto)* A seat publishes its candidate as a signed claim
- *(agents)* A proposal can say whether its content is the answer or an address
- *(nats)* Content-addressed object storage, so an answer can be named instead of carried
- *(agents)* A content address names an answer by its own hash
- *(agents)* A deliberation tells its seats how it carries answers
- *(telemetry)* A failed task says why
- *(agents)* A job carries generic variables the protocol never interprets
- *(agents)* Claim anchors survive tables, truncation, and dispute quotes
- *(content)* File uploads with public links
- *(hls)* Segment uploaded video so viewers can seek
- *(files)* Configurable upload spool directory, and a full one reported legibly
- *(files)* Parse the segmenting state in one place
- *(serve)* Set each agent's context window from the provider catalog
- *(sdk)* Read addressed content, encode KV keys and derive the fleet token from the SDK
- *(feed)* Noolog-feed joins the SDK, the feed's shared contract for node, orchestrator, web client and agents
- *(feed)* Noolog-feed renders records as mail and threads as mbox, moved from nsed-content so the node can serve them
- *(jmap)* Noolog-jmap carries the JMAP and JMAP Mail wire types, written from RFC 8620 and RFC 8621
- *(feed)* Noolog-feed maps records and channels onto JMAP Email and Mailbox, behind feature jmap
- *(files)* Noolog-files carries the files pipeline contract, so agents and the node stage uploads in one NATS inbox
- *(files)* Pipeline steps read staged bytes through a digest check and discard them when done
- *(files)* An upload carries a signed job request, the policy and query to run once the file is stored
- *(files)* Pipeline steps write derived objects through a Blob over the inbox, so segmenting runs as a step unchanged
- *(files)* The agent dashboard stages uploads in the files pipeline and asks the node for everything else
- *(plans)* Plan types move into the SDK so the node and the services reading its plans share one definition
- *(kms)* KMS client and signed-request wire in the SDK, shared by the node, the KMS and the orchestrator
- *(caller)* Signed caller assertion in the SDK; the node forwards who is calling, services verify it

### 🐛 Bug Fixes

- *(llm)* Never send response_format alongside native tools
- *(llm)* Carry the service tier on the request, not on the agent
- *(agents)* Conciseness is a category like the other five, not a special case
- *(llms)* Reach every endpoint, and send only parameters a backend accepts
- *(agents)* An MCP evaluator can read the line range the outline points at
- *(workers)* A skipped task reports that it finished
- *(workers)* An agent's failure reports what actually happened, not its category
- *(workers)* A failed task reports the provider's reason, and a credit exhaustion actually pauses the agent
- *(agents)* An evaluator that skips the conciseness axis no longer votes neutral on it
- *(agents)* A backend refusing a forced tool choice gets one retry without it, instead of losing the turn
- *(agents)* Recognise the refusal that describes the forced call instead of naming the parameter
- *(agents)* Judge a refused tool choice on the response body, which the terse error text withholds
- *(tests)* Stop shipping a developer's own filesystem layout in the published crate
- *(claude-recovery)* A dotted directory no longer breaks session recovery, forcing the full prompt every round
- *(agents)* Configuring a signing key no longer switches signing on, which would drop every proposal
- *(crypto)* The byte stability every signature depends on is declared, not inherited
- *(agents)* A parse failure names the model that produced it
- *(agents)* A seat broken by a mixed tool array says so instead of going quiet
- *(llm)* A provider tool call echoed as builtin_function no longer fails the completion
- *(crypto)* Publish a candidate claim where it is retained
- *(agents)* A proposal type must not move historical digests
- *(agents)* The addressed-proposal example uses a substrate-neutral scheme
- *(agents)* A configured delegated search tool actually reaches the agent
- *(agents)* A backend-run tool call is not refused by the active-set gate
- *(agents)* An echoed backend-run tool call is recorded as a success
- *(llms)* Replayed history re-marks a backend-run call as builtin_function
- *(agents)* A proposal whose answer hides in the thought process is recovered, not dropped
- *(tools)* The delegated search tool stops borrowing the provider's name
- *(files)* Close the upload findings from the pre-merge audit
- *(providers)* Model health reads the catalog only, never a billed completion
- *(upload)* Answer an oversize upload before it is sent
- *(files)* Deleting a video removes its segments too
- *(files)* The transcoder's working directory is configurable
- *(files)* An upload past the ceiling reads as too large, not malformed
- *(agents)* Declare each tool name once per request
- *(agents)* Refuse a config that declares one search tool twice
- *(tools)* Namespace the delegated-search tool so nothing else can claim its name
- *(tests)* Retry transient spawn failures in the ClaudeAgent chat tests
- *(deps)* Update rustls to 0.23.45 for GHSA-2mjx-qc3c-rqvc
- *(ci)* Survive a slow crates.io response in the dependency age check
- *(ci)* Fast-track rustls 0.23.45 and scope the dependency age check again
- *(providers)* Send the provider key with the model-catalog probe
- *(llms)* Retry under max_completion_tokens and surface upstream failures
- *(llms)* Name the fix when a model refuses function tools
- *(deps)* Rustls 0.23.46, patching RUSTSEC-2026-0346/0347/0348/0349, fast-tracked past the cooldown
- *(deps)* Cargo deny ignores RUSTSEC-2024-0449, unsafe-libyaml unmaintained via the deprecated serde_yaml
- *(feed)* Noolog-feed spells out its package fields, so nsed-orchestrator can use it by path
- *(deny)* The zip advisories are ignored, since zip only unpacks the vendored Swagger UI archive at build time
- *(files)* A Verified reader polled past its end keeps its verdict instead of hashing nothing

### 📚 Documentation

- *(tutorials)* Say that turns come back as turns
- *(sdk)* Skip.md gains the reference-section frontmatter
- *(crypto)* Document reading a whole job's audit trail
- *(crypto)* Name what the job trail subject covers
- *(agents)* Say how a seat broken by tool config or parse failure shows up
- *(nats)* The telemetry stream doc no longer points at a file that was never written
- *(cli)* Say why the TUI stream does not carry the consensus location
- *(nats)* Document the consensus event and why it is not on job_complete
- *(crypto)* Say orchestrator instead of an invented role
- *(crypto)* Clear the invented role from the newly merged claim code too
- *(crypto)* Keep the audit trail subject documented after the reverts
- *(agents)* An addressed proposal carries a scheme
- *(invites)* Mint unified codes with roles/preset, index every page
- *(sdk)* The content fetch protocol is a reference of its own; the codec and the fleet token move to theirs
- *(feed)* The feed contract has its reference in the SDK, next to the crate that implements it
- *(caller)* The caller assertion is internal; no public reference page

### ⚡ Performance

- *(agents)* A model already known to refuse a forced tool choice is not asked again

### 🚜 Refactor

- *(llm)* Let each provider say how it reports cost
- *(crypto)* Make the artifact binding generic
- *(agents)* The deliberation type is a job variable, not a protocol field
- *(files)* Group the upload subsystem under one module
- *(files)* One upload implementation for both surfaces
- *(files)* Publish the HLS annotation keys the serving side must read
- *(content)* The feed's types, rules and FeedStore move into noolog-feed, a crate with no NATS or SDK dependency
- *(jmap)* The JMAP types split into jmap-core and jmap-mail, as JMAP splits its capabilities, with no noolog-specific code
- *(files)* The files module becomes the noolog-files crate, so a store outside the SDK can implement Blob
- *(policy)* PolicyConfig, RoleConfig, PolicyMode and PolicySla move to the leaf crate noolog-policy
- *(sdk)* Policy, request and pipeline types live in quorum-rs outside the cli feature, noolog-policy dropped

### 🧪 Testing

- *(crypto)* Pin the wire contract between the dylib's candidate and the attestation
- *(crypto)* A score and a commit are comparable only about the same artifact
- *(crypto)* Pin the numeric range a signed payload can carry and still verify
- *(crypto)* A signature that cannot be decoded fails verification
- *(crypto)* Read the candidate claim where it is now published
- *(address)* Each refusal says which component it was
- *(hls)* Cover the real transcode, and stop broker tests skipping in CI
- *(files)* Cover deleting a video and uploading it again
- *(files)* Name the re-upload isolation test in words the spell check accepts
- *(agents)* Assert a propose never declares one function twice

### ⚙️ Miscellaneous Tasks

- Classify a feature release as a minor bump
- Make the ffmpeg step fit a self-hosted runner
- *(hooks)* A tracked pre-commit hook runs the format, license and advisory checks CI runs, before the commit is made

### ◀️ Revert

- *(project)* Remove the project-advanced route
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

### 🚜 Refactor

- *(agent-health)* Abstract model-down detection behind a trait
- *(llm-repair)* Dedup read_proposal/read_critiques positional arg parsing

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
- *(release)* V0.8.0

### 💼 Other

- *(claims)* Require verbatim claim quotes so the UI highlighter can locate them
- *(claims)* Force verbatim claim quotes — no paraphrase (client substring-match)
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

### 📚 Documentation

- On_provider_response idempotency note (reruns on retry) + debug log for dropped before_prompt transform
- Middleware-declared proposal schema (forced structured output) + on_job_complete hook
- Thread TUI — how-to, reference, explanation
- *(explanation)* Add "Understanding rooms and policies"
- *(explanation)* Add policy-as-model + session/thread explainer; forward-note rooms
- *(explanation)* Document the agent working-directory override

### ⚡ Performance

- *(prompts)* Static system message + per-turn header — restore prompt-cache reuse

### 🚜 Refactor

- *(agents)* Rename content key pd_worktree → agent_working_dir
- *(workers)* Reuse the react loop's retry for reviewer blocks — drop the second budget

### 🧪 Testing

- *(worker)* Hook-invocation integration tests via mock middleware
- *(user-tools)* Prove ask_user threads build_request -> AgentContext -> claude MCP surface
- *(ask_user)* Prove claude_cli gets ask_user as user_ask_user over MCP

### ⚙️ Miscellaneous Tasks

- Gitignore stray .git-worktrees/ dirs
- TODO comments for deferred gaps (disable_native_tools enforcement, rmcp version pin)
- *(supply-chain)* Fast-track crossbeam-epoch 0.9.20 (RUSTSEC-2026-0204)
- *(perf)* Disable debuginfo for external deps to cut build RAM

### 💼 Other

- *(quorum-rs)* Add fmt-check, clippy, audit make targets
- *(claude)* Surface session-recovery outcome + retry failure for the lock race

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
