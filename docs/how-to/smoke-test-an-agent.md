# Smoke-test an agent

`quorum smoke-test <agent_id>` verifies that one of your agents actually
participates in real deliberations on the remote orchestrator — the full NSED
protocol end to end (the agent's real LLM provider, chat **or** responses API,
proposal + evaluation rounds), not a synthetic ping.

> **It makes real LLM calls.** Each run submits a real deliberation that your
> agent answers — so it costs tokens and takes provider latency. The command
> warns and asks for confirmation before running (skip with `--yes`).

## Prerequisites

- The agent must be **serving and online**: run `quorum serve` first. Smoke-test
  builds a deliberation from the orchestrator's *online* agents; an offline
  target is rejected.
- At least **2** of your agents online (a deliberation needs ≥2 participants).
  Smoke-test fills the deliberation with the target plus your other online
  agents.

## Usage

```bash
quorum smoke-test justindgx            # 3 runs (default)
quorum smoke-test justindgx --runs 5   # more runs
quorum smoke-test justindgx --yes      # skip the confirmation prompt (CI)
```

## What it does

1. Resolves the remote orchestrator from your `quorum.yml` workspace.
2. Warns it makes real LLM calls; confirms (unless `--yes` or no TTY).
3. Reads the orchestrator's online agents, checks the target is online, and
   selects the target + enough other online agents for a deliberation.
4. Submits `--runs` deliberations of a small smoke task, streams each to
   completion, and inspects the trace (`/deliberation/{id}/details`) to confirm
   the **target** agent proposed or evaluated.
5. Prints a per-run line and a participation rate.

It uses an ad-hoc deliberation (the same path as `quorum run`) — nothing is
created or left on the orchestrator, so it needs only your operator token (no
admin / `manage_rooms`).

## Output

```
⚠ smoke-test runs REAL deliberations using your agents (LLM cost + latency).
  Continue? (y/N) › y
run 1/3 ✓ justindgx participated (score 0.82)
run 2/3 ✗ justindgx absent from trace
run 3/3 ✓ justindgx participated (score 0.79)
smoke justindgx: 2/3 participated (67%)
```

Exit code is `0` only when **all** runs pass; non-zero otherwise (CI-friendly).

## Troubleshooting

| Symptom | Cause |
|---|---|
| `agent not online` | The agent isn't serving — run `quorum serve`. |
| `needs ≥2 online agents` | Fewer than 2 of your agents are online. Start more. |
| target keeps showing `absent from trace` | The agent is online but not eligible/selected for the deliberation — check its operator grants vs the room/policy tags (see [run an agent fleet](run-an-agent-fleet.md)). |
