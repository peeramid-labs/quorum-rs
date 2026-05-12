# Sandboxed builtin tools — `nsed-agent`

Reference for the three sandboxed filesystem tools that non-Claude agents (provider class `openai`) get in their tool-call surface: `read_file`, `grep_search`, `pdf_query`.

All three share the same access policy. The *why* — what the sandbox does and does not guarantee — lives in [`scoped-read-file`](../explanation/scoped-read-file.md) (originally written for `read_file`, applies to the family).

## Shared access policy

| Rule | Applies to |
|---|---|
| Per-agent allow-list of roots (`roots: [...]`) canonicalized once at construction | `read_file`, `grep_search` |
| Bare-filename basenames against a configured `trees_root` | `pdf_query` |
| Absolute paths refused unless they canonicalize under an allowed root | All |
| `..`-traversal refused (canonicalize-then-prefix check, defends symlink-out) | All |
| NUL bytes in path / tree arguments refused | All |
| Wall-clock timeout (configurable, default 10s) terminates the subprocess | `grep_search`, `pdf_query` |
| Per-call stdout cap (`max_bytes`) — over-cap output marked `truncated: true` | `grep_search`, `pdf_query` |
| stderr capped at 64 KiB inside the spawned task | `grep_search`, `pdf_query` |
| Arguments passed as single argv elements — never shell-interpolated | All |
| One audit log line per call | All |

A bad root / script / tree directory fails loud at agent startup — the fleet never accepts traffic with a half-broken tool. See `serve.rs` for the validation loop.

## `read_file`

Read a single file under one of the allow-listed roots, optionally with pagination for long files.

### Tool args

| Arg | Type | Required | Description |
|---|---|---|---|
| `path` | string | yes | Filesystem path. Resolved under one of the allowed roots; absolute paths or `..`-escapes outside the allow-list are refused. |
| `offset` | integer | no | 0-based byte offset to start reading. Defaults to `0`. |
| `limit` | integer | no | Maximum bytes to return THIS call. Hard-capped at the tool's configured per-call ceiling. |

### Return shape

```json
{
  "path": "/canonical/resolved/path",
  "content": "...",
  "bytes_returned": 4096,
  "next_offset": 8192,
  "truncated": false
}
```

`truncated: true` when content was cut at `limit`. `next_offset` lets the caller paginate without re-reading from zero.

### Refusal modes

- `path not under any allowed root`
- `path not found`
- `READ_FILE_PROCESS_ERROR` — IO or permission failure surfacing the underlying error

## `grep_search`

Recursive `grep -rEn` confined to the allow-listed roots, with regex pattern, optional glob filter, and result pagination.

### Tool args

| Arg | Type | Required | Description |
|---|---|---|---|
| `pattern` | string | yes | Extended regex (POSIX ERE). Passed to `grep -E`. |
| `path` | string | no | Sub-path under one of the allowed roots. Omit to search every root. |
| `include` | string | no | Shell-glob filter passed to `grep --include`. Examples: `"*.c"`, `"adi_*.h"`. |
| `case_insensitive` | bool | no | When `true`, passes `-i`. Default `false`. |
| `offset` | integer | no | Match-index to start from (0-based). Use `next_offset` from prior responses. |
| `limit` | integer | no | Maximum match lines this call. Default 10. Hard-capped at `max_results` (the per-tool sandbox ceiling). |

### Return shape

```json
{
  "matches": "<path>:<line>:<content>\n...",
  "match_lines": 7,
  "next_offset": 7,
  "truncated": false
}
```

`matches` is grep's `path:lineno:content` lines joined by newlines. `match_lines` is the count returned this call. `truncated: true` indicates `max_bytes` capped the stdout — caller should paginate.

### Refusal modes

- Empty `roots:` config at construction
- `path not under any allowed root`
- `GREP_PROCESS_ERROR` — invalid regex (grep exit 2), timeout, or other process failure

### Quirks

- An earlier revision passed `grep -m <N>` to the subprocess; in recursive mode `-m` is per-FILE, so the cap leaked up to `N × files` matches. The current implementation enforces the cap after collecting stdout — `truncated: true` is the trustworthy signal.
- Catastrophic-backtracking regex patterns can stall grep; the wall-clock timeout is the hard bound.

## `pdf_query`

Wraps a Python `pdf_query.py` script that performs semantic lookup over pre-extracted PDF trees. Each "tree" is a basename under the agent's configured `trees_root`.

### Tool args

| Arg | Type | Required | Description |
|---|---|---|---|
| `tree` | string | yes | Bare basename of a tree file under `trees_root`. Slashes, `..`, NUL, absolute paths are refused before any FS call. |
| `query` | string | yes | Free-text semantic query. Passed as a single argv element. |
| `top_k` | integer | no | Number of top-scoring nodes the underlying script fetches. Defaults to the tool's `max_results`. Saturates at that cap. |
| `offset` | integer | no | Hit-rank to start from (0-based). |
| `limit` | integer | no | Maximum hits to return this call. Defaults to 5. Hard-capped at `max_results`. |

### Return shape

```json
{
  "hits": [
    {"node_id": "...", "score": 0.87, "pages": "12-13", "excerpt": "..."}
  ],
  "next_offset": 5,
  "truncated": false
}
```

The per-hit shape (`node_id`, `score`, `pages`, `excerpt`) comes from the underlying script — the tool wraps but doesn't normalise.

### Refusal modes

- `tree must be a bare filename` — slashes, `..`, NUL, or absolute path
- `tree {name} not found under trees_root`
- `tree {name} resolves outside trees_root` — symlink-out attempt within the trees directory
- `PDFQUERY_PROCESS_ERROR` — script non-zero exit, timeout, or python_bin failure

## Pagination contract (all three tools)

Each tool that supports pagination returns `next_offset`. Callers should:

1. Use the returned `next_offset` as the `offset` for the follow-up call.
2. Stop when the response has fewer than `limit` records AND `truncated: false`.
3. Trust `truncated: true` over `match_lines == limit` — a full page that exactly equals `limit` *might* be the last page; `truncated: true` is the unambiguous signal that more remains.

## Config example

Tools are activated per-agent in the bundle's `nsed.yml`:

```yaml
agents:
  - name: GlmAggregatorBot
    provider_id: openrouter
    model_name: z-ai/glm-5.1
    tools:
      read_file:
        roots: ["${BUNDLE_ROOT}/corpus", "${BUNDLE_ROOT}/linux"]
        max_bytes: 65536
      grep_search:
        roots: ["${BUNDLE_ROOT}/corpus", "${BUNDLE_ROOT}/linux"]
        max_bytes: 32768
        max_results: 100
        timeout_secs: 10
      pdf_query:
        trees_root: "${BUNDLE_ROOT}/trees"
        script_path: "${BUNDLE_ROOT}/scripts/pdf_query.py"
        python_bin: python3
        max_bytes: 65536
        max_results: 20
        timeout_secs: 30
```

Each tool's config goes through the validation loop in `nsed-cli/src/commands/serve.rs` at startup; a bad config fails the agent before traffic is accepted.

## See also

- [`scoped-read-file`](../explanation/scoped-read-file.md) — explanation of why these tools exist and the threat model they enforce
- `crates/nsed-agent/src/tools/scoped_read.rs` — `read_file` implementation
- `crates/nsed-agent/src/tools/scoped_grep.rs` — `grep_search` implementation
- `crates/nsed-agent/src/tools/pdf_query.rs` — `pdf_query` implementation
