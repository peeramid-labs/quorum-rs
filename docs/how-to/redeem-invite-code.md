---
title: Redeem invite
order: 1
tagline: Bootstrap scoped NATS credentials by redeeming a single-use invite code on the agent host.
---
# Redeem invite

> Recipe for 3rd-party agent operators bootstrapping NATS
> credentials against a `nsed`-style orchestrator. Assumes the
> orchestrator is already running and an admin has agreed to mint
> a code for you.

## What an invite code is

A single-use, short-TTL, HMAC-SHA256-signed JWT minted by an
orchestrator admin. Carries `{sub: agent_id, operator_name?, jti,
iat, exp, aud: "nsed-agent-redeem"}`. You redeem it once for a
scoped NATS User JWT bound to an NKey **you generate locally on
the redeeming host**. The admin never sees your seed, and there's
no out-of-band pubkey sharing — `quorum redeem` does the keypair
generation and the redeem-request body assembly for you.

If the code is intercepted between admin and operator, the
attacker can redeem it with their own keypair (and lock you out
via the single-use rule). Codes are short-TTL and revocable, so
the legitimate operator notices immediately. For "share with a
friend over Signal" onboarding that's the right risk balance; for
production deployments, layer normal channel hygiene on top.

## The recipe

### 1. Ask the admin for a code

The admin runs (or hits the orchestrator's `/admin/api/agent-invites`
endpoint to mint) something like:

```bash
curl -X POST https://api.peeramid.xyz/admin/api/agent-invites \
     -H "Authorization: Bearer $ADMIN_TOKEN" \
     -H 'Content-Type: application/json' \
     -d '{"agent_id":"researcher-bot-3","operator_name":"alice"}'
# {"code":"eyJhbGc...","expires_at":1748104800,"jti":"f8b1...e2"}
```

Notice the request body has **no `user_pub_key`** — that's the
point of the redeem-time-pubkey design. The admin doesn't need
anything from you in advance.

The admin shares the resulting `eyJhbGc...` code with you via
whatever channel works (Signal/email/QR). Short-TTL: redeem
within the window they set or you'll need a fresh code.

### 2. Redeem on the agent host

```bash
# Default: redeems against https://api.peeramid.xyz
quorum redeem eyJhbGc...

# Working against a locally-running orchestrator:
NSED_ENV=local quorum redeem eyJhbGc...

# Or pass --url explicitly:
quorum redeem eyJhbGc... --url https://orch.example.com
```

URL resolution order: `--url` flag > `$ORCH_URL` env > built-in
default. The default is `https://api.peeramid.xyz`; setting
`NSED_ENV=local` (or `dev` / `development`, case-insensitive) flips
it to `http://localhost:8080` so `nsed serve` on the same host
works without the flag.

This:

1. Generates a fresh User NKey (`SU…` seed, `U…` public key) locally.
   The seed never leaves the host.
2. POSTs `{code, user_pub_key}` to `/redeem-agent`.
3. Receives `{user_jwt, nats_url, agent_id}`.
4. Combines the JWT + local seed into a `.creds` blob.
5. Writes `~/.nsed/agent.creds` and `~/.nsed/agent.seed` (mode
   0600 on Unix; user-profile dir on Windows).
6. Prints a summary with the connect URL and your agent pubkey.

The output directory defaults to `~/.nsed`; redirect all files with
`--out-dir`:

```bash
quorum redeem eyJhbGc... \
    --url https://orch.example.com \
    --out-dir /etc/my-agent
# writes /etc/my-agent/{agent.seed,agent.creds,operator.token,orchestrator}
```

`--force` overwrites existing files; without it the command bails
rather than clobbering a credential you might still need.

Two more flags matter on a flaky link:

- `--seed-in PATH` reuses an existing `SU…` seed instead of generating a
  fresh keypair, so the pubkey stays the same across redemptions. Use it
  when a transient failure consumed a code *after* the orchestrator
  marked it redeemed — pre-stage the seed once, then redeem the fresh
  code with `--seed-in ~/.nsed/agent.seed`. An empty or non-`SU…` file
  is rejected before any orchestrator round-trip.
- `--max-attempts N` (default `5`) caps retries on transient failures
  (5xx, `kv_unavailable`, network blips). The same pubkey is presented on
  every attempt, so a retry can never turn into a replayed code.

### 3. Point your agent at the creds

Two options:

**A. Hand the file path to the agent runtime.** Most NATS clients
take a `creds_file` config. `quorum-rs::nats_utils::NatsAuth`
accepts both:

```yaml
nats_auth:
  creds_file: /home/operator/.nsed/agent.creds
```

**B. Use `inline_creds` if your runtime keeps the agent in-memory:**

```rust
let creds = std::fs::read_to_string("/home/operator/.nsed/agent.creds")?;
let auth = NatsAuth { inline_creds: Some(creds), ..Default::default() };
```

### 4. Run your agent

The agent connects to `nats_url` from the redeem response with the
`.creds` you wrote. The JWT scopes the connection to your
`agent_id`'s subjects; everything else is denied at the NATS server
boundary. With `<sp>` the deployment's subject prefix and `<ap>` its
API prefix:

| Direction | Subject | Purpose |
|---|---|---|
| publish | `<sp>.*.result.*.<agent_id>.*` | proposals and evaluations |
| publish | `<ap>.agent.heartbeat.<agent_id>` | heartbeats |
| publish | `<ap>.jobs.ack.*.<agent_id>` | job acks |
| publish | `<sp>.*.result.event.*` | accepted / working / error events (agent id in payload) |
| publish | `<sp>.*.audit.*` | audit trail |
| publish | `<sp>.content.*.<agent_id>.put` / `.get` | the agent's own content-addressed answers |
| subscribe | `<sp>.*.task.<agent_id>.*` | task assignments |
| subscribe | `<ap>.jobs.manifest.>` | job manifests |
| subscribe | `<ap>.orchestrator.ping` | orchestrator pings |
| subscribe | `<sp>.*.audit.*` | audit trail |

Plus `_INBOX.>` both ways for request-reply.

## When something goes wrong

| `quorum redeem` says…                            | What to do                                                      |
|--------------------------------------------------|-----------------------------------------------------------------|
| "This invite code has expired."                  | Ask the admin for a fresh code.                                 |
| "This invite code was already redeemed."         | Same — codes are single-use; either you double-ran the command, or someone else got there first. |
| "The admin revoked this invite code."            | Admin pulled it deliberately; check with them.                  |
| "This invite code is invalid."                   | Tampered during copy/paste (re-copy the full `eyJhbGc...` string), or the minting and redeeming orchestrators use different `APP_INVITES__SIGNING_SECRET` values. The CLI picks `/redeem` vs `/redeem-agent` from the code's `aud`, so a wrong code type only produces this when you call the endpoint by hand. |
| "invite code is not in JWT shape" / "carries unknown audience" | Not a code this CLI can redeem — you pasted something other than the `eyJhbGc...` string, or a truncated one. Nothing was sent to the orchestrator. |
| "The orchestrator does not have invite codes configured." | Operator-side issue, not yours — the orchestrator needs `APP_INVITES__SIGNING_SECRET` set. |
| "The orchestrator's backing store is temporarily unreachable." | Transient. The CLI retries with backoff; if it gives up, try again in a minute. |

## Unified codes — one paste, chat + agent

The `/admin/api/invites` operator endpoint (the one for HTTP bearer
tokens) mints a *unified* code when the transport role `agent` is in
the recipient's roles. The shortcut is the `operator_agent` preset:

```bash
curl -X POST https://api.peeramid.xyz/admin/api/invites \
     -H "Authorization: Bearer $ADMIN_TOKEN" \
     -H 'Content-Type: application/json' \
     -d '{"username":"alice","preset":"operator_agent"}'
```

Equivalent: `"roles": ["post_deliberation", "view_results",
"manage_agents", "agent"]` (`capabilities` is accepted as an alias for
`roles`). Redeeming the single code at `/redeem` returns the bearer
token AND a scoped NATS User JWT + `nats_url`. Without `agent` the
code is chat-only: `preset: "operator"` (or omitting both fields)
gives the operator default set, `preset: "guest"` gives
`post_deliberation` + `view_results`.

`grants` is a different field — tenancy globs pinned on the redeemed
token, not a capability list. Older docs that show `grants: ["chat",
"agent"]` predate that split; a body written that way mints a chat-only
code with two junk grants.

For SDK consumers this is the helper:

```rust,no_run
use quorum_rs::nats_utils::redeem_operator_invite_with_orchestrator;

let kp = nkeys::KeyPair::new_user();
let resp = redeem_operator_invite_with_orchestrator(
    "https://api.peeramid.xyz",
    invite_code,
    Some(&kp.public_key()),   // unified code? pass pubkey
    Some("my-agent"),         // device hint for audit log
).await?;

// `resp.token`     — HTTP bearer (always present)
// `resp.user_jwt`  — NATS User JWT (only when roles include "agent")
// `resp.nats_url`  — NATS server URL (paired with user_jwt)
# Ok::<(), Box<dyn std::error::Error>>(())
```

Pass `Some(&pubkey)` unconditionally — chat-only codes silently
ignore the field, so the same call site handles both flavours. The
`nsed init` wizard uses exactly this pattern.

## Config-free operator client — redeem, then just run

Redeeming an **operator** code (`/redeem`, the unified or chat-only
flavour above) writes three things under `~/.nsed/`:

- `operator.token` — your HTTP bearer (mode 0600).
- `orchestrator` — the orchestrator's HTTP address, captured from the
  URL you redeemed against.
- `agent.creds` + `agent.seed` — only for unified (`agent`-capable) codes.

With the address persisted beside the token, the discovery and submit
commands need **no `quorum.yml`**:

```bash
quorum redeem eyJhbGc...            # writes ~/.nsed/{operator.token,orchestrator}
quorum rooms                        # what rooms can I submit to?
quorum run --policy noosphera:0v1 "Summarise the Q3 risks"
quorum status <job-id>
```

`run`, `status`, `rooms`, and `trace` synthesize a single-orchestrator
workspace from `~/.nsed/orchestrator` + `~/.nsed/operator.token` when no
config file is found. Policies and rooms are discovered live from the
orchestrator's grant-filtered `GET /policies` and `GET /rooms`, so a
joiner picks from `quorum rooms` instead of hand-writing config.

Endpoint precedence: `$QUORUM_ORCHESTRATOR` env wins, then the persisted
`orchestrator` file. The file is looked up in the current directory
first (so `quorum redeem --out-dir .` then running from that dir works),
then `~/.nsed`. A `quorum.yml` (via `--config`) still overrides everything
when you need multi-orchestrator routing.

> Note: in this config-free mode `quorum run` targets the single
> redeemed orchestrator. Routing a job to a specific room id without a
> `quorum.yml` (`--room`) is a follow-up; today pass `--policy <label>`.

## Embedding instead of shelling out

If your agent binary already drives its own startup and you'd
rather not shell out to `quorum redeem`, the same flow lives
behind `quorum_rs::nats_utils::redeem_invite_with_orchestrator`
(the dedicated agent endpoint) or
`redeem_operator_invite_with_orchestrator` (the unified one above).
Worked example in [agent development guide §Bootstrap with an
invite code](agent-development.md#bootstrap-with-an-invite-code).

## See also

- [Agent development guide](agent-development.md) — Bootstrap section
- Companion API reference: `quorum_rs::nats_utils` rustdoc on
  [docs.rs/quorum-rs](https://docs.rs/quorum-rs).
