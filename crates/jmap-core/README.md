# jmap-core

JMAP core (RFC 8620) data types, written from the RFC text: `Id`, the
Session, requests, responses, invocations, errors, and the `/get`, `/set` and
`/query` arguments. No transport and no request processing yet. Dependencies:
`serde`, `serde_json`. Mail types are in [jmap-mail](../jmap-mail/README.md);
reference: [docs/reference/jmap.md](../../docs/reference/jmap.md).

## Tests

```sh
cargo test -p jmap-core
```

| Suite | Checks |
|---|---|
| `rfc_examples` | the RFC 8620 examples parse and serialize back unchanged |
| `ids_and_methods` | `Id` validation; query defaults and nested filters; `/set` patches and errors |
