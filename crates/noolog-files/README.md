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
| (crate root) | `sha256_hex_bytes`, the name a blob is stored under |

A store implements `Blob`; `upload` and `hls` work over the trait, so a new
store gets uploads and segmenting without changes here. The NATS tests need a
broker at `NATS_URL` (default `nats://localhost:4222`).
