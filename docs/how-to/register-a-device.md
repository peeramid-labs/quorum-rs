# How to register a device (self-serve, no invite)

Mint an operator token from a device key pair — no invite code, no admin. The
caller proves possession of a NATS **user** nkey by signing a domain-separated
message; the orchestrator returns a fresh operator token seeded with the default
promotional credits.

Use this for the anonymous activation funnel. To join an existing grant space
instead, use the collaborator-invite path (`POST /api/invites/accept`).

## 1. Mint (or load) a device identity

`DeviceIdentity` (in `quorum-crypto-core`, default `device` feature) wraps a
NATS user nkey:

```rust
use quorum_crypto_core::DeviceIdentity;

let id = DeviceIdentity::generate();            // fresh U… user key
let seed = id.seed()?;                           // persist this (mode 0600); never transmit
let pubkey = id.public_key();                    // U… — sent to the server
```

Rehydrate a persisted device on the next run with `DeviceIdentity::from_seed(&seed)?`
(it rejects a non-user key).

## 2a. Create path (first registration)

Sign the static, domain-separated message and post it. No nonce.

```rust
let msg = format!("nsed-operator-register:{pubkey}");
let signature = id.sign_hex(msg.as_bytes())?;    // lowercase hex
```

```
POST /register
{ "pubkey": "<U…>", "signature": "<hex>", "display_name": "optional" }
```

A **replayed** create (same pubkey) collides with `409 Conflict` — one device
maps to exactly one operator (`op_<sha256(pubkey)[..16]>`), so a replay can't
mint a second free operator.

## 2b. Login path (idempotent, multi-device)

To re-authenticate a known device — or deterministically auto-sync a second
device to the same operator — use a one-time nonce instead of the static
message:

```
GET /auth/challenge?pubkey=<U…>        →  { "nonce": "<one-time>" }
```

Sign `"{nonce}:{pubkey}"`, then post the nonce alongside the signature:

```
POST /register
{ "pubkey": "<U…>", "signature": "<hex over nonce:pubkey>", "nonce": "<one-time>" }
```

With a valid nonce a **re-seen** pubkey returns its **existing** token (no 409);
the nonce is consumed (replay-safe).

## 3. Store the token

```json
{ "name": "op_1a2b3c4d5e6f7081", "token": "…", "budget": 0.0 }
```

`token` is shown **once** — store it securely; it authenticates subsequent API
calls as this operator. `budget` is the seeded promotional credit, set by the
deploy's self-serve grant policy (0 by default).

## Constraints

- `pubkey` must be a NATS **user** key (`U…`); account/server keys are rejected.
- `grants` and `public` are rejected if set — a self-serve operator may not
  self-assign tenancy grants (that would bypass the paid-ensemble gate). Join a
  grant space via the collaborator-invite path instead.
- Signatures are lowercase hex over the exact message bytes; a wrong message,
  encoding, or key type fails verification.

## See also

- [`persona` yaml shapes](../reference/persona-yaml-shapes.md)
- `DeviceIdentity` API — `quorum_crypto_core::DeviceIdentity` rustdoc.
