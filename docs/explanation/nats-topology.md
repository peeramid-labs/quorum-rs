---
title: "Architecture & NATS"
order: 1
tagline: How NSED uses NATS JetStream for orchestration, task distribution, and state.
---

# Architecture & NATS

This document outlines the event-driven architecture used in the NSED system, focusing on how NATS JetStream is used for orchestration, task distribution, and state management.

## **1\. High-Level Topology**

The system is built around a "Hub-and-Spoke" model where **NATS JetStream** acts as the central nervous system. Components do not call each other directly; they communicate exclusively by publishing and subscribing to NATS subjects.

### **Key Subjects (Topics)**

| Subject Pattern | Type | Purpose | Publisher | Subscriber |
| :---- | :---- | :---- | :---- | :---- |
| `sphera.jobs.submit` | **Queue** | Entry point for new deliberation requests. | Broker (`broker.rs`) | Orchestrator Worker |
| `sphera.jobs.manifest.{job_id}` | **Command** | Job manifest broadcast with agent list. | Broker (`broker.rs`) | NSED Workers |
| `sphera.jobs.complete.{job_id}` | **Data** | Deliberation result published after completion. | Orchestrator Worker | (Stream-captured) |
| `sphera.jobs.ack.{job_id}.{agent_id}` | **Response** | Worker acknowledgment of manifest. | NSED Worker | Orchestrator |
| `nsed.{job_id}.task.{agent}.{action}` | **Command** | A specific task for an agent (e.g., "propose", "evaluate"). | Orchestrator | NSED Worker |
| `nsed.{job_id}.result.{round}.{agent}.{action}` | **Data** | The output from an agent (e.g., the proposal JSON). 6 segments. | NSED Worker | Orchestrator |
| `nsed.{job_id}.result.event.{type}` | **Data** | Lifecycle events for history replay and real-time SSE streaming. | Orchestrator | SSE Handler / History API |
| `telemetry.orch.{event_type}` | **Telemetry** | Orchestrator-side metrics-only events. Independent stream from the result/event tree above. | Orchestrator | Forwarder |
| `telemetry.agent.{agent_id}.{event_type}` | **Telemetry** | Per-agent metrics-only events. The `agent_id` subtree is JWT-bound — the telemetry `Agent { agent_id }` role grants `publish("telemetry.agent.{agent_id}.>")` only, so an agent cannot forge events under a peer's id. Roles are minted by `<orchestrator>::credentials::issue_telemetry_jwt`. | NSED Worker (per agent) | Forwarder |

**SSE Event Types**: `round_start`, `proposal_submitted`, `evaluation_submitted`, `round_complete`, `job_complete`, `agent_accepted`, `agent_working`, `agent_error`, `budget_update`, `budget_phase_complete`, `tool_call_pending`, `tool_call_responded`, `tool_call_expired`, `user_injection`.

**Telemetry Event Types** (see [the telemetry reference](../reference/telemetry.md) for the full contract): orch tree — `round_started`, `phase_complete`, `agent_responded`, `agent_timed_out`, `eval_injected_synthetic`, `convergence_sample`, `job_finalized`, `submission_received`, `phase_quorum_reached`, `phase_tail_closed`. Agent tree — `llm_request_start` / `_complete` / `_failed` / `_stalled`, `tool_call_executed`, `retry_loop_attempt`, `task_accepted` / `_completed` / `_failed`, `nats_connection_state`, `prompt_exposure_detected`. Telemetry events carry **no** prompt / proposal / `thought_process` / secret content; redaction is enforced at the type layer in `quorum-rs::telemetry`.

### **Subject Naming Convention**

The subject structure follows a base pattern with variations by message type:

**Task messages** (5 segments):
```text
{prefix}.{session}.task.{agent}.{action}
```

**Result messages** (6 segments — includes round number):
```text
{prefix}.{session}.result.{round}.{agent}.{action}
```

**Event messages** (5 segments):
```text
{prefix}.{session}.result.event.{type}
```

- **prefix**: Namespace identifier (e.g., `nsed` for production, `test_xxx` for tests)
- **session**: Job/session UUID for isolation
- **round**: Zero-indexed round number (only in result messages)
- **agent**: Target agent identifier
- **action**: Operation type (`propose`, `evaluate`, etc.)
- **type**: Event type (`round_start`, `job_complete`, etc.)

### **Input Validation (NATS Naming Rules)**

All user-supplied identifiers (`room_id`, `agent_names`, `scope_id`, `job_id`) are validated at the API handler boundary **before** they reach NATS. The `validate_nats_name()` function (in `the orchestrator`) enforces the NATS protocol naming rules:

| Character | Status | Reason |
| :-------- | :----- | :----- |
| `\0` (null) | **Forbidden** | NATS protocol terminator |
| ` ` (space) | **Forbidden** | NATS protocol delimiter |
| `.` (period) | **Forbidden** | NATS subject level separator |
| `*` (asterisk) | **Forbidden** | NATS single-level wildcard |
| `>` (greater-than) | **Forbidden** | NATS full wildcard |
| `/` (slash) | **Forbidden** | Breaks KV paths and many client tools |
| Control chars, tabs, newlines | **Forbidden** | Protocol safety |
| Letters, digits, `-`, `_` | **Allowed** | Recommended charset: `[a-zA-Z0-9_-]` |
| Other Unicode | **Allowed** | Valid per NATS spec but not recommended |

Requests with invalid names receive a **400 Bad Request** response with a descriptive error listing all forbidden characters found. A secondary `sanitize_subject_component()` function is used as defense-in-depth internally, replacing any non-alphanumeric/non-hyphen/non-underscore character with `_`.

### **Stream Configuration**

NATS JetStream streams are configured to capture specific subject patterns:

| Stream Name | Subjects | Purpose |
| :---- | :---- | :---- |
| `sphera_jobs` | `sphera.jobs.submit`, `sphera.jobs.complete.>`, `sphera.jobs.manifest.>`, `sphera.jobs.ack.>`, `nsed.*.task.>` | Global job queue for submission, manifests, ACKs, and task dispatch |
| `nsed_results_{job_id}` | `nsed.{job_id}.result.>` | Per-job stream for results and events |
| `NSED_TELEMETRY` | `telemetry.>` | Drain buffer for telemetry. Stream name matches the `nsed-telemetry-forwarder::config::DEFAULT_STREAM_NAME` constant. Configured separately on dedicated telemetry node(s) via NATS placement tags — MUST NOT share storage with `nsed_results_*` on orchestrator hosts (load isolation + tenancy). Defaults: file storage, 24h time cap / 2 GB size cap, `Discard: old`, drained by the `nsed-telemetry-forwarder` durable consumer. Stream + placement detail lives in `ops/nats/telemetry-placement.md`. |

**Important**: Two streams cannot have overlapping subject patterns. Tests use unique prefixed streams to avoid conflicts.

**Consumer Configuration**: The orchestrator uses a shared durable consumer named `sphera_orchestrator_group` for the production `sphera_jobs` stream (enabling load distribution across multiple orchestrators). The `ack_wait` is set to `job_timeout + 60s` to prevent NATS from redelivering long-running jobs.

## **2\. File-to-File Control Flow (The Handover)**

The execution flow moves between distinct Rust modules via NATS messages.

### **Phase 1: Job Submission**

1. **Entry**: crates/the orchestrator/src/handlers/deliberation.rs receives a HTTP POST request.
2. **Validation**: `validate_nats_name()` checks `room_id`, `agent_names`, and `scope_id` for NATS-incompatible characters. Returns 400 if any fail.
3. **Action**: Calls broker::add\_job\_to\_queue.
4. **NATS**: Publishes JSON payload to **sphera.jobs.submit** (with double-await for JetStream ack confirmation).
5. **Handover**: The API handler returns 202 Accepted immediately. The job is now "at rest" in the NATS queue.

### **Phase 2: Orchestration Pickup**

1. **Entry**: crates/the orchestrator/src/workers/orchestrator.rs (running in background).
2. **Trigger**: Consumes message from **sphera.jobs.submit** via shared pull consumer.
3. **Setup**:
   * Creates/Checks KV Buckets using `ensure_kv_bucket()` (idempotent: create, fallback to get).
   * Claims the job in nsed\_job\_ownership (Distributed Lock).
4. **Handover**: Instantiates the Orchestrator struct (crates/the orchestrator/src/orchestrator.rs) and calls run\_deliberation().

### **Phase 3: The Deliberation Loop (Round Execution)**

Inside crates/the orchestrator/src/orchestrator.rs:

1. **Stream Init**: Creates a transient JetStream stream nsed\_results\_{session\_id} to capture all results and events for this specific job.
2. **Task Dispatch**:
   * Iterates through required agents.
   * **NATS**: Publishes to **nsed.{id}.task.{agent\_id}.propose**.
3. **Wait**: The Orchestrator creates a "Pull Consumer" on the nsed\_results\_{id} stream and waits.

### **Phase 4: Agent Execution**

1. **Entry**: `crates/quorum-rs/src/workers/nsed_worker.rs` (standalone agent) or `crates/quorum-rs/src/workers/nsed_worker.rs` (embedded agent). Both use the same `NatsNsedWorker` implementation.
2. **Trigger**: Wildcard subscription matches **nsed.\*.task.{my\_id}.\***.
3. **Deduplication**: Each message is checked against a processed-messages KV store using the key `{stream}-{sequence}-{subject}` (the subject component prevents cross-session collisions when stream sequence numbers are reused).
4. **Logic**:
   * Deserializes context.
   * Calls agent.propose() or agent.evaluate() (LLM interaction).
   * Accesses `NatsScratchpadStore` (`crates/quorum-rs/src/workers/nsed_worker.rs`) for persistent memory.
5. **NATS**: Publishes output to **nsed.{id}.result.{my\_id}.propose**.

> **Note:** Agents can run as standalone processes (using `quorum-rs` crate directly) or embedded in the orchestrator. The NATS protocol is identical in both cases — the orchestrator doesn't know or care whether an agent runs in-process or on a remote GPU node.

### **Phase 5: Result Aggregation & Events**

1. **Re-Entry**: crates/the orchestrator/src/orchestrator.rs (which was waiting).
2. **Trigger**: The Pull Consumer sees the new message in the nsed\_results\_{id} stream.
3. **Processing**: Aggregates the proposal/evaluation.
4. **Persistence**: Writes the full round history to the KV Store (nsed\_hist\_{id}).
5. **Event Publishing**: Publishes lifecycle events to **nsed.{id}.result.event.{type}** (e.g., `round_start`, `proposal_submitted`, `evaluation_submitted`, `round_complete`, `job_complete`). The SSE handler subscribes to the same `nsed.{id}.result.event.>` pattern for real-time streaming.

## **3\. Persistence Architecture (Key-Value Stores)**

We use NATS JetStream Key-Value (KV) stores for state persistence. **No external database is required** - all state is managed through NATS KV buckets.

| Bucket Name | Naming Logic in Code | Purpose | Handled By |
| :---- | :---- | :---- | :---- |
| `nsed_job_status` | Static (configurable) | Global index of job states (Running, Completed, Failed). | workers/orchestrator.rs |
| `nsed_job_ownership` | Static (configurable) | Distributed lock to prevent double-processing. | workers/orchestrator.rs |
| `nsed_hist_{id}` | `nsed_hist_demo-room-1` | Stores round history (proposals/evaluations per round) and budget snapshots (`budget_{session_id}` key for resume support). | orchestrator.rs / nats.rs |
| `nsed_proc_{agent}` | `nsed_proc_Jaya_xxx` | Agent's process-local state during a job. | workers/nsed_worker.rs |
| `nsed_local_mem_{agent}` | `nsed_local_mem_Jaya_xxx` | Agent's private scratchpad/memory. | workers/nsed_worker.rs |
| `nsed_toolcalls_{id}` | `nsed_toolcalls_demo-room-1` | Stores pending/responded/expired user tool calls. Keys: `call_{uuid}`. Created at job start if user tools are defined; deleted at job completion. History: 5 revisions, TTL: 3 days. | agents/user_tools.rs, handlers/deliberation.rs |
| `nsed_inject_{id}` | `nsed_inject_demo-room-1` | Stores user injections (hot-wire messages) and tool change requests. Key: `injections` (append-only list with CAS updates). | handlers/deliberation.rs, orchestrator.rs |

### **Worker Configuration**

Workers are configured with a `WorkerConfig` struct:

```rust
pub struct WorkerConfig {
    pub nats_url: String,              // NATS server URL
    pub stream_name: String,           // JetStream stream to consume from
    pub consumer_name: String,         // Unique consumer identifier
    pub subject_prefix: String,        // Subject namespace (default: "nsed")
    pub scratchpad_retention_secs: u64 // TTL for scratchpad data (0 = no TTL)
}

// Builder pattern:
let config = WorkerConfig::new(nats_url, stream_name, consumer_name)
    .with_subject_prefix("nsed".to_string())
    .with_scratchpad_retention(86400 * 7);  // 7 days
```

The `subject_prefix` defaults to `"nsed"` and allows test isolation - tests use unique prefixes while production uses the default.

**Scratchpad Retention**: The `scratchpad_retention_secs` controls TTL for agent scratchpad data. Defaults to 7 days. Set to 0 to disable TTL. Configure globally via `orchestrator.scratchpad_retention_secs` in settings.

**Sanitization & Validation (Defense-in-Depth):**

The system uses a **two-layer** strategy for Job ID / Room ID safety:

1. **Validation (handler boundary)**: `validate_nats_name()` rejects identifiers containing NATS-forbidden characters (`\0`, space, `.`, `*`, `>`, `/`, control chars) with a 400 error. This catches bad input before it reaches NATS.
2. **Sanitization (internal)**: `sanitize_subject_component()` replaces any non-alphanumeric, non-hyphen, non-underscore character with `_`. This is a defense-in-depth backstop in case validation is bypassed.

* Allowed characters: Alphanumeric, `-` (dashes), `_` (underscores), and other Unicode.
* Reason for preserving dashes: Web URLs usually use dashes (`demo-room-1`), while NATS internal keys often prefer underscores. The system preserves dashes in bucket names so the API can find `nsed_hist_demo-room-1` easily.

## **4\. SSE Event Streaming**

The SSE (Server-Sent Events) handler in `crates/the orchestrator/src/handlers/stream.rs` subscribes to **Core NATS** (not JetStream) on the subject pattern:

```text
nsed.{job_id}.result.event.>
```

This catches all lifecycle events published by the Orchestrator via `publish_event()`. Events are forwarded to connected SSE clients in real-time. The event type is extracted from the subject suffix (e.g., `nsed.demo42.result.event.round_complete` -> event type `round_complete`).

Because the SSE handler subscribes to the `result.event` namespace (which is also captured by the per-job JetStream stream `nsed_results_{job_id}`), events are both durable (persisted to disk via JetStream) and real-time (delivered to SSE clients via Core NATS subscription).

## **5\. Test Isolation Strategy**

Integration tests use isolated NATS environments to prevent subject overlap conflicts:

```rust
// TestContext creates unique streams per test
let ctx = TestContext::new_with_orchestrator("my_test").await?;
// Creates stream: test_my_test_{uuid}
// With subjects: test_my_test_{uuid}.*.task.>, nsed.{job_id}.task.>, etc.
```

Key principles:
- Each test gets a unique stream name with UUID
- Tests that use the Orchestrator include `nsed.{job_id}.*` subjects
- Workers use `subject_prefix: "nsed"` to match the default prefix
- Orchestrator consumers use per-worker names in tests (`orch_group_{id}`) vs shared `sphera_orchestrator_group` in production
- Cleanup via `ctx.teardown()` removes test streams after completion
