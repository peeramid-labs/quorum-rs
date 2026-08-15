---
title: Dump prompts
order: 12
tagline: Capture each agent's exact model payload to hunt prompt bloat, with no config change or rebuild.
---
# Dump prompts

Capture the exact payload each agent sends to its model — to find prompt bloat,
redundant context, or oversized system prompts — without changing any provider
config or rebuilding.

## Enable it

Set `NSED_DUMP_PROMPTS_DIR` to a writable directory when you launch the agents
(or the orchestrator/`serve` process that spawns them):

```bash
NSED_DUMP_PROMPTS_DIR=/Users/tim/SpheRa/prompt-dumps \
  nsed serve --config quorum.yml
```

Unset (the default) → the feature is off, at zero cost. There is no YAML flag: the
dump is process-wide, so one env var covers every agent and every provider in the
run.

## What lands in the directory

One file per outgoing call, in the provider's **native** format:

| provider | file | contents |
|---|---|---|
| `claude` | `…-claude.txt` | the CLI argv, one flag+value per line (so `--append-system-prompt` payloads are readable), then `----- stdin prompt -----` and the piped prompt |
| `openai` / `ollama` / `openrouter` / … | `…-<engine>.json` | the final Chat Completions request JSON as sent (messages, tools, params) |

Filenames are `{seq}-{agent}[-r{round}][-{phase}]-{provider}.{ext}`, e.g.
`000042-EngineerBot-Fast-r2-propose-claude.txt`. `{seq}` is a per-process counter,
so files sort in call order.

## Analyze

The claude dump shows every `--append-system-prompt` block — the usual home of
prompt bloat. Rank system-prompt sizes:

```bash
cd /Users/tim/SpheRa/prompt-dumps
# biggest claude invocations (bytes)
ls -S *-claude.txt | head

# total chars per agent (rough token proxy: ~4 chars/token)
for f in *-claude.txt; do printf '%s\t%s\n' "$(wc -c <"$f")" "$f"; done | sort -rn | head
```

For the OpenAI-wire dumps, inspect the `messages` array (repeated context, stale
history, duplicated instructions):

```bash
jq '.messages | map({role, chars: (.content | length)})' 000007-agentA-openai.json
```

## Notes

- Writing is best-effort: a filesystem error is logged (`dump_prompts: write
  failed`) and swallowed — a dump never breaks a live agent.
- Dumps contain full prompts (personas, context files, task text). Treat the
  directory as sensitive; don't commit it.
- The dump is taken at the send site *after* all middleware/system-prompt assembly,
  so it is exactly what the model receives — the ground truth for efficiency work.
