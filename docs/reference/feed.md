# Feed

The contract for channels and their records, implemented by `noolog-feed`
([crate](../../crates/noolog-feed/README.md)). It holds for every store and
every consumer: the node core (PostgreSQL), nsed-orchestrator (JetStream),
the web client and agents. Routes, posting limits, lists and rendering are
the serving service's, not part of this contract.

## Channels

A channel is an address, `showcase@noolog.io`, with records appended to it.

| Property | Rule |
|---|---|
| Existence | A channel is registered by its first record (`ensure_channel`); there is no create operation. |
| Registration | Idempotent. `created_at` is the time of the first registration; later ones do not change it. |
| Ownership | None. Any principal may append; a channel carries no owner, title or description. |
| Index | `list_channels` returns every registered channel, sorted by address. |

## Addressing

`validate_address(address)` accepts exactly the addresses a deployment issues;
`channel_address(name, domain)` builds one from a name.

| Part | Rule |
|---|---|
| Local part | 1–64 characters of `[a-z0-9.-]`; no leading or trailing `-` or `.`, no `--`, no `..` |
| Domain | contains a `.`; `[a-z0-9.-]` |
| Whole address | at most 254 characters |

Addresses are refused, never normalised: uppercase, spaces, `+tags` and
doubled separators fail validation. `channel_address` maps a free-form name to
a valid local part (lower-cased, runs of other characters become one `-`).
An address is an opaque key; nothing is parsed out of it.

## Records

A record is shaped after JMAP `Email` (RFC 8621) and serialised in camelCase.
It is not JMAP-conformant: `id` is a number.

```json
{
  "id": 42,
  "threadId": null,
  "from": "acme@noolog.io",
  "to": ["showcase@noolog.io"],
  "subject": "What we shipped in August",
  "receivedAt": "2026-09-01T09:27:46Z",
  "size": 1840,
  "keywords": [],
  "textBody": "…",
  "htmlBody": "…",
  "attachments": [
    { "blobId": "3b1f…", "type": "video/mp4", "size": 41288301, "name": "demo.mp4" }
  ],
  "messageId": "<cover@example.org>",
  "inReplyTo": null,
  "references": []
}
```

| Field | Rule |
|---|---|
| `id` | Assigned by the store when the record is appended: per channel, ascending from 1. A caller's value is ignored. |
| `to` | Every channel the record is posted to. |
| `keywords` | JMAP keywords. `$draft` marks a draft (below). Per-reader flags (`$seen`, `$flagged`) are not stored. |
| `attachments[].blobId` | A content digest. Where the bytes are kept, and the `url` filled in on read, are the serving deployment's. |
| `messageId`, `inReplyTo`, `references` | RFC 5322 headers, for records that arrived as or are served as mail. |
| `signedBy` | The public key that signed the post, when the poster had one. |
| absent | `cc`, `bcc`: a broadcast record has no copied recipients. |

**Drafts.** A record carrying `$draft` is stored but never served: not by id,
not by slug, not in a page.

**Message ids.** A `messageId` points at the first record that carried it, per
channel and, separately, per sender. A redelivered message is appended again
but the id keeps naming the first record (`get_by_message_id`,
`get_by_sender_message_id`).

**Paging.** `page(address, before, limit)` returns records newest first with
ids below `before`; `page_from(address, first, limit)` returns records oldest
first from id `first`. Both leave drafts out.

## Blog entries

A record that is a blog post or page carries the vendor property
`noolog:blog` (capability `https://noolog.io/jmap/blog`, RFC 8620 §1.8). A
client that does not know it sees an ordinary record.

```json
"noolog:blog": {
  "slug": "introducing-nsed",
  "kind": "post",
  "visibility": "public",
  "featured": true,
  "excerpt": "…",
  "featureImage": { "url": "https://…/cover.png", "alt": "…" },
  "authors": [{ "slug": "peersky", "name": "Tim Pechersky" }],
  "tags": [{ "slug": "news", "name": "News" }],
  "meta": { "description": "…", "ogImage": "https://…" },
  "origin": { "system": "ghost", "id": "…" }
}
```

The full schema is kept in the noolog-rfcs repository.

| Rule | Enforced by |
|---|---|
| A public tag's slug is repeated in the record's `keywords`, so clients filter by tag the way they filter by keyword; internal tags (`internal: true`) are not | producers (convention; the Ghost importer follows it) |
| A draft is `$draft` | `noolog-feed`, as for any record |

### Slugs

A slug names an entry permanently within its channel.

| Property | Rule |
|---|---|
| Form | 1–200 characters of `[a-z0-9._-]`, not starting with `.` (`validate_slug`) |
| Revision | Posting again under a slug you hold moves it to the new record; earlier records keep their ids. |
| Drafts | A `$draft` revision is not indexed; the slug keeps naming the live entry. |
| Ownership | The first poster holds the slug in that channel. Another sender's post under it fails with `SlugTaken` after its record is appended; nothing is rolled back. |
| Races | Settled by compare-and-set on the index entry: two senders racing one slug have exactly one winner. |

## Stores

`FeedStore` is what a store implements: channel registration and lookup,
append, read by id, the two paging queries, the slug index with
compare-and-set, and the message-id index with first-claim inserts. The rules
above are implemented once, over that trait, in `noolog-feed`.

| Store | Where |
|---|---|
| `MemoryFeedStore` | `noolog-feed` (reference) |
| `JetStreamFeedStore` | nsed-orchestrator, `nsed-content` |
| `PostgresFeedStore` | noolog-node, `core` |

A store conforms when it passes `noolog_feed::conformance` (feature
`conformance`), one check per rule above.
