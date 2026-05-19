# Redeem an invite code

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
curl -X POST https://orch.example.com/admin/api/agent-invites \
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
quorum redeem eyJhbGc... --url https://orch.example.com
```

This:

1. Generates a fresh User NKey (`SU…` seed, `U…` public key) locally.
   The seed never leaves the host.
2. POSTs `{code, user_pub_key}` to `/redeem-agent`.
3. Receives `{user_jwt, nats_url, agent_id}`.
4. Combines the JWT + local seed into a `.creds` blob.
5. Writes `~/.nsed/agent.creds` and `~/.nsed/agent.seed` (mode
   0600 on Unix; user-profile dir on Windows).
6. Prints a summary with the connect URL and your agent pubkey.

Default file locations are overridable:

```bash
quorum redeem eyJhbGc... \
    --url https://orch.example.com \
    --creds-out /etc/my-agent/agent.creds \
    --seed-out  /etc/my-agent/agent.seed
```

`--force` overwrites existing files; without it the command bails
rather than clobbering a credential you might still need.

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
`agent_id`'s subjects — publish to `*.result.*.<agent_id>.*` and
heartbeat to `*.agent.heartbeat.<agent_id>` is allowed; everything
else is denied at the NATS server boundary.

## When something goes wrong

| `quorum redeem` says…                            | What to do                                                      |
|--------------------------------------------------|-----------------------------------------------------------------|
| "This invite code has expired."                  | Ask the admin for a fresh code.                                 |
| "This invite code was already redeemed."         | Same — codes are single-use; either you double-ran the command, or someone else got there first. |
| "The admin revoked this invite code."            | Admin pulled it deliberately; check with them.                  |
| "This invite code is invalid."                   | Copy/paste glitch most likely. Re-copy the full `eyJhbGc...` string from the source. |
| "The orchestrator does not have invite codes configured." | Operator-side issue, not yours — the orchestrator needs `APP_INVITES__SIGNING_SECRET` set. |
| "The orchestrator's backing store is temporarily unreachable." | Transient. The CLI retries with backoff; if it gives up, try again in a minute. |

## Embedding instead of shelling out

If your agent binary already drives its own startup and you'd
rather not shell out to `quorum redeem`, the same flow lives
behind `quorum_rs::nats_utils::redeem_invite_with_orchestrator`.
Worked example in [agent development guide §Bootstrap with an
invite code](agent-development.md#bootstrap-with-an-invite-code).

## See also

- [Agent development guide](agent-development.md) — Bootstrap section
- Companion API reference: `quorum_rs::nats_utils` rustdoc on
  [docs.rs/quorum-rs](https://docs.rs/quorum-rs).
