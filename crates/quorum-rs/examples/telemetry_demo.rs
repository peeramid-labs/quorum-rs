//! `telemetry_demo` — hands-on inspection harness for the agent-only
//! telemetry foundation shipped in PR1 of #309.
//!
//! Run:
//!
//! ```bash
//! cargo run -p quorum-rs --example telemetry_demo
//! ```
//!
//! What it demonstrates:
//!
//! 1. `derive_trace_id` is deterministic across processes — same inputs
//!    produce the same hex, and a boundary-shift on `(job_id, agent_id)`
//!    does not collide under the length-prefixed encoding.
//! 2. Subject derivation produces the documented
//!    `telemetry.agent.<id>.<event>` shape, and rejects invalid
//!    `agent_id` tokens up front.
//! 3. The redaction invariant is structurally enforced: the JSON
//!    encoding of every variant contains none of the forbidden substrings
//!    (`prompt`, `content`, `thought_process`, …). If a future variant
//!    ever adds a content field, this example panics.
//! 4. `TelemetryConfig` parses the real YAML block from the agent config
//!    and honours `enabled: false`.

use quorum_rs::DeliberationPhase;
use quorum_rs::config::AgentFleetConfig;
use quorum_rs::telemetry::{
    AgentEventCommon, ContextEmergencyShrink, FinishReason, LlmErrorClass, LlmRequestComplete,
    LlmRequestFailed, LlmRequestStalled, LlmRequestStart, NatsConnectionState,
    NatsConnectionStateChanged, PromptExposureDetected, RecentToolOutput, RetryLoopAttempt,
    RetryReason, TaskAccepted, TaskCompleted, TaskFailed, TaskFailureClass, TelemetryConfig,
    TelemetryEvent, TelemetrySource, ToolCallExecuted, derive_trace_id,
};

fn main() {
    section("1. trace_id determinism + collision resistance");
    demo_trace_id();

    section("2. Subject derivation + agent_id validation");
    demo_subjects();

    section("3. Event catalog — one JSON line per variant (pipe to jq)");
    demo_catalog();

    section("4. Redaction invariant");
    demo_redaction();

    section("5. Config parse — sample fleet + opt-out overlay");
    demo_config();

    println!();
    println!("OK — all invariants demonstrated.");
    println!("Hints:");
    println!("  • Pipe JSON lines to jq:   cargo run --example telemetry_demo 2>/dev/null | \\");
    println!("      awk '/^\\{{/' | jq '.type'");
}

fn section(title: &str) {
    println!();
    println!("=== {title} ===");
}

fn demo_trace_id() {
    let base = derive_trace_id("job-abc", 3, DeliberationPhase::Proposing, "CortexB");
    let again = derive_trace_id("job-abc", 3, DeliberationPhase::Proposing, "CortexB");
    assert_eq!(base, again, "determinism broken");
    println!(
        "(job-abc, round 3, propose, CortexB) -> {base}   [reproduced: {}]",
        base == again
    );

    let a = derive_trace_id("ab", 1, DeliberationPhase::Proposing, "c");
    let b = derive_trace_id("a", 1, DeliberationPhase::Proposing, "bc");
    assert_ne!(a, b, "delimiter collision!");
    println!("collision attack (ab,c) vs (a,bc): distinct (a={a}, b={b})");
}

fn demo_subjects() {
    let agent = TelemetrySource::agent("CortexB").expect("valid agent_id");
    let evt_agent = TelemetryEvent::TaskAccepted(TaskAccepted {
        common: sample_agent_common("CortexB"),
        dispatch_delay_ms: 42,
        task_publish_ts: None,
        job_age_at_accept_ms: None,
    });
    println!(
        "agent    -> {}",
        agent.subject(evt_agent.kind(), None).expect("valid")
    );

    println!("agent_id validation:");
    for id in [
        "CortexB",
        "agent-1_v2",
        "evil.injection",
        "has*wildcard",
        "has>wildcard",
        "has whitespace",
        "",
    ] {
        match TelemetrySource::agent(id) {
            Ok(_) => println!("  {id:?}  -> ACCEPT"),
            Err(e) => println!("  {id:?}  -> REJECT ({e})"),
        }
    }
}

fn demo_catalog() {
    let agent_src = TelemetrySource::agent("CortexB").unwrap();
    for evt in sample_events() {
        let subject = agent_src.subject(evt.kind(), None).unwrap();
        let json = serde_json::to_string(&evt).unwrap();
        println!("# subject: {subject}");
        println!("{json}");
    }
}

fn demo_redaction() {
    let all_json: String = sample_events()
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect::<Vec<_>>()
        .join(" ");
    for field in [
        "prompt",
        "prompt_text",
        "content",
        "thought_process",
        "proposal_body",
        "reasoning_text",
        "api_key",
        "justification",
        "message",
        "persona",
    ] {
        let pattern = format!("\"{field}\":");
        let leaked = all_json.contains(&pattern);
        assert!(
            !leaked,
            "REDACTION LEAK: field {field:?} appeared in catalog JSON"
        );
        println!("  {field:<18} NOT PRESENT");
    }
}

fn demo_config() {
    // Minimal fleet config: two agents, telemetry on by default.
    // Demonstrates the AgentFleetConfig + TelemetryConfig parse contract.
    let sample_yml = r#"
telemetry:
  enabled: true
agents:
  - name: proposer-a
    provider_id: openai
    model_name: gpt-4o-mini
  - name: evaluator-b
    provider_id: openai
    model_name: gpt-4o-mini
"#;
    let fleet: AgentFleetConfig =
        serde_yaml::from_str(sample_yml).expect("sample fleet config must parse");
    println!(
        "sample fleet config      -> telemetry.enabled = {}  ({} agents)",
        fleet.telemetry.enabled,
        fleet.agents.len()
    );

    let opt_out: TelemetryConfig = serde_yaml::from_str("enabled: false\n").unwrap();
    println!(
        "'enabled: false' overlay -> telemetry.enabled = {}",
        opt_out.enabled
    );

    let missing: TelemetryConfig = serde_yaml::from_str("{}").unwrap();
    println!(
        "missing block (default)  -> telemetry.enabled = {}",
        missing.enabled
    );
}

// ---------------------------------------------------------------------------
// Sample builders
// ---------------------------------------------------------------------------

fn sample_agent_common(agent_id: &str) -> AgentEventCommon {
    AgentEventCommon {
        agent_id: agent_id.to_string(),
        job_id: Some("job-abc".into()),
        round: Some(3),
        phase: Some(DeliberationPhase::Proposing),
        ts: 1_776_790_692_747,
        trace_id: derive_trace_id("job-abc", 3, DeliberationPhase::Proposing, agent_id),
    }
}

fn sample_events() -> Vec<TelemetryEvent> {
    let agent = sample_agent_common("CortexB");
    vec![
        TelemetryEvent::LlmRequestStart(LlmRequestStart {
            common: agent.clone(),
            request_id: "req-1".into(),
            model: "google/gemma-4-26b-a4b-it".into(),
            provider_id: "openrouter".into(),
            attempt: 1,
            estimated_input_tokens: 1_200,
            context_utilization_pct: 92.0,
            recent_tool_output_bytes: 21_540,
        }),
        TelemetryEvent::LlmRequestComplete(LlmRequestComplete {
            // Demo flips this event into the evaluate phase. trace_id
            // is derived from `(job_id, round, phase, agent_id)`, so
            // overriding `phase` without recomputing would emit an
            // event whose phase doesn't match its trace — the catalog's
            // correlation invariant. Recompute via the same helper the
            // SDK uses at runtime.
            common: AgentEventCommon {
                phase: Some(DeliberationPhase::Evaluating),
                trace_id: derive_trace_id(
                    "job-abc",
                    3,
                    DeliberationPhase::Evaluating,
                    &agent.agent_id,
                ),
                ..agent.clone()
            },
            request_id: "req-1".into(),
            latency_ms: 4_200,
            ttft_ms: Some(180),
            generation_ms: Some(4_020),
            input_tokens: 1_200,
            output_tokens: 350,
            reasoning_tokens: 120,
            cached_tokens: 0,
            cost_usd: 0.0041,
            reported_cost_usd: Some(0.0039),
            cache_write_tokens: None,
            finish_reason: FinishReason::Stop,
            provider_backend: Some("openrouter/deepinfra".into()),
            claim_assessments_emitted: Some(12),
            disagreements_emitted: Some(2),
            messages_chars: 4_800,
            max_tokens_requested: Some(2_000),
            response_chars: 1_400,
            tool_calls_emitted: 0,
            max_tokens_shrunk_to_floor: false,
            available_space_at_dispatch: Some(800),
        }),
        TelemetryEvent::LlmRequestFailed(LlmRequestFailed {
            common: agent.clone(),
            request_id: "req-2".into(),
            error_class: LlmErrorClass::RateLimit,
            http_status: Some(429),
            retry_after_ms: Some(2_000),
            latency_ms: 180,
            provider_id: "openrouter".into(),
            provider_backend: Some("openrouter/parasail".into()),
        }),
        TelemetryEvent::LlmRequestStalled(LlmRequestStalled {
            common: agent.clone(),
            request_id: "req-3".into(),
            elapsed_ms: 30_000,
            ttft_received: false,
            last_token_ms: None,
        }),
        TelemetryEvent::ToolCallExecuted(ToolCallExecuted {
            common: agent.clone(),
            tool_name: "search_deliberation".into(),
            latency_ms: 420,
            success: true,
            output_bytes: 18_240,
            output_tokens_estimated: Some(4_560),
            truncated: false,
            paginated: true,
        }),
        TelemetryEvent::RetryLoopAttempt(RetryLoopAttempt {
            common: agent.clone(),
            attempt: 3,
            reason: RetryReason::SchemaError,
            cumulative_latency_ms: 18_400,
            cumulative_cost_usd: 0.0127,
            cumulative_input_tokens: 3_200,
            cumulative_output_tokens: 900,
        }),
        TelemetryEvent::TaskAccepted(TaskAccepted {
            common: agent.clone(),
            dispatch_delay_ms: 40,
            task_publish_ts: Some(1_776_790_692_700),
            job_age_at_accept_ms: Some(40),
        }),
        TelemetryEvent::TaskCompleted(TaskCompleted {
            common: agent.clone(),
            duration_ms: 12_000,
            dispatch_delay_ms: 40,
            queue_wait_ms: Some(5),
            phase_budget_remaining_ms: 3_000,
            llm_attempts: Some(2),
            tool_call_count: Some(1),
            pending_publish_depth: Some(0),
        }),
        TelemetryEvent::TaskFailed(TaskFailed {
            common: agent.clone(),
            duration_ms: 120_000,
            dispatch_delay_ms: 40,
            queue_wait_ms: Some(5),
            phase_budget_remaining_ms: 0,
            llm_attempts: Some(5),
            tool_call_count: Some(3),
            failure_class: TaskFailureClass::Timeout,
            pending_publish_depth: Some(0),
            reason: None,
        }),
        // Connection-state events are process-level — no task scope —
        // so the envelope's task fields are `None`. The `trace_id`
        // still has the catalog's `TRACE_ID_LEN` lowercase-hex shape;
        // it just hashes a `(agent_id, uuid)` seed instead of the
        // task tuple via `TelemetryContext::new(...)` which is what
        // the worker uses at runtime.
        TelemetryEvent::NatsConnectionStateChanged(NatsConnectionStateChanged {
            common: quorum_rs::telemetry::TelemetryContext::new(&agent.agent_id, None, None, None)
                .common(),
            state: NatsConnectionState::Reconnecting,
            reconnects_so_far: 2,
            pending_publish_depth: Some(7),
            buffer_bytes: Some(4_096),
        }),
        TelemetryEvent::PromptExposureDetected(PromptExposureDetected {
            common: agent.clone(),
            terminal_tool: "submit_proposal".into(),
            blocked: true,
            hit_count: 3,
            response_length_chars: 1_482,
            suspicion_score: 4.76,
            xml_tag_hits: 2,
            tool_name_hits: 1,
            instruction_hits: 0,
            wrong_acronym_hits: 0,
            sample_hits: vec![
                "xml-tag <working_memory>".into(),
                "xml-tag <key_findings>".into(),
                "tool-name submit_proposal".into(),
            ],
        }),
        TelemetryEvent::ContextEmergencyShrink(ContextEmergencyShrink {
            common: agent.clone(),
            available_space: 199,
            requested_max: 4_000,
            floor_used: 200,
            estimated_input: 130_700,
            context_window: 131_072,
            recent_tool_outputs: vec![
                RecentToolOutput {
                    tool: "read_file".into(),
                    bytes: 244_000,
                },
                RecentToolOutput {
                    tool: "grep_search".into(),
                    bytes: 18_400,
                },
            ],
        }),
    ]
}
