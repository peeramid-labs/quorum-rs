# Epic-read bridge protocol

Wire protocol for reading a patch-deliberation epic's git state over NATS. A client with
no filesystem and no forgejo access sends a request; a fleet node holding the epic replies
from its local clone. For the design rationale see
[About the epic-read bridge](../explanation/epic-read-bridge.md).

Implemented in [`crate::epic_read`](../../crates/quorum-rs/src/epic_read.rs).

## Transport

Request/reply over NATS. One request yields one reply.

| | |
|---|---|
| Subject | `<prefix>.epic.<project_id>.read` |
| Pattern | Request/reply (`nats.request`) |
| Server subscription | Queue group on `<prefix>.epic.*.read` — exactly one holder answers |
| Payload | JSON `ReadRequest` (request) / JSON `ReadReply` (reply) |

`<prefix>` is the deployment's `api_prefix` (default `sphera`). `<project_id>` is the
epic's canonical root-commit sha — the same key `ProjectRegistry` groups agents on. A node
subscribes the queue group only for the projects it holds; a request for an unheld project
is answered with an `error` reply, never served from another epic.

## Request

JSON object, internally tagged by `op` (snake_case).

| `op` | Fields | Returns |
|---|---|---|
| `files_list` | `path` (string, default `""` = root), `at` (string, optional) | `paths` |
| `file_read` | `path` (string, required), `at` (string, optional) | `content` |
| `refs_list` | — | `refs` |
| `diff` | `base` (string, required), `target` (string, required) | `content` |

`at` pins a read to a commit/ref (point-in-time "what did job X produce"); omitted or
`null` reads current HEAD. `base`/`target` are refs — a `job/<job>/<agent>` branch against
its `base/<job>` tag yields the proposal's diff.

### Request examples

```json
{"op": "files_list", "path": "docs"}
{"op": "file_read", "path": "docs/spec.md", "at": "head/job1"}
{"op": "refs_list"}
{"op": "diff", "base": "base/job1", "target": "job/job1/AgentA"}
```

## Reply

JSON object. Exactly one of `paths` / `content` / `refs` carries a success payload, or
`error` carries a structured failure. Empty/absent fields are omitted.

| Field | Type | Set for |
|---|---|---|
| `paths` | array of string | `files_list` — tree entry names under `path` |
| `content` | string | `file_read` (file text) / `diff` (unified diff) |
| `refs` | array of string | `refs_list` — `job/*` branches + `base/*` / `head/*` tags |
| `error` | string | any failure or refusal |

A reply always arrives — a bad subject, unparseable request, unheld project, unsafe path,
git failure, or a reply too large for NATS all come back as `{"error": "..."}`, never a
silent timeout. A reply that would exceed the server's `max_payload` (the largest NATS
message, default 1 MB) can't be published, so it is replaced with a size error naming the
byte count — narrow the request (a subpath or a single file) and retry.

### Reply examples

```json
{"paths": ["README.md", "docs"]}
{"content": "spec v2 — AgentA"}
{"refs": ["base/job1", "head/job1", "job/job1/AgentA"]}
{"error": "this node does not hold project \"abc123\" — read refused (out of scope)"}
```

`content` is trimmed of trailing whitespace (the git invocation trims its stdout), so a
`file_read` reply is not a byte-exact copy of the file — a trailing newline is dropped.
Treat it as text for display/diffing, not for hash-reconstructing the blob.

## Guarantees

- **Read-only.** No op writes; proposals flow through the deliberation pipeline, not here.
- **Epic-confined.** `path` is rejected before touching git if it is absolute, starts with
  `/`, contains a `..` component, or contains a backslash. `git show <ref>:<path>` is
  itself repo-relative. A ref (`at` / `base` / `target`) that starts with `-` is refused
  before git runs — git would otherwise parse it as an option (e.g. `git diff --output=<file>`
  writes outside the epic). Git env (`GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE`) is
  scrubbed per call.
- **Project-scoped.** A node answers only for epics in its held map; the subject's
  `project_id` is the authorization key.
- **Fresh.** Reads reflect the epic's current git state; a landed deliberation commit shows
  on the next read with no extra fetch.

## Error strings

`error` is human-readable, not a stable enum. Current forms:

| Condition | Substring |
|---|---|
| Unheld project | `out of scope` |
| Absolute path | `is absolute — reads are confined to the epic tree` |
| `..` escape | `escapes the epic tree` |
| Flag-like ref | `parsed as a git option` |
| Malformed subject | `malformed read subject` |
| Bad request JSON | `bad read request` |
| Reply too large | `exceeds the <n>-byte NATS payload limit` |
| Git failure | the underlying `git <args>: <stderr>` |
