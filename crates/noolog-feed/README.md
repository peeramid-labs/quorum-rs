# noolog-feed

Channels, threads and records shaped like JMAP `Email`, the rules over them,
and the storage seam they sit on. No NATS and no dependency on the rest of the
SDK: it is the contract between the node core (noolog-node, which keeps the
feed in PostgreSQL), nsed-orchestrator, the web client and agents. The
contract itself is documented in [docs/reference/feed.md](../../docs/reference/feed.md).

## Modules

| Module | Owns |
|---|---|
| (crate root) | `Channel`, `Record`, `BodyPart`, `SlugEntry`, `SlugTaken`, `DRAFT`; `channel_address`, `validate_address`; the rules over a store: `ensure_channel`, `get_channel`, `list_channels`, `post`, `page`, `page_from`, `get_record`, `get_by_slug`, `slug_entry`, `get_by_message_id`, `get_by_sender_message_id` |
| `store` | `FeedStore` (the primitives a store answers), `MemoryFeedStore` (the reference store) |
| `blog` | The `noolog:blog` vendor extension on a record: `Blog`, `Meta`, `Author`, `Tag`, `Image`, `BLOG`, `BLOG_CAPABILITY` |
| `conformance` | One async check per rule, each taking a store and an address no earlier run used; feature `conformance` |

## Rules and stores

The rules live above `FeedStore` and hold for every store: a draft is stored
but never served; a slug points at the newest revision by whoever took it
first, by compare-and-set, so a race has one winner; a message id points at the
first record that carried it, per channel and per sender; record ids are
assigned by the store, per channel, from 1.

| Store | Crate |
|---|---|
| `MemoryFeedStore` | here |
| `JetStreamFeedStore` | nsed-orchestrator `nsed-content` |
| `PostgresFeedStore` | noolog-node `core` |

A store is correct when it passes `conformance`:

```rust
let store = MyStore::new(...);
noolog_feed::conformance::a_slug_cannot_be_taken_from_another_operator_even_in_a_race(&store, "fresh@noolog.io").await?;
```

## Tests

```sh
cargo test -p noolog-feed                 # rules and conformance on MemoryFeedStore
```

The JetStream and PostgreSQL stores run the same suite in their own
repositories, with feature `conformance`:

```toml
noolog-feed = { git = "https://git.peeramid.xyz/peeramid-labs/quorum-rs.git", rev = "…", features = ["conformance"] }
```
