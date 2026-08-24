---
title: Agent Identity Keys
order: 16
tagline: The keys an agent declares, the signatures an operator attaches to a held response, and what is carried but not yet checked.
---
# Agent identity keys

An agent is addressed by its `name` everywhere — subjects, rosters, config. These
fields add the keys that name binds to, so a reader holding both can tell whether
a signature came from the agent it claims to.

> **Carried, not checked.** Nothing in the SDK verifies these signatures yet. A
> well-formed signature is accepted and stored; a malformed one is refused. Treat
> what follows as provenance that travels with a response, not as proof of it,
> until a verifier rejects a tampered message.

## Agent config

```yaml
- name: "Reviewer"
  model: "some-provider.some-model"
  signing_key: "${REVIEWER_SIGNING_SEED}"   # or: file:~/.nsed/reviewer.key
  operator_pubkeys:
    - "0x03bb…"
    - "0x03cc…"
```

| Field | Default | Meaning |
| --- | --- | --- |
| `signing_key` | unset | Reference to the agent's own signing key: `${VAR}`, `file:<path>`, or a literal, each naming a hex-encoded 32-byte Ed25519 seed. Unset means the agent signs nothing. |
| `operator_pubkeys` | empty | Public keys authorised to act for this agent, i.e. an operator editing a held response. A list, because an agent may have more than one operator and because a key must be rotatable without a window where neither the old nor the new one works. |

**Put the reference in config, not the key.** `signing_key` names where the seed
lives so the seed itself is not carried by a file that gets committed, copied, or
serialized back out. A literal is accepted because the resolver allows one, but
writing a seed inline puts a private key wherever that file goes. A key file
should be readable only by the account running the agent.

The two fields are asymmetric on purpose. An agent holds its *own* private key, so
its public identity is **derived** from that key — never declared. A declared
public key would be unverifiable decoration: nothing reconciles it against the key
actually doing the signing, and the two could disagree silently. Operator keys are
the opposite case — those private keys belong to people, elsewhere, so naming
which public keys may act for the agent is the only way to state it.

A reference that resolves to nothing, or to something that is not 32 bytes, yields
no identity rather than a different one: the agent announces no key instead of one
nobody can check.

A configured key is installed as the worker's signing hook at construction, so the
agent signs with the key it announces. A caller that sets its own hook afterwards
still wins — the builder runs after construction.

### Adding a key backend

The reference resolves to an `AuditSigner`, not to key material. The schemes above
name a *stored secret*, which is read into a software Ed25519 signer, but a token,
a TPM or a secure enclave never surrenders its private key — it signs on request.
Those are added as further arms of the same resolver and need no change at any
call site, because nothing downstream ever sees a seed.

## Heartbeat

The public half of `signing_key` is derived and published on every heartbeat as
`agent_pubkey` (`{api_prefix}.agent.heartbeat.{agent_id}`). The orchestrator holds
no copy of an agent's config, so the heartbeat is the only route by which a key
reaches the registry that would later have to check a signature against it.

It is derived per heartbeat rather than cached, so an agent whose key reference
stops resolving announces nothing rather than a key it can no longer use. Absent
when no `signing_key` is configured.

## Operator signatures on a held response

An operator editing a buffered response may attach signatures over the edit:

```http
PUT /api/agents/{name}/buffer/{id}
```

```json
{
  "content": { "…": "…" },
  "operator_comment": "tightened the second claim",
  "signatures": [
    {
      "algorithm": "ed25519",
      "public_key": "0x03bb…",
      "signature": "3q2+7w==",
      "role": "operator",
      "signer_id": "alice"
    }
  ]
}
```

They are recorded on the resulting `OperatorAnnotation` and travel with the audit
trail.

| Field | Meaning |
| --- | --- |
| `algorithm` | Names the scheme, e.g. `ed25519`, `secp256k1`, `ml-dsa-65`. A verifier dispatches on this string, so a signature without it names nothing that could check it. |
| `public_key` | The signer's key, hex-encoded. Carried alongside rather than looked up, so a reader need not guess among an agent's operators. |
| `signature` | The signature bytes, base64-encoded. |
| `role` | `author`, `evaluator`, `orchestrator`, `operator`, or `witness`. |
| `signer_id` | Who signed — an agent id, an operator principal. |

### Why a list

The audit envelope this feeds is multi-signature, and a classical and a
post-quantum signature over the same edit are separate entries rather than one
opaque blob. A reader can then see which algorithms actually covered the edit,
instead of inferring it. Signing both ways is best-effort: if the post-quantum
signer fails, a classical-only signature is still produced, and that difference is
only legible if the entries are distinct.

### What is rejected

`signatures` is optional — an unsigned edit is accepted, and whether *unsigned* is
acceptable is the release path's decision, not this type's. What is refused, with
`400`, is a signature that could never be checked:

- an entry missing its `algorithm`, `public_key`, or `signature`
- any of those present but blank — an empty string satisfies a presence check
  while carrying nothing, which is how a fail-closed check becomes fail-open

Both edit shapes are validated: a comment-only edit carrying a malformed
signature is refused exactly like a content edit carrying one.
