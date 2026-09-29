# Content fetch and the fleet token

Two contracts an agent shares with the orchestrator, kept here so both sides
build them from one copy.

## Reading addressed content (`quorum_rs::content`)

A proposal can carry a [content address](../../crates/quorum-rs/src/content_address.rs)
instead of its answer. Whoever holds the bucket answers requests for it on a
NATS subject; everyone else reads through that responder with
`content::fetch(client, subject, &address, capability)`.

| Rule | Why |
|---|---|
| A request is JSON `{"address", "offset"}`; a reply is at most `WINDOW_BYTES` (512 KiB) | a NATS payload is capped, so replies are paged from the offset |
| Every reply carries `Nsed-Content-Total` | the reader refuses a responder that serves more than it said the content holds |
| The last reply carries `Nsed-Content-Eof: true` | a reply with no bytes and no end is an error, never a loop |
| A refusal is `Nsed-Content-Error`, with an empty body | an error is never mistaken for empty content |
| A capability is presented as `Nsed-Content-Capability` | the router checks it; `capability_headers` builds it |
| The reassembled bytes must hash to the address's digest | the reply crossed an open subject from a party the reader does not authenticate |

KV keys derived from principals or addresses go through
`nats_utils::nats_kv_key_encode` (`[A-Za-z0-9_-]` literal, every other byte
`=XX`), and `nats_kv_key_decode` accepts only what that encoder produces.

## The fleet token (`quorum_rs::crypto::system_agent_token`)

The internal fleet's bearer is derived from the NATS account seed, so the
seed is the only secret to manage: the orchestrator provisions the token and
the fleet presents it. It is HMAC-SHA256 of the label `nsed-system-agent-v1`
keyed by the trimmed seed, as 64 lowercase hex characters. Changing the
derivation means a new label, on both sides at once.
