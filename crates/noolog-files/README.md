# noolog-files

Files an operator uploads, and what it takes to serve them: the `Blob`
storage seam, its NATS object-store backend, upload spooling and media-type
sniffing, and HLS segmenting. The contract between whoever stores files
(nsed-orchestrator today, the noolog node's PostgreSQL store from M1e) and
whoever writes or serves them (agents, the gateway). `quorum_rs::files`
re-exports this crate unchanged.

## Modules

| Module | Owns |
|---|---|
| `blob` | `Blob` (put, ranged get, head, annotations, delete, usage, list), `ObjectMeta`, `Visibility`, `Usage`, `QuotaExceeded`; `NatsBlob` and `open_blob` over a JetStream object store; `parse_byte_range` |
| `upload` | `spool` (hash while writing to disk, size cap enforced while streaming), `sniff_mime`, `read_upload_form` (axum multipart), `store_upload`, `Refusal` |
| `hls` | `segment_and_store` over any `Blob` with a `Transcoder` (`Ffmpeg`), `rewrite_playlist`, `HlsState`, derived-object naming (`derived_prefix`), `delete_with_derived` |
| `pipeline` | The files pipeline contract: the shared inbox `files_inbox`, the work-queue stream `FILES_PIPELINE`, `stage` (put the bytes, announce on `files.received.<owner>`), `Staged` (upload id, owner, metadata, `encryption`, plan of steps, derived objects, notes), `advance` along `files.step.<name>`, `open` (the staged bytes behind a `Verified` reader that fails unless they hash to the digest) and `discard` |
| (crate root) | `sha256_hex_bytes`, the name a blob is stored under |

A store implements `Blob`; `upload` and `hls` work over the trait, so a new
store gets uploads and segmenting without changes here. The NATS tests need a
broker at `NATS_URL` (default `nats://localhost:4222`).

## The pipeline

A producer stages bytes and announces them for its owner; the owner is the
subject's last token, which each producer's NATS credentials allow only for
its own operator. The consumer of `files.received.*` plans the steps (ending
in `commit`), and each step's worker reads the staged object, re-hashes it
against the digest, does its work and calls `advance`; `commit` streams `open` into the store, so altered bytes error before they land, then calls `discard`. Messages carry no
bytes. Staged objects are plaintext (`Encryption::None`) until steps run in
attested enclaves with released keys; the inbox is shared until per-operator
NATS tenancy.
