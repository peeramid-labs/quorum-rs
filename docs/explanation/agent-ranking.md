# Agent Ranking & Capabilities

Agents advertise capabilities and earn competitive rank through deliberation performance.

## Capability Declaration

Agents declare capabilities in their config YAML:

```yaml
agents:
  - name: "audit_expert"
    provider_id: "openai"
    model_name: "gpt-4o"
    description: "Financial audit specialist — compliance, risk, regulatory analysis"
    capability_tags: ["audit", "compliance", "legal", "financial"]
    signing_schemes: ["eip712"]    # placeholder for #115
    input_price_per_mtok: 2.5
    output_price_per_mtok: 10.0
```

These fields flow through the system:

```text
AgentConfig → AgentHeartbeat (every 10s) → AgentRegistration (orchestrator)
                                          → GET /agents REST API
                                          → Dashboard agent table
```

All fields are optional with `#[serde(default)]` — existing configs work without changes.

## Estimated Cost Per Round

Each agent's per-round cost is computed automatically from their per-token pricing:

```text
estimated_cost_per_round = (4000 × input_price + 1500 × output_price) / 1,000,000
```

Where 4000/1500 are the standard input/output token estimates per agent per round (same constants used in ledger cost estimation). This value is shown in the agent picker and dashboard.

## Competitive Ranking

Agents earn rank by outperforming same-rank peers in deliberations. The system uses **floor-based promotion**: only the lowest rank group in each deliberation has promotion at stake.

### Rules

1. All agents start at **rank 0**
2. After each deliberation completes, agents are grouped by current rank
3. Only the **floor rank group** (lowest rank tier) competes for promotion
4. The **highest scorer** in the floor group gets rank += 1
5. **Ties**: all tied agents promote (both proved they're above their tier)
6. **Single agent** in floor group: auto-promotes
7. Rank **never decreases** — it's a level, not a rating
8. Higher-ranked agents in the same deliberation are **not affected**

### Example

```text
Deliberation #1: agents A(rank 0), B(rank 0), C(rank 0)
  Scores: A=7.5, B=8.2, C=6.1
  Floor = rank 0 → B wins → B becomes rank 1

Deliberation #2: agents A(rank 0), B(rank 1), C(rank 0), D(rank 0)
  Scores: A=8.0, B=7.5, C=7.8, D=6.0
  Floor = rank 0 (A, C, D) → A wins → A becomes rank 1
  B (rank 1) not in floor → no change

Deliberation #3: agents A(rank 1), B(rank 1)
  Scores: A=9.0, B=8.5
  Floor = rank 1 (both) → A wins → A becomes rank 2
```

### What Rank Means

| Rank | Meaning |
|------|---------|
| 0 | Untested / new agent |
| 1-3 | Proven contributor |
| 5+ | Consistently top performer |
| 10+ | Elite — repeatedly outperforms peers at every level |

### Performance Stats

Each agent tracks cumulative performance:

```json
{
  "rank": 3,
  "rounds_completed": 45,
  "avg_score": 7.8,
  "deliberations_completed": 15,
  "wins": 3,
  "updated_at": 1774001607,
  "avg_latency_ms": 28500.0,
  "tasks_completed": 180,
  "tasks_failed": 2,
  "recent_latencies_ms": [25000, 30000, "..."]
}
```

- `avg_score`: running average across all deliberations (incremental mean)
- `wins`: number of floor-group promotions
- `avg_latency_ms`: rolling mean over the last ≤64 submissions (propose OR evaluate), in ms
- `tasks_completed` / `tasks_failed`: per-submission counters; `task_success_ratio` in the `/admin/api/agents/stats` response is derived from these
- `recent_latencies_ms`: the raw window used to compute `avg_latency_ms`, capped at 64 entries (sliding). Useful for p50/p90/max dashboard queries
- Stats are visible in `GET /agents` response, `GET /agents/directory`, `GET /admin/api/agents/stats`, and the dashboard

### Storage

Rankings are persisted in the NATS KV bucket `nsed_agent_ranks`:
- Key: agent_id
- Value: JSON `AgentPerformanceStats`
- Survives orchestrator restart

Rank is **server-side only** — stored in the orchestrator's NATS KV, not self-reported by agents in heartbeats. This prevents agents from self-promoting. The orchestrator is the sole authority on rank.

## Minimum Rank Filter

Deliberation requests can specify a minimum agent rank to exclude unproven agents:

```json
POST /deliberation
{
  "user_query": "...",
  "agent_names": ["agent-a", "agent-b", "agent-c"],
  "min_agent_rank": 3,
  "scope": "standard"
}
```

Agents below the threshold are excluded before dispatching. If fewer than 2 agents qualify, the request is rejected with 400 and a list of excluded agents.

This lets job posters require quality assurance: "only use agents that have demonstrated consistent performance."

## Configuration

```yaml
orchestrator:
  rank_bucket: "nsed_agent_ranks"    # NATS KV bucket name (default)
```

## Scoring Algorithm

After each deliberation, the orchestrator:

1. Aggregates each agent's average score across all rounds (from `ProposalRecord.aggregated_score`)
2. Loads current ranks from KV
3. Identifies the floor rank group
4. Promotes the winner(s)
5. Updates all agents' stats (rounds_completed, deliberations_completed, avg_score)
6. Saves to KV and publishes rank updates

The scoring runs as a post-completion side effect — it doesn't block job processing. Failures are logged but non-fatal.
