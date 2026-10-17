# noolog-jmap

JMAP wire types, written from RFC 8620 (core) and RFC 8621 (mail): requests,
responses, errors, the Session, the standard `/get`, `/set` and `/query`
arguments, and `Mailbox`, `Thread` and `Email`. Types only, no transport or
server logic; dependencies are `serde` and `serde_json`. Reference:
[docs/reference/jmap.md](../../docs/reference/jmap.md).

## Tests

```sh
cargo test -p noolog-jmap
```

| Suite | Checks |
|---|---|
| `rfc_examples` | the RFC examples parse and serialize back unchanged |
| `ids_and_methods` | `Id` validation; query defaults and nested filters; `/set` patches and errors |
| `jmap_client` | Stalwart's `jmap-client` (dev-dependency only) parses what these types serialize |
