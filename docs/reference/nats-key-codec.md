# NATS key codec

Maps any string (a principal, an email, a job id) to a valid NATS KV key or
subject token, and back. `quorum_rs::nats_utils::nats_kv_key_encode` and
`nats_kv_key_decode`.

## Encoding

| Input byte | Output |
|---|---|
| `A`–`Z`, `a`–`z`, `0`–`9`, `-`, `_` | unchanged |
| any other byte, `=` and `.` included | `=XX`, uppercase hex |

| Input | Output |
|---|---|
| `alice` | `alice` |
| `op_1.main` | `op_1=2Emain` |
| `alice@noolog.io` | `alice=40noolog=2Eio` |

Properties: output is ASCII and matches `[A-Za-z0-9_=-]+`; contains no `.`,
so it is always one subject token; injective, so two inputs never share a key.

## Decoding

`nats_kv_key_decode` returns `None` for any string the encoder cannot
produce: lowercase hex, a dangling `=`, or a byte outside the alphabet. Each
input therefore has exactly one accepted encoding, and a key arriving from
the wire cannot be rewritten into another that decodes to the same value.
