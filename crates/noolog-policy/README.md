# noolog-policy

Deliberation policies as clients write them, and their content address.

| Item | What |
|---|---|
| `PolicyConfig` | roster (`agents`) or `roles`, `max_rounds`, `effort`, `sla`, `variables`, `capabilities`, `tags`, `mode`; `policy_id()` is the SHA-256 of the canonical JSON the orchestrator hashes |
| `RoleConfig` | a role's `count`, `capabilities`, `pinned_agents`, `moderator`, and client-only `context` files (not part of the address) |
| `PolicySla` | `job_timeout_secs` (`0` = no budget, read through `job_timeout()`), `response_sla_secs`, `max_tokens` |
| `PolicyMode` | `deliberation` (default), `passthrough`, `moderator` |

A leaf crate: `quorum-rs` re-exports these at `cli::workspace` and
`scheduling`, and `noolog-files` uses them for the job an upload asks for.
