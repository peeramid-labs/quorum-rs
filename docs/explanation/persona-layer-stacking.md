---
title: Persona layer stacking
order: 7
tagline: Why personas stack in layers and why md paths resolve against process CWD.
---

# Persona layer stacking

> Design rationale for the layered-persona feature on `quorum.yml`.
> Covers why the layered shape exists, why files are read at
> parse time, and why paths resolve against process CWD instead
> of the yaml file's parent directory.

## Why layers at all

Before layers, `persona:` was `Option<String>`. Operators with
multi-agent fleets ran into a familiar problem: every agent
shares a 4–30 line block describing the deliberation style, the
output format, or the safety guardrails — and those blocks drift
between agents the moment one operator forgets to copy a change
across all of them.

The pre-layer workarounds were:

1. Manually re-paste the shared block on every change (works for
   2 agents, breaks for 12).
2. Collapse everything into one mega-string (loses readability;
   PR diffs become unreviewable).
3. Build a custom binary that loads the yaml, post-processes
   `persona`, and re-serialises (every consumer reinvents this).

Stacked layers move (3) into the SDK. The yaml stays readable
(one layer per concern), shared blocks live in shared files
(single source of truth), and the resolved persona is still
plain `Option<String>` so every downstream call site —
`MultiAgentRunner`, `MultiAgentStatusServer`, the chat
endpoint, the Claude `--append-system-prompt` plumbing — keeps
working unchanged.

## Why parse-time file reads, not lazy reads

The first design considered was deferring `md` file reads until
the agent first needed its persona — `AgentConfig` would hold
`Option<PersonaSpec>` and a resolver method would walk the
layers on demand. Two reasons that lost:

1. **Failure timing.** Lazy reads surface a missing file
   somewhere deep in the runtime — the agent is already
   connected to NATS, has advertised a half-baked tool list,
   and only fails when the first job arrives. Eager reads
   surface the failure at fleet boot, in the same log line as
   the agent name, before the orchestrator has heard of it.
2. **Type churn.** `AgentConfig` is consumed at 15+ sites
   today (the worker builder, the control plane, the
   per-agent dashboard, the chat handler, the test fixtures,
   the JSON Schema generator). Changing `persona` from
   `Option<String>` to `Option<PersonaSpec>` would mean
   touching every one of them. The custom deserializer keeps
   the public type stable and contains the new behaviour to
   one function.

The cost is that the deserializer does I/O. That's normally
considered bad form, but the I/O is bounded (one file read per
layer, no fan-out), happens once per fleet boot, and fits the
existing pattern of `quorum validate` / `quorum serve` already
doing file I/O against `quorum.yml` and `~/.nsed/agent.creds`.

## Why CWD-relative paths

The deserializer has no yaml-path context. By the time
`serde_yaml::from_str` calls the field deserializer, the source
path is gone — the parser only sees the yaml string and the
field's serde annotation.

Two ways to add yaml-path context:

- (a) Wrap the parse path: instead of calling
  `serde_yaml::from_str(yaml)` directly, walk the parsed
  `serde_yaml::Value`, locate every `persona:` field, resolve
  paths against the yaml's parent, then deserialise. Two-pass.
  Slow, duplicate logic, fragile when yaml shape changes.
- (b) Thread the yaml path through a thread-local or
  `Deserializer` newtype that the custom function reads via
  `deserializer.state`. Possible with `serde::de::DeserializeSeed`
  but pulls a lot of plumbing for one feature.

Both add code surface that needs to be carried forever. The CWD
approach is what every other CLI does (Docker bind-mounts,
`cargo build` paths, shell scripts): if the operator wants
paths to resolve from a specific directory, they `cd` there
first or use absolute paths. Documented in
[reference/persona-yaml-shapes.md].

Concretely:

- Operators running `quorum serve` from a project root (typical)
  use `./prompts/x.md` and the path works.
- Service units / Docker entrypoints set `WorkingDirectory=` /
  `WORKDIR` to the directory holding `quorum.yml` — paths still
  work.
- CI jobs that invoke `quorum serve` from a parent dir use
  absolute paths or a wrapper shell script that `cd`s first.

If a future feature really needs yaml-relative paths (e.g.
`include:` for sub-yaml composition), it'll be worth wiring
option (a) globally. For one field with one shape, CWD is
enough.

## What's deliberately out of scope

- **Template expansion inside md files.** Layers concatenate raw
  bytes. Adding `${ENV_VAR}` or `{{handlebars}}` opens a much
  larger surface (escaping, recursion, error reporting) and
  every variable expansion already has a place to live (env
  vars before invocation, or a future include mechanism).
- **Layer reordering / overrides via control plane.** The control
  plane patches `AgentConfig.persona` as a single string today
  — same shape, same wire format. Layered editing would need a
  whole new patch grammar, which is more product than this PR
  is trying to be.
- **Roundtrip preservation of the layered shape.** Serialising
  an `AgentConfig` back to yaml writes the resolved string. If
  operators want to dump-and-re-edit a layered persona, they
  do so against the source `quorum.yml`, not against a
  serialised snapshot of `AgentConfig`.

## See also

- [how-to/compose-persona-from-shared-files.md] — the operator-facing recipe.
- [reference/persona-yaml-shapes.md] — formal grammar + error modes.
