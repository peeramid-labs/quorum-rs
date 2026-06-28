# Smoke-test an agent

`quorum smoke-test <agent_id>` verifies one of your own agents in three
escalating stages — cheapest signal first, full protocol last — all **in-process**
(no orchestrator, no NATS, no second agent):

1. **chat** — calls the agent's model directly 10× with a trivial prompt.
2. **tool-calling** — 10× with a tool defined, expecting a tool call back.
3. **NSED** — builds the agent and runs full deliberations against it: each
   deliberation is several rounds, and every round runs BOTH NSED phases — the
   agent `propose()`s, then `evaluate()`s its own proposal. Each round's
   evaluation (score + critique) is threaded into the next round's proposing
   context, so the agent inspects its own past proposals and evals exactly as in
   real deliberation. This is the full NSED wrapper (ReAct loop + tool-calling)
   for a single agent — no orchestrator or peers needed.

Each stage gates the next: if a stage fails every sample, the run stops (no
point testing tools when chat is down, or NSED when tool-calling is down).

> **It makes real LLM calls.** All three stages hit your provider — real tokens
> + latency. The command warns and asks for confirmation before running (skip
> with `--yes`).

## Prerequisites

- The agent must be one of **your own** agents — declared in `quorum.yml`'s
  `agents:`. An id that isn't yours is refused (smoke never pulls in other
  operators' / remote agents).
- `quorum serve` does **not** need to be running — the agent is built and driven
  locally from your config.

**Only the specified agent** is exercised — no peers, no orchestrator.

## Usage

```bash
quorum smoke-test justindgx                       # NSED = 10 deliberations × 5 rounds (default)
quorum smoke-test justindgx --runs 3 --rounds 2   # fewer/cheaper deliberations
quorum smoke-test justindgx --yes                 # skip the confirmation prompt (CI)
```

`--runs` (default 10) sets the number of NSED deliberations; `--rounds` (default
5) sets the rounds per deliberation (each round = propose + evaluate). The chat
and tool-calling stages are fixed at 10 samples each. The chat/tool stages call
the model directly (built from the agent's provider in `quorum.yml`). A
**subprocess** provider (`exec` / `claude` / `mcp`) has no directly-callable
model, so the chat/tool stages are skipped — but the NSED stage still runs (those
agents implement `propose`).

## Output

```
⚠ smoke-test makes REAL LLM calls (chat, tool-calling, and NSED propose) …
  Continue? (y/N) › y
smoke `justindgx` → provider `vllm`, model `qwen2.5-72b` @ http://localhost:8000/v1
chat: 10/10 ok · avg 412ms · errors 0%
tools: 9/10 ok · avg 530ms · errors 10%
nsed: 10/10 ok · avg 4820ms · errors 0%
  full details (first deliberation, 5 rounds):
  round 1: proposal 240c · scratchpad none · prior: none (first round) · evaluated 1 candidate(s) → score 0.50
  round 2: proposal 310c · scratchpad written 412c · prior: proposal✓ score 0.50 critiques 1 · evaluated 1 candidate(s) → score 0.62
  …
```

Each stage reports **success / avg latency / error rate**, with `— last error: …`
appended on any failure (everything runs in this process, so there is no server
log to consult). The NSED stage also prints the **full details** of the first
successful deliberation, round by round: proposal size, whether the agent wrote
its scratchpad, what prior context (proposal / score / critiques) was fed into
that round, and the resulting evaluation score — so you can confirm the agent
actually exercised cross-round state. Exit code is `0` only when every stage that
ran fully passed; non-zero otherwise (CI-friendly).

## Troubleshooting

| Symptom | Cause |
|---|---|
| `not one of your agents in quorum.yml` | You passed an id that isn't in your `agents:` (e.g. someone else's remote agent). Use one of your own. |
| `chat: 0/10 … last error: Connection refused` | Provider unreachable / wrong `base_url`. |
| `chat: 0/10 … last error: 401` | Bad/missing API key for the provider. |
| `tools: … last error: model returned no tool call` | The model didn't emit a tool call — it may not support tool-calling, or needs the repair/engine flags (see [run an agent fleet](run-an-agent-fleet.md)). |
| `nsed: 0/N … last error: …` | The agent's `propose()` failed — the error is the agent/LLM error verbatim. |
