# Talking to the Ottie daemon

Everything Ottie does goes through the daemon's HTTP API: the web UI, the desktop app and the browser extension use it, and so can your scripts. This guide covers how to sign in, the conventions, live events, errors and play links. The full reference, every endpoint and field, is the OpenAPI description:

- [`openapi.json`](openapi.json) in this folder (open it in any OpenAPI viewer, such as Swagger Editor or Redocly), or
- `GET /api/v1/openapi.json` on a running daemon, which describes exactly that version.

Both are generated from the code (`cargo test -p ottie-api` rewrites the file), so they match what the daemon answers. TypeScript clients can use the generated types in [`shared/api-types/`](../../shared/api-types/) instead of writing their own.

## Where it is

`ottie serve` listens on **`http://127.0.0.1:24068`**, on this computer only. Change it with `--address`/`--port`, `OTTIE_ADDRESS`/`OTTIE_PORT`, or `address`/`port` in `config.toml`. Everything is under **`/api/v1`**; every other path is the web UI.

Reaching it from another device: see [`docs/remote.md`](../remote.md) (Tailscale, a VPN or a reverse proxy with HTTPS; never the port open to the internet), and give that device a token of its own (below).

## Signing in

Every request needs the API **token**: 64 hex characters, created the first time the daemon runs.

```bash
ottie token        # prints it
```

It's stored in the `token` file of Ottie's config folder (`%APPDATA%\Ottie` on Windows, `~/.config/ottie` on Linux, `~/Library/Application Support/Ottie` on macOS). Anyone who has it can control your downloads, so keep it private.

Send it as a bearer token:

```bash
TOKEN=$(ottie token)
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:24068/api/v1/jobs
```

Without it, or with the wrong one, every path under `/api/v1` answers `401` (even unknown paths, so they don't reveal what exists). The token never goes in a URL or a cookie.

Before sending this computer's token to an Ottie on this computer, a client can make sure that's what answers: `POST /api/v1/identity` with `{"nonce": "<32 to 128 hex digits, random, new each time>"}` needs no token and answers `{"proof": "…"}`, the HMAC-SHA256 of `ottie identity v1`, a line break and the nonce, keyed with the token, as hex. Compute the same and compare (in constant time); if they differ, something else holds the port, and it mustn't get the token. The desktop app and the `ottie` command do this.

There are two exceptions. The events WebSocket takes the token in a different way (see [Live events](#live-events)), and play links need no token at all (see [Play while downloading](#play-while-downloading)).

### This computer and other devices

The token file's token is **this computer's** (the desktop app, `ottie open`, your scripts here): it can do everything. `ottie token --new` replaces it; the old one stops working at once.

Every other device (a phone, a laptop, a TV, a script elsewhere) should get a **token of its own**, from Settings → Connection or the command line (docs/decisions/0052):

```bash
ottie device add Phone              # prints its token and a sign-in link, this once
ottie device add TV --read-only     # it may look, play and copy links, nothing more
ottie device list
ottie device remove 2               # its requests are refused from now on
```

Or through the API, with this computer's token: `POST /access/devices` `{"name": "Phone", "readOnly": false}` answers `{"device": {…}, "token": "…"}`; `GET /access/devices` lists them, `DELETE /access/devices/{id}` removes one (its open event streams close). Ottie keeps only a hash of each device's token.

A device's token is refused (`403`) where only this computer may act: managing devices and the folders they may use (`ownerOnly`). A **read-only** one may only `GET`, ask what links a text holds (`POST /jobs/preview`), which category a link gets (`POST /categories/match`) and make play links (`POST /jobs/{id}/stream`); anything else answers `403 readOnly`.

A device may only have downloads written into **every category's folder and the folders listed** (`folderNotAllowed` otherwise, for `destDir` and a category's `folder`, and for the `destDir` of `POST /jobs/preview`, which looks for files already there): those in `folders` in `config.toml`, and those chosen in Settings (`PUT /access/folders` `{"folders": ["/srv/downloads"]}`, this computer only). This computer isn't limited. `GET /access` tells any token who it is (`device`: null for this computer), whether it's `readOnly`, and the folders listed.

## Conventions

- JSON, with `camelCase` field names. Send `Content-Type: application/json` with a body.
- **Job IDs** are strings like `2026.10.13-14.28.59-1`: the creation time in UTC plus a counter. Sorting them sorts by creation. Category and view IDs are numbers; General is category `1`.
- Sizes and speeds are in **bytes** (speeds in bytes per second); `0` means "no limit". Times are milliseconds since 1970 (UTC).
- Every speed limit goes up to 1 TiB/s (`1099511627776`) and every time up to the end of year 9999 (`253402300799999`); past that, `outOfRange` names the field. The settings' ranges, and `speedLimit` for the others, are in `shared/api-types/Ranges.ts`.
- Folders are **absolute paths on the daemon's computer**.
- Changes answer `204 No Content`; creations answer `201` with the new ID.
- Fields marked optional in a body may be left out. In `PATCH` bodies, a field left out stays as it is.

## A short tour

```bash
API=http://127.0.0.1:24068/api/v1
AUTH="Authorization: Bearer $(ottie token)"

# Add a download (to its category's folder; add "destDir" or "category" to choose)
curl -H "$AUTH" -H "Content-Type: application/json" \
     -d '{"url": "https://example.com/file.iso"}' $API/jobs
# → 201 {"id": "2026.10.13-14.28.59-1"}

# Every download, oldest first; or just this one
curl -H "$AUTH" $API/jobs
curl -H "$AUTH" $API/jobs/2026.10.13-14.28.59-1

# Pause it (answers once it has really stopped), then resume it
curl -H "$AUTH" -X POST $API/jobs/2026.10.13-14.28.59-1/pause
curl -H "$AUTH" -X POST $API/jobs/2026.10.13-14.28.59-1/resume

# Its own speed limit (1 MiB/s) and priority
curl -H "$AUTH" -H "Content-Type: application/json" -X PATCH \
     -d '{"speedLimit": 1048576, "priority": "high"}' $API/jobs/2026.10.13-14.28.59-1

# Move it (and any others listed) one place up in the queue; also "down", "top", "bottom",
# or next to another download: {"before": id} or {"after": id}
curl -H "$AUTH" -H "Content-Type: application/json" \
     -d '{"ids": ["2026.10.13-14.28.59-1"], "to": "up"}' $API/jobs/reorder

# Take it off the list (a finished file stays on disk)
curl -H "$AUTH" -X DELETE $API/jobs/2026.10.13-14.28.59-1

# A total limit for all downloads, and at most 3 at once
curl -H "$AUTH" -H "Content-Type: application/json" -X PATCH \
     -d '{"globalLimit": 5242880, "maxConcurrent": 3}' $API/settings

# The language every following device shows; null: each its own system's
curl -H "$AUTH" -H "Content-Type: application/json" -X PATCH      -d '{"language": "de"}' $API/settings
```

A download's `state` is one of `queued`, `connecting`, `downloading`, `paused`, `completed`, `failed`, `cancelled`, `retrying` and `scheduled`. Downloads wait in `queued` until a slot is free: the highest `priority` first, then the lowest `place` (new downloads go last; `POST /jobs/reorder` moves them among the queued, scheduled and paused ones of their priority). `failed` and `paused` downloads can be resumed. `completed` and `cancelled` are final. A `downloading` job with `waitingForBandwidth: true` is running but gets no bandwidth for now: the speed limits give it all to others (urgent ones first).

## Start and stop times

A download can wait for a start time and pause at a stop time (`startAt`, `stopAt`, in milliseconds since 1970, UTC), given when adding it (`POST /jobs`) or later (`PATCH /jobs/{id}`). Until its start time it's `scheduled`, then it joins the queue at its `place`; at its stop time it pauses if it hasn't finished (even if it never started). Each time is used once, then becomes `null`. The stop time must come after the start time (`stopBeforeStart`).

- A start time already past means now. In a `PATCH`, `null` takes a time away; a scheduled download then joins the queue at once, as it does when resumed (`POST /jobs/{id}/resume`).
- Given to a queued, paused, failed or retrying download, a start time makes it `scheduled`; a running one stops first (its `changed` event comes once it has).
- A completed or cancelled download takes no times (`invalidTransition`).

```bash
# Start at 23:00 on 2026-10-14 (UTC), stop at 07:00 the next morning
curl -H "$AUTH" -H "Content-Type: application/json"      -d '{"url": "https://example.com/file.iso", "startAt": 1792018800000, "stopAt": 1792047600000}' $API/jobs
```

## Time slots

Time slots say when downloads run, stop or slow down, by the daemon's own clock and time zone (`GET /slots` tells which, as `timeZone`). Each applies to every download (`category` null) or to one category, every week on some `days` or once on a `date`, from one time to another (`HH:MM`; an end before the start runs into the next day, the same time twice is a whole day). While it lasts it does any of:

- `downloads`: `"stop"` (running downloads stop, or with `letFinish` they finish and only new ones wait; when it ends, those it held carry on, or with `stayPaused` they're paused, to be resumed by hand), or `"allow"` (over a lower slot that stops them);
- `speedLimit`: the total limit (or the category's) in bytes per second, 0 for none;
- `maxConcurrent`: how many may run at once.

Where two apply to the same thing at once, the one higher in the list (lower `place`) wins, for each of the three. Outside every slot the usual settings apply; a slot never changes them. A download a slot stops is `scheduled`, with `heldBy` the slot's ID; resuming it (`POST /jobs/{id}/resume`) runs it anyway until it pauses, fails or ends. With `stayPaused`, the downloads it held become `paused` when it ends at its time, even if the daemon wasn't running then; switching the slot off or removing it lets them carry on instead. A slot that happens once is removed once it's over, and a category's slots go with it.

```bash
# Every night from 23:00 to 07:00, stop downloads
curl -H "$AUTH" -H "Content-Type: application/json"      -d '{"name": "Night", "days": ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"],
          "from": "23:00", "to": "07:00", "downloads": "stop"}' $API/slots
# → 201 {"id": 1}

# Weekdays 09:00-18:00, 2 MiB/s at most; then put it first in the list
curl -H "$AUTH" -H "Content-Type: application/json"      -d '{"name": "Work", "days": ["monday", "tuesday", "wednesday", "thursday", "friday"],
          "from": "09:00", "to": "18:00", "speedLimit": 2097152}' $API/slots
curl -H "$AUTH" -H "Content-Type: application/json" -d '{"ids": [2, 1]}' $API/slots/reorder
```

`PUT /slots/{id}` replaces one (switch it off with `"enabled": false`), `DELETE /slots/{id}` removes it. Every change, and every slot starting or stopping to apply, comes as a `slotChanged` event (`slotRemoved` when one goes).

## Several links at once

`POST /jobs/preview` finds the links in any text (one per line, an email, a web page) and says what adding each would do, without adding anything. `POST /jobs/batch` then adds several downloads in one request: each one as `POST /jobs` would, in order. Those that can be added are, even if others are refused; the answer gives each one's ID (or `null`) and why the others were refused.

```bash
curl -H "$AUTH" -H "Content-Type: application/json" \
     -d '{"text": "Get https://example.com/a.zip, then https://example.com/b.iso.\nftp://example.com/c"}' \
     $API/jobs/preview
# → {"links": [
#      {"kind": "link", "url": "https://example.com/a.zip", "name": "a.zip",
#       "matched": {"category": 1, "extension": "zip", "byFolder": false}, "known": false,
#       "exists": false},
#      {"kind": "link", "url": "https://example.com/b.iso", …},
#      {"kind": "invalid", "text": "sftp://example.com/c",
#       "refused": {"error": "…", "code": "unsupportedScheme", "scheme": "sftp"}}]}

curl -H "$AUTH" -H "Content-Type: application/json" \
     -d '{"jobs": [{"url": "https://example.com/a.zip"}, {"url": "https://example.com/b.iso", "category": 3}]}' \
     $API/jobs/batch
# → 200 {"ids": ["2026.10.14-09.12.30-1", null],
#        "refused": [{"index": 1, "error": "…", "code": "categoryNotFound", "id": 3}]}
```

`known: true` means a download of that same link is already on the list, and `exists: true` that a file with the name it suggests is already in the folder it would go to (see below). Both endpoints refuse more links than the setting `maxBatch` allows (10,000 by default) with `tooManyLinks`, adding none.

A link in the text can hold **sequences**, curl-style, and the preview lists every link they give: `[1-120]` (a leading zero pads: `[001-120]`), `[a-z]`, a step (`[0-100:10]`), a list (`{small,large}`, two items or more). Several in one link give every combination, the last changing fastest: `https://example.com/s[1-2]/{a,b}.jpg` gives `s1/a.jpg`, `s1/b.jpg`, `s2/a.jpg`, `s2/b.jpg`. Anything not well-formed stays as written (`http://[::1]/`, `a[]=1`, `{id}`), and `\[`, `\]`, `\{`, `\}` (and `\,` in a list) are the character itself. The cap counts a sequence as all the links it gives, before making any: `tooManyLinks` comes at once, with that count. Links that would add up to more than 64 MiB of text are refused the same way, with `max` then how many of them would fit. A range is at most 64 characters as written; a longer one stays text. `POST /jobs` and `POST /jobs/batch` take links as they are.

## A file already there

When a download's name is taken in its folder at the server's first response (by the name the server gives, which may differ from the link's), it does what its `existing` says: `"rename"` saves it under the first free name (`name (1).ext`), `"replace"` downloads it beside the file and puts it in its place once complete (a failed download leaves the file as it was), `"ask"` stops it before anything is written: it fails with the problem `{"code": "fileExists", "name": …}` until it's resumed with another answer. A download without one of its own follows the setting `existing` (`"rename"` by default).

```bash
# Replace the file if it's there
curl -H "$AUTH" -H "Content-Type: application/json" \
     -d '{"url": "https://example.com/file.iso", "existing": "replace"}' $API/jobs

# A download that stopped to ask: rename it after all, then resume it
curl -H "$AUTH" -H "Content-Type: application/json" -X PATCH \
     -d '{"existing": "rename"}' $API/jobs/2026.10.13-14.28.59-1
curl -H "$AUTH" -X POST $API/jobs/2026.10.13-14.28.59-1/resume
```

## FTP and FTPS

Links may also be `ftp://` (TLS when the server offers it, plain otherwise), `ftpes://` (TLS required, `AUTH TLS`) or `ftps://` (TLS from the first byte, port 990), as FileZilla names them (docs/decisions/0054). A user name and password go in the link (`ftp://me:secret@nas.local/films/a.mkv`; without one, anonymous); the daemon keeps the password and never gives it back: a job's `url` is `ftp://me@nas.local/films/a.mkv`. A link to a folder (ending in `/`) is refused as `invalidUrl`.

An FTP download can resume, and use several connections, when the server gives its size and supports `REST STREAM`; it compares the size and modification time again before resuming. An FTP server's refusal is the problem `{"code": "ftpReply", "status": 530, "detail": "Login incorrect."}` (530: user name or password, 550: no such file or no access, 421: busy or too many connections; 4xx are retried), and an `ftpes://` link to a server without TLS is `noTls`.

A certificate the system doesn't trust (a NAS or a router that made its own) stops the download with `{"code": "certificate", "host": "nas.local", "fingerprint": "e1bc…be63"}` (SHA-256, lowercase hex). Trusting that one certificate for that server, then resuming, lets it through; another certificate from the same server is refused again:

```sh
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"host": "nas.local", "fingerprint": "e1bc…be63"}' $API/certificates   # → 201 {"id": 1, …}
curl -H "Authorization: Bearer $TOKEN" -X POST $API/jobs/$ID/resume
curl -H "Authorization: Bearer $TOKEN" $API/certificates                       # every one trusted
curl -H "Authorization: Bearer $TOKEN" -X DELETE $API/certificates/1           # refused again
```

## Downloads from a browser

A client handing over a download the browser started (the extension, docs/decisions/0055) can send what the browser would have: `cookie` (the `Cookie` header for that link), `referrer` (the page it came from) and `userAgent` (the browser's, sent instead of Ottie's). They go with every request of that download, and no other; on a redirect to another server, the cookie is left out. The daemon keeps them with the download, never gives them back (no job or event has them) and never logs them. A value with a line break, or over 32 KiB, is refused as `{"code": "invalidHeader", "name": "cookie"}`.

A client that may send `POST /jobs` again when it gets no answer (a timeout, a lost connection: the download may have been added all the same) gives it a `requestId`, a random ID of 1 to 64 letters, digits, `-` or `_` (a UUID; else `invalidRequestId`), the same in every try (docs/decisions/0060). Sent again with the same one, by the same token, within 10 minutes, it answers `201` with the first one's ID instead of adding another; while the first is still being added, it waits for it. Only an added download is remembered: a refused request is checked again. The daemon keeps them in memory, so a restart forgets them. `POST /jobs/batch` ignores it.

```sh
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"url": "https://example.com/report.pdf", "cookie": "session=4f1c…",
       "referrer": "https://example.com/reports", "userAgent": "Mozilla/5.0 …"}' $API/jobs
```

## When saving fails

While Ottie can't write to its database (a full disk, a drive gone), `storageFailing` in `GET /settings` and in every `settingsChanged` holds the last error's English text, and it's `null` again once writing works. Downloads' progress made meanwhile may be lost when Ottie stops: show it, as the web UI does with a banner. A request that changes something (adding or removing downloads, settings, categories, views, time slots, certificates) is answered only once it's saved: if it can't be, it's refused with `500 storageFailed` and nothing changed.

## The first-start setup

Until `setupDone` is true in `GET /settings`, front ends offer the setup. It comes in steps (`setupStep`, in this order: `look`, `downloads`, `categories`, then the desktop app's `startup`, `clipboard`, `notifications`); a front end shows those it has. Going past one, done or skipped, saves it with the settings it changes, so a setup stopped halfway carries on after it in whichever front end opens next. Ending it sets `setupDone` and clears the step.

```bash
# Past the downloads step, with what it changed
curl -H "$AUTH" -H "Content-Type: application/json" -X PATCH \
     -d '{"maxConcurrent": 4, "existing": "replace", "setupStep": "downloads"}' $API/settings
# Done
curl -H "$AUTH" -H "Content-Type: application/json" -X PATCH \
     -d '{"setupDone": true, "setupStep": null}' $API/settings
```

## Live events

`GET /api/v1/events` is a WebSocket that pushes every change as a JSON message. Browsers can't set headers on a WebSocket, so the token travels as an offered **protocol**: offer both `ottie.v1` and `ottie.token.<token>`, and the server picks `ottie.v1`.

```js
const socket = new WebSocket('ws://127.0.0.1:24068/api/v1/events', [
	'ottie.v1',
	`ottie.token.${token}`
]);
```

To keep an up-to-date copy of the list:

1. **Connect first**, then fetch `GET /jobs` (and `/categories`, `/views`, `/settings` if you show them). Connecting first means no change can slip between the two.
2. Apply each message in order. The `type` field says what it is:

| `type` | Fields | What to do |
|---|---|---|
| `changed` | `job` | Insert or replace the job with that `id` |
| `progress` | `id`, `downloaded`, `total`, `ranges` | Update its byte counts (a few times a second while it runs) |
| `removed` | `id` | Drop the job |
| `categoryChanged` / `categoryRemoved` | `category` / `id` | The same, for categories |
| `viewChanged` / `viewRemoved` | `view` / `id` | The same, for saved views |
| `slotChanged` / `slotRemoved` | `slot` / `id` | The same, for time slots (also when one starts or stops applying) |
| `settingsChanged` | `settings` | Replace the settings: all of them, as they are now |
| `resync` | | You fell behind and missed events: fetch the lists again |

3. If the socket closes, reconnect and start again at step 1.

`ranges` lists, for a download over several connections, each connection's part of the file (`start`, exclusive `end`, and `done` bytes from `start`). It's empty for one connection.

For a speed graph, `GET /speed` gives the total download speed over the last hour, kept since the daemon started: `{"samples": [...]}`, all downloads' speed together in bytes per second, one sample per second, oldest first, at most 3600. Each download's speed is estimated from its progress and smoothed (a moving average), as Ottie's pages do; fetch it with the lists, then add your own samples from the `progress` events.

## Errors

A refusal answers a status and a JSON body with English text and a **code**:

```json
{ "error": "no job with ID 2026.10.13-14.28.59-9", "code": "jobNotFound", "id": "2026.10.13-14.28.59-9" }
```

Show users a message chosen by `code` (translated, with the extra fields filled in), and fall back to `error` for codes you don't know: a newer daemon may add some. Every code is listed under the `Reason` schema in `openapi.json`, and each endpoint lists the ones it can answer. In short:

| Status | Means | Examples |
|---|---|---|
| 400 | Something in the request is wrong | `invalidUrl`, `relativeFolder`, `outOfRange`; `invalidPath` (`/categories/abc`: a number ID that isn't one); `invalidBody` (not JSON at all) |
| 401 | No token, or the wrong one | `unauthorized` |
| 404 | No such thing | `jobNotFound`, `categoryNotFound`, `noSuchEndpoint` |
| 409 | Not possible right now | `invalidTransition` (resuming a finished download), `jobMoving` |
| 413 | The body is too large (2 MB; 64 MB for `POST /jobs/batch` and `/jobs/preview`) | `invalidBody` |
| 415 | The body isn't sent as `Content-Type: application/json` | `invalidBody` |
| 422 | The body isn't the JSON expected | `invalidBody` |
| 503 | The daemon is shutting down | `stopping` |

When a download fails, the job itself says why: `error` in English, and `problem` as a code (`network`, `httpStatus`, `stalled`, `ftpReply`, `certificate`…) to translate the same way.

## Play while downloading

Players such as VLC, mpv or a browser's `<video>` can't send the token. So a download, finished or not, gets a **play link** of its own:

```bash
curl -H "$AUTH" -X POST $API/jobs/2026.10.13-14.28.59-1/stream
# → {"url": "/api/v1/stream/9f2c…e1/movie.mp4"}
vlc "http://127.0.0.1:24068/api/v1/stream/9f2c…e1/movie.mp4"
```

- The link needs **no token**: the random ticket in it is the permission. It reads that one file, nothing else. Anyone who has it can play the file, so share it only as you would the file itself.
- It expires after **12 hours unused**, and when the daemon restarts. The same download keeps the same link while it's in use.
- `Range` requests work as usual (`206`, or `416` past the end) when the size is known. If the server never said the size, the whole file is sent as it arrives.
- While a link is being read, a download over several connections goes where it reads (a seek included) and fetches its last 1 MiB early, where many videos keep their index.
- A download in **streaming mode** (`"streaming": true` on the job) does that from the start, as if a player read it: its connections take the file in order, one piece each, and its last 1 MiB comes first. Set it when adding (`"streaming"` in `POST /jobs`) or later (`PATCH /jobs/{id}`). Left out when adding, it's on for videos and music if the settings' `streamMedia` is (the default), going by the link's extension, then by the file's real name once the server answers. A download over one connection already comes in order.
- A read past what has arrived **waits** for the bytes, even while the download is queued or paused. The answer breaks off if the download fails, is cancelled or removed, or has to start over because the file changed on the server.

## Versions

The `v1` in the path changes only if the API changes in a way that breaks existing clients. New endpoints, new fields and new codes can appear at any time, so ignore fields and codes you don't know rather than failing on them.
