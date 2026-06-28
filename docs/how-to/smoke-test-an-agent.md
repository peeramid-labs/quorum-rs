# Smoke-test an agent

`quorum smoke-test <agent_id>` verifies one of your own agents in three
escalating stages — cheapest signal first, full protocol last:

1. **chat** — calls the agent's model directly 10× with a trivial prompt.
2. **tool-calling** — 10× with a tool defined, expecting a tool call back.
3. **full NSED** — submits real single-agent deliberations through the
   orchestrator and checks the agent participated.

Each stage gates the next: if a stage fails every sample, the run stops (no
point testing tools when chat is down, or NSED when tool-calling is down).

> **It makes real LLM calls.** All three stages hit your provider (and, for
> NSED, the orchestrator) — real tokens + latency. The command warns and asks
> for confirmation before running (skip with `--yes`).

## Prerequisites

- The agent must be one of **your own** agents — declared in `quorum.yml`'s
  `agents:`. An id that isn't yours is refused (smoke never pulls in other
  operators' / remote agents).
- The agent must be **serving and online**: run `quorum serve` first. An offline
  target is rejected.

**Only the specified agent** runs the deliberation — no other agents are
involved.

## Usage

```bash
quorum smoke-test justindgx            # 3 runs (default)
quorum smoke-test justindgx --runs 5   # more runs
quorum smoke-test justindgx --yes      # skip the confirmation prompt (CI)
```

`--runs` (default 3) controls the NSED stage; the chat and tool-calling stages
are fixed at 10 samples each. The chat/tool stages call the model directly
(built from the agent's provider in `quorum.yml`); the NSED stage submits ad-hoc
single-agent deliberations the same way `quorum run` does — only your operator
token, no `manage_rooms`. A **subprocess** provider (`exec` / `claude` / `mcp`)
can't be called directly, so the chat/tool stages are skipped and only NSED runs.

## Output

```
⚠ smoke-test makes REAL LLM calls (chat + tools direct, then deliberations) …
  Continue? (y/N) › y
chat: 10/10 ok · avg 412ms · errors 0%
tools: 9/10 ok · avg 530ms · errors 10%
nsed 1/3 ✓ justindgx participated (score 0.82)
nsed 2/3 ✓ justindgx participated (score 0.79)
nsed 3/3 ✓ justindgx participated (score 0.80)
nsed justindgx: 3/3 participated (100%)
```

Each direct stage reports **success / avg latency / error rate**. Exit code is
`0` only when every stage that ran fully passed (chat all ok, tools all ok, NSED
all participated); non-zero otherwise (CI-friendly).

## Troubleshooting

| Symptom | Cause |
|---|---|
| `not one of your agents in quorum.yml` | You passed an id that isn't in your `agents:` (e.g. someone else's remote agent). Use one of your own. |
| `agent not online` | The agent isn't serving — run `quorum serve`. |
| `absent from trace` | The agent is online but the deliberation didn't record a contribution — check its operator grants/capabilities vs the policy (see [run an agent fleet](run-an-agent-fleet.md)). |
