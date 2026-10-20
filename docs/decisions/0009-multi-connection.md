# 0009: Multi-connection downloads with dynamic splitting

Accepted 2026-10-12.

**Decision:** a download is split across several connections (default 4, never into ranges under 1 MiB). When a connection finishes, it takes an unfinished range nobody is working on, or else the second half of the busy range with the most left. This is IDM-style dynamic splitting; each connection is one long HTTP request.

**When it splits.** The first request always asks for `bytes=0-`, and splitting happens only if the server answers `206` (proof that ranges work), sends a strong ETag or a `Last-Modified` (so `If-Range` can catch a change), doesn't say `Accept-Ranges: none`, and the file is at least two minimum ranges long. Otherwise the existing single-connection path runs. That first response becomes the range starting at 0, so there's no separate probe request.

**How it works** (`crates/core/src/segmented.rs`)
- A coordinator (the `run` future) owns the range list. Each connection is a task in a `JoinSet`, writing through its own file handle at its own offset.
- Coordinator and connection share three atomics: `end` (only the coordinator lowers it, on a split), `pos` (written so far) and `durable` (flushed to disk).
- The `.part.ottie` file gets its full size up front. A resume checks that length; a different size means `RestartReason::PartFileDamaged`.
- Connections flush at least every second. `Event::Checkpoint` reports only flushed ranges; the manager saves them with the next progress save (rule from 0008).
- Every extra request is a closed range (`bytes=N-M`) with `If-Range`. A `200`, or a `Content-Range` that doesn't match, means the file changed: all connections stop and the run restarts from zero, at most once per run (`Error::RemoteUnstable` after that). A restart empties the `.part.ottie` (synced) before its new request, so if that request fails, the next try finds an empty file to match the empty map, never a file at full size with holes that a resume at its length would take as complete (added 2026-10-20).
- A connection may write a few bytes past a split point it hadn't noticed yet. That's harmless, because `If-Range` guarantees both connections write the same bytes, and `done` is capped at `end`.
- An error on one connection stops the others and ends the run. Retries come in 2f.

**Storage:** a `segments` table (migration 2): `(job, range_start, range_end, done)`, replaced in the same transaction as the job row. Rows are removed once the file is complete.

**Alternatives considered:** fixed pieces with a worker pool (BitTorrent-style: simpler and stream-friendly, but one request per piece), and a static split (simplest, but the slowest connection decides when the download ends).

**Later:** streaming mode (step 3) will split near the playhead and fetch the last range early, instead of splitting the range with the most left. Per-download and per-category connection counts come with settings and categories.
