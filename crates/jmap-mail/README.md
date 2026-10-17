# jmap-mail

JMAP for Mail (RFC 8621) data types over [jmap-core](../jmap-core/README.md),
written from the RFC text: `Mailbox`, `Thread`, `Email`, `EmailAddress`, body
parts and values, and the `Email/query` filter condition. Reference:
[docs/reference/jmap.md](../../docs/reference/jmap.md).

## Tests

```sh
cargo test -p jmap-mail
```

| Suite | Checks |
|---|---|
| `rfc_examples` | the RFC 8621 examples parse and serialize back unchanged; filters nest |
| `jmap_client` | Stalwart's `jmap-client` (dev-dependency only) parses what these types serialize |
