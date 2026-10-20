# 0029: Play while downloading

Accepted 2026-10-13. Adds an exception to `docs/decisions/0014`: play links work without the token.

**Decision** (Mathieu's choice): a download's file, complete or not, can be played through a short-lived **play link**. The download order adapts in two ways: it **follows the player** (seeks included), and later a per-download **streaming mode** keeps a strict order from the start. They come as three slices:
- **5f1:** play links and the reader. The order doesn't change yet. One connection already arrives in order; with several, a player waits for the gap before its position to fill.
- **5f2, follow the player:** while a link is being read, connections go where the player reads, and the last 1 MiB is fetched early (many MP4s keep their index there). Connections show what they've written more often while someone reads. See *Following the player* below.
- **5f3, streaming mode:** a toggle saved with the download (when adding, or in the detail panel): in order from the start, last chunk first. On by itself for videos and music, which a setting can turn off. See *Streaming mode* below.

**Play links:**
- `POST /jobs/{id}/stream` (with the token) answers `{ "url": "/api/v1/stream/<ticket>/<file name>" }`. The ticket is 32 random bytes as hex. A job keeps the same link while it's in use.
- `GET /api/v1/stream/<ticket>/<name>` needs **no token**: players (VLC, mpv, a browser's `<video>`, Android players) can't send one. The ticket stands for it. The name is only there so players can show it.
- A link reads one job and nothing else. It expires once **unused for 12 hours**. Links live in the daemon's memory, so a restart ends them all. Anyone who has a link can play that file, and the UI says so.
- A complete job is served from its file, an unfinished one from its `.part.ottie`. `Range` works as usual (`206`, `416` past the end) when the size is known. With no size, the whole file is sent as it arrives, with `Accept-Ranges: none`.
- A read past what has arrived **waits** for the bytes, also while the download is queued or paused. It ends (a broken transfer, never wrong bytes) if the download fails, is cancelled or removed, or starts over because its file changed on the server. Shutdown ends open streams too. A restart is seen even when its event was missed (a player paused for minutes stops reading, and its reader falls behind on events): each job counts its restarts, and every read compares the count with the one it opened with (added 2026-10-20).
- Responses carry `Content-Security-Policy: sandbox` and `X-Content-Type-Options: nosniff`. A downloaded web page or SVG opened through its link then can't run scripts as the daemon's own pages (which hold the token).

**The reader** (`ottie_core::Reader`): it hands out only bytes already on disk. With one connection, that's up to the `.part.ottie` length. With several, it's the ranges written so far. While someone reads, every 200 ms each connection hands its bytes to the system (no disk sync) and the download shows readers its ranges directly (through `Playback`), so a reader is under half a second behind. Otherwise readers go by the ranges saved every second. Syncing to disk every 200 ms instead was tried first, and made players wait seconds after a seek: on Windows, each sync of a large file can take that long. Once complete, it's the whole file. It keeps the file open while the download completes or moves. Windows allows the rename because Rust opens files with `FILE_SHARE_DELETE`, and a test covers it. It reopens the file when the job's path changes.

**Following the player (5f2):**
- Each reader keeps its next offset (its *head*) in a `Playback` shared with the job's run, which a backend gets next to its `Throttle`. A head lives as long as its request: a player that seeks opens a new request and drops the old one.
- Every 200 ms, a multi-connection download works out the points players need: from each head, the first byte that hasn't arrived (newest head first), then the same from the start of the last 1 MiB.
- A point that no connection reaches within `min_split` bytes (1 MiB) gets a range of its own, and a free connection takes it before any other range. If none is free, the busy connection that leads to no such point and has the most left stops where it is (at its next chunk) and goes there; the rest of its range waits for a free connection. Each time costs one request and what that connection had buffered (up to ~400 KiB, as with any split).
- A download over one connection already arrives in order, so it doesn't change. Reading doesn't start a queued or paused download.

**Streaming mode (5f3):**
- Each download has a `streaming` flag, saved with it. The one adding it may set it (`"streaming"` in `POST /jobs`); otherwise the daemon turns it on for videos and music when the setting `streamMedia` is on (the default, in Settings → Downloads). It goes by the link's extension, then decides again by the file's real name at the first response, as the category does (`docs/decisions/0012`). Once the user sets it, when adding or later (`PATCH /jobs/{id}`), the file type no longer decides. The list of video and music extensions is the UI's (`filetypes.ts`), copied in `playback.rs`.
- A multi-connection download in streaming mode works as if a player read from the start (or from where the newest player reads): its last 1 MiB first, then the file in order. Each connection takes the next range nobody works on, cut to one **piece**; each busy range is cut down to a piece once it has more than 4 pieces left, so a download switched to streaming mode brings its connections to the front within a piece.
- A piece is what one connection fetches in about 2 s at the download's measured speed (`HttpConfig::piece_time`), and at least 4 × `min_split` (4 MiB). Each piece costs a request, so a piece of 2 s keeps that to a few percent of the time even on fast links; the playable part then grows in steps of about one piece per connection.
- The flag reaches the running download through its `Playback`, so a change takes effect at once.
- A download over one connection already comes in order; streaming mode changes nothing there (its last 1 MiB doesn't come first).
- Alternatives (Mathieu's choice): one connection in strict order, plus one for the end (simplest, but one connection's speed); fixed 1 MiB pieces as aria2's in-order mode (tighter order, but a request per MiB per connection, which costs speed on fast links). And for the default: off unless set, or on for videos and music (chosen, with the setting to turn it off).

**In the UI:** the detail panel shows **Play** (a link to open in a new tab; the desktop app opens it in the system's browser) and **Copy link** (for VLC and other players), for videos and music that have a file. `docs/decisions/0031` later let Play open a media player. The add step and the detail panel (until the download is over) have a **Streaming mode** switch; in the add step it shows what the daemon would choose until the user changes it.

**Alternatives:**
- A key derived from the token and the job ID (HMAC) in the URL: no state, and it survives restarts. But revoking one link means changing the token for every client, and it's a long-lived credential sitting in player histories.
- Header-only auth, playing through `fetch` + Media Source in our own page: no external players, and many formats wouldn't play.
