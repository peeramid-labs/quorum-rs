# Glossary

Key terms used across the NSED documentation and CLI output.

---

**Agent**
A process that participates in deliberation. Each agent has a unique name, a backing language model (LLM), and a set of capability tags. Agents propose answers and evaluate each other's proposals. The reference implementation is built on `nsed-agent-sdk`.

**Budget**
The combined resource envelope for a deliberation: wall-clock time (Phase 1), and optionally token counts and cost (Phase 2). The budget manager allocates per-round and per-phase timeouts adaptively based on observed agent latencies.

**Capability**
A tag that declares what an agent is good at (e.g., `lang:rust`, `security:owasp`, `*` for general purpose). Policies use capability matching to select which agents participate in a deliberation.

**Convergence**
The condition under which the deliberation halts early — when thermodynamic evidence `E(r)` reaches the threshold `ε × B`. The `effort` parameter (0–1) controls how much accumulated evidence is required. See [convergence-protocol.md](convergence-protocol.md).

**Decisiveness**
`Σ|net_support[p]|` — the total absolute signed QV opinion mass across all proposals in a round. Used internally in the convergence pipeline; the primary halting signal is now **winner conviction** (see below). See [scoring-variables.md](scoring-variables.md).

**Effort**
The user-facing halting sensitivity dial (`effort`, range [0, 1], default 0.6). Fraction of the thermodynamic positive budget `B` that must accumulate as evidence before the deliberation may halt. Lower effort halts sooner; higher effort requires stronger sustained consensus. Replaces `convergence_threshold` in policy configuration (`convergence_threshold` remains a backward-compatible alias).

**Deliberation**
A single end-to-end reasoning session. One deliberation produces one final answer through N rounds of propose + evaluate cycles. Identified by a `job_id` / `room_id`.

**Domain A / Domain B**
The two logical planes in the split-node architecture: **Domain A (AI Cortex)** runs the orchestrator worker and LLM inference, while **Domain B (Control Plane)** runs the API gateway, job manager, and NATS server. See [architecture.md](architecture.md).

**Evaluation**
The structured critique an agent writes about another agent's proposal. Contains a **signed** score in [-1, +1] (negative = explicit opposition, 0 = neutral, positive = endorsement) and a rationale. All evaluations are cross-evaluations — no agent scores its own proposal.

**Fan-out / Fan-in**
The orchestrator pattern that dispatches tasks to all agents concurrently (fan-out) and waits for responses before aggregating (fan-in). Used for both propose and evaluate phases.

**HITL (Human-in-the-Loop)**
The operator review mechanism. When `response_sla_secs > 0` in a policy, each agent's LLM response is held in a buffer before being forwarded to the orchestrator. An operator can inspect, edit, or reject the response within the review window. See [how-to/hitl.md](how-to/hitl.md).

**Job**
The unit of work dispatched via the NATS job queue. A job carries the deliberation parameters (task, agents/policy, scope, budget). Each job has a unique `job_id`. After creation, a job is processed by an orchestrator worker. The terms "job" and "session" are often used interchangeably.

**Job Manager / Broker**
The Domain B component that receives API requests, resolves agents from policies, serialises the `JobPayload`, and publishes to the NATS stream. The Dynamic Expertise Broker selects agents using a knapsack algorithm.

**NATS / JetStream**
The messaging backbone of NSED. NATS JetStream provides durable streams (job queue), key-value buckets (history, scratchpads, status), and publish-subscribe (SSE events). No external SQL database is used. See [NATS.md](NATS.md).

**nsed**
Acronym: **N-Way Self-Evaluating Deliberation**. A Runtime Mixture-of-Models protocol where N agents propose answers and cross-evaluate every peer in turn, converging via a Macro-Scale RNN consensus loop. See the [whitepaper](https://arxiv.org/abs/2601.16863) for the full protocol specification, Dynamic Expertise Broker, and empirical results. Hallucinated alternative expansions (e.g. "Neural Swarm") are not correct.

**Net Support**
The per-proposal signed aggregate of evaluations. Positive net support means evaluators collectively favoured this proposal; negative means they rejected it. Used to rank proposals within a round. See [scoring-variables.md](scoring-variables.md).

**Orchestrator**
The server component (running as `nsed-orchestrator` or embedded via `nsed serve`) that manages the NATS infrastructure, exposes the REST API and dashboard, dispatches jobs, and runs deliberation workers. One orchestrator can serve many simultaneous deliberations.

**Phase**
One half of a deliberation round. The **propose phase** is when agents generate their answers; the **evaluate phase** is when agents score each other's answers. Each phase has an independent timeout derived from the budget manager.

**Policy**
A named, content-addressable configuration that defines the rules for a deliberation: which agents (or capability-matched roles) participate, how many rounds to run, `effort` (halting sensitivity), `min_rounds`, SLA timers, and capability requirements. Policies are registered on the orchestrator and referenced by rooms.

**Proposal**
The structured answer produced by one agent in the propose phase. Contains the agent's text response, a round number, and metadata. After all proposals are collected, they are shared with all agents for the evaluate phase.

**QV (Quadratic Voting)**
The scoring mechanism used to aggregate evaluations. Raw evaluation scores are transformed so that extreme scores require more "votes" to express, reducing the impact of outlier evaluators. See [scoring-variables.md](scoring-variables.md).

**Room**
A named workspace endpoint combining a policy and an orchestrator. When a client runs `nsed run`, it targets a room. The room determines which policy governs the deliberation and which orchestrator processes it. Configured in `nsed.yaml`.

**Round**
One full iteration of the propose + evaluate cycle. A deliberation runs 1 to N rounds (determined by the policy). If convergence is detected, the round loop terminates early.

**Scope**
The overall time envelope for a job: `quick` (2 min), `standard` (10 min), `thorough` (30 min), `unlimited` (24 h), or `custom`. The budget manager derives per-round timeouts from the scope.

**SLA (Service Level Agreement)**
In NSED, SLA refers to the timing commitments for a deliberation job. `response_sla_secs` is the HITL review window (how long a response is held before auto-release). `job_timeout_secs` is the total wall-clock budget for the entire deliberation job — the BudgetManager divides this adaptively across rounds and phases.

**Session**
Synonymous with Job in most contexts. The session ID (`room_id`) identifies the NATS KV buckets (`nsed_hist_{id}`) and subjects (`nsed.{id}.*`) for a specific deliberation.

**Winner Conviction**
The primary halting signal: `w = ns[winner] / Σ|ns_p|` ∈ [-1, +1]. The winning proposal's signed net support divided by the total absolute net support mass across all proposals. +1 = unanimous endorsement, 0 = split verdict, -1 = unanimous rejection of the winner. Each round's contribution to the evidence accumulator is `Δe(r) = max(w, 0) × u'(r)`. See [convergence-protocol.md](convergence-protocol.md).

**Workspace**
The local directory containing `nsed.yaml`. The CLI uses the workspace config to know which orchestrators, agents, and rooms to work with.
