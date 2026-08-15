---
title: read_file sandbox
order: 9
tagline: Why HTTP-only agents get a sandboxed, read-only file tool scoped to operator roots.
---

# read_file sandbox

Why agents that lack a native filesystem affordance get a
configurable, sandboxed read path, and what the sandbox does and
does not guarantee.

## Why the tool exists

NSED supports several provider classes. Some — Claude CLI, MCP
subprocess, exec subprocess — bring their own filesystem affordances
via the surrounding runtime. Others — the openai-compatible
provider class — execute purely as HTTP chat-completion calls and
have no file access at runtime. For those agents the only grounding
available is whatever was inlined into the system prompt at startup.

That model breaks down whenever:

- The corpus an agent should reason over varies per job and is too
  large or too dynamic to inline up front.
- An aggregator or evaluator needs to verify a peer agent's
  citation against the actual source rather than trust the peer's
  transcription.
- The deliberation context references files (corpora, source trees,
  fixtures) the operator has already mounted into the agent's
  environment.

The sandboxed `read_file` tool gives those agents a read-only
filesystem affordance scoped to operator-configured roots — without
granting shell access, write capability, or directory traversal
beyond those roots.

## What the sandbox guarantees

The tool is constructed with one or more **canonical** root paths
from `agent_config.read_file_roots`. Every call goes through this
sequence:

1. The argument `path` is interpreted as either:
   - **Relative**: tried against each configured root in order; the
     first existing match wins. Operators that mount multiple
     parallel trees (e.g. corpus + source + fixtures) can rely on a
     single relative path resolving against whichever owns it.
   - **Absolute**: used directly.
2. The candidate path is **canonicalised** (`Path::canonicalize`),
   which resolves `..`, normalises `.`, and follows every symlink
   to its target.
3. The canonical target must `starts_with` at least one canonical
   root. Anything outside fails with `READ_FILE_OUT_OF_SANDBOX`.
4. The target must be a regular file. Directory targets and
   non-regular files (sockets, devices, FIFOs) are rejected.
5. Files larger than the configured cap (default 64 KiB, sized to
   fit inside a typical per-call tool-output budget) are truncated
   to the cap and returned with a `[truncated: N bytes total,
   returned first M]` marker. The agent gets a usable prefix
   instead of a hard rejection.

These are layered: a symlink that *looks* like it lives under the
root but resolves outside fails at step 3 because canonicalisation
followed the symlink before the prefix check ran. There is no
"follow once" exception.

## What the sandbox does not protect against

- **Race conditions on a parent directory.** Two layers narrow the
  TOCTTOU window:
  - The tool opens the canonical path **once** with `O_NOFOLLOW`
    (Unix) and derives metadata + bytes from that file handle, so
    a swap of the final component to a symlink between `resolve()`
    and `open()` fails the open instead of escaping.
  - A separate `metadata()` syscall before `open()` would have
    introduced an exploitable check-then-open window; metadata is
    instead read from the open file descriptor.

  An attacker with write access to a *parent directory* of the root
  could still swap an intermediate component between syscalls — the
  full mitigation for that is per-component `openat` + `O_NOFOLLOW`,
  which is a heavy lift for marginal value given the threat model
  assumes operator-owned roots.
- **Information leakage via timing.** Large files take longer to
  read. An LLM can in principle infer file size from latency. Do
  not place high-entropy secrets inside a configured root.
- **Indirect exfiltration via tool output.** A read result is fed
  back into the conversation. If the agent quotes it in downstream
  output that other parties see (proposals, evaluations, logs),
  the data has left the sandbox in the reasoning sense. Configure
  roots to contain only data you'd be comfortable seeing in any
  final agent output.

## Provider-class gating

The tool is only injected for openai-compatible (`openai`,
`openrouter`, …) provider classes. Agents whose config has an
`exec`, `mcp`, or `claude` provider section are skipped — those
runtimes already expose their own filesystem affordances and
mounting a second one would broaden capability beyond the scope of
this tool. Setting `read_file_roots` on a non-openai-class agent is
a no-op (no error, no tool injected).

## How to activate

Add `read_file_roots` to the agent's config. Empty (the default)
keeps the tool inactive — the agent's tool list does not include
`read_file` until at least one root is configured.

```yaml
agents:
  ResearchBot:
    provider_type: openai
    model: <provider>/<model>
    read_file_roots:
      - /workspace/corpus
      - /workspace/source
      - /workspace/fixtures
```

Each root is canonicalised at tool construction. Roots that fail to
canonicalise (missing path, EACCES, bind-mount not yet attached)
are dropped with a `tracing::warn` and the tool stays alive with
the remaining roots. A degenerate "all roots dropped" config makes
every read fail with `READ_FILE_OUT_OF_SANDBOX: no roots configured
for this agent` — the agent sees the tool but every call is denied.
That is intentional; surfacing the misconfiguration at the LLM
layer is faster than discovering it post-mortem.

## Audit log

Every call emits a single `tracing::info!` line at the agent
binary's log level:

```text
agent=ResearchBot tool=read_file path=docs/topic.md bytes=4216 truncated=false result=ok
agent=ResearchBot tool=read_file path=docs/big.md bytes=65536 truncated=true result=ok
agent=ResearchBot tool=read_file path=docs/somedir bytes=0 result=denied
agent=ResearchBot tool=read_file request="../../etc/passwd" bytes=0 result=denied
```

Two log-key conventions, depending on when the call failed:

- **`path=…`** — resolution succeeded. Used for both successful
  reads (including reads truncated under the size cap, which are
  still `result=ok` with `truncated=true`) and post-resolution
  denials such as a directory or non-regular-file target. The value
  is the canonical path **relative to the matched root**, so the
  host filesystem layout stays out of the audit log even when the
  agent submitted an absolute path.
- **`request="…"`** — resolution itself was rejected
  (`READ_FILE_OUT_OF_SANDBOX`: no roots, escape via `..`, absolute
  path outside any root, symlink resolves outside, missing file).
  The value is the agent-supplied argument verbatim so
  prompt-injected paths stay visible to operators.


## Configuration knob

| Field | Default | Notes |
|---|---|---|
| `read_file_roots: Vec<PathBuf>` | `[]` (tool inactive) | One or more directories to expose. `serde(skip_serializing)` on the wire, so paths never leave the host. |
| `DEFAULT_READ_FILE_MAX_BYTES` | `65_536` | Per-call read cap. Override per construction via `with_max_bytes(...)` if a calling crate needs a larger window. |

## Implementation surface

- `crates/quorum-rs/src/tools/scoped_read.rs` — the tool itself
  (`ScopedReadFileTool`), the canonical-prefix check, and the
  `O_NOFOLLOW` open path.
- `crates/quorum-rs/src/agents/config.rs` — the
  `read_file_roots: Vec<PathBuf>` field on `AgentConfig`.
- `is_openai_family_provider` and `aggregate_tools` in
  `crates/quorum-rs/src/agents/nsed_agent.rs` — conditional
  registration gated on the openai-class predicate plus a non-empty
  `read_file_roots`.

A CLI ergonomics flag for setting roots from the command line
(repeatable on `nsed run`) is a follow-up; today operators configure
roots directly in agent YAML.
