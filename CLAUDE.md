# CLAUDE.md

**Ottie**: a cross-platform download manager meant to replace uGet (unmaintained). Licensed MPL-2.0. Repo: `github.com/MatthiewD/ottie`.

## Naming

| Thing | Name |
|---|---|
| Product | Ottie. Store and extension listings: "Ottie – Download Manager" |
| Mascot / tagline | An otter. Sea otters stash food and a favourite stone in a skin pouch, so Ottie "tucks your files away" |
| Command / binary | `ottie` (one binary; `ottie serve` runs the daemon) |
| Cargo packages | `ottie` (the binary), `ottie-core`, `ottie-api`, `ottie-app` (Tauri) |
| App identifier (Tauri, = Android package) | `io.github.matfalco.ottie`. **Never change it after the first Android build**: a new identifier is a different app with no upgrade path |
| Tauri | `productName` "Ottie", window title "Ottie" |
| Config/data folders | `ottie` on Linux (`~/.config/ottie`, `~/.local/share/ottie`), `Ottie` on Windows/macOS |
| Environment variables | `OTTIE_` prefix |
| systemd unit / Docker image | `ottie.service` (`ExecStart=ottie serve`) / `ottie` |

## What we're building

- A background **daemon** that does all the downloading, with three front ends on top: a **web UI**, a **desktop app** (Windows, Linux, later macOS) and an **Android app**. iOS is out of scope.
- A **browser extension** (Chromium-based browsers and Firefox, desktop) that takes over downloads from the browser: intercepts them, hides the browser's own download UI where possible, and shows our download list in its popup.
- Must-haves: pause/resume (including after a restart or crash), multi-connection downloads, and speed limits at three levels (per download, per category, global), adjustable while downloads run.
- **Play while downloading** (uGet can): a short-lived play link (`POST /jobs/{id}/stream` → `GET /stream/{ticket}/{name}`, no token, range support) serves a partial file, so the browser, VLC/mpv or an Android player can play a video before it finishes, locally or from a remote daemon (`docs/decisions/0029`). The download order follows the player and, in a per-download streaming mode, stays in order and fetches the last chunk early (many MP4s keep their index at the end).
- **Translated UI:** in the system's language by default (a device or every device can choose another), English as the fallback. English and French, then languages in batches (EU, Latin America, Asia, Africa; `docs/decisions/0041`): de, es-ES, es-419, it, nl, pl, pt-PT, pt-BR, then the rest of the EU in Latin script (sv, ro, cs, hu, da, fi, sk, hr, sl, lt, lv, et, ga, mt), Greek and Cyrillic (el, bg, uk), Asia (zh-Hans, zh-Hant, ja, ko, id, hi, vi, tr), Africa (sw, am, ha, yo, zu), and right to left, with the whole page mirrored (ar, fa, ur; `docs/decisions/0042`). Each is one JSON file in `ui/messages/` and, for the tray, one in `app/messages/`; daemon messages reach clients as codes they translate. The `ottie` command line and logs stay English.
- uGet features to bring over time: categories (folder + file-type rules), queue with max concurrent downloads, opt-in clipboard monitoring (desktop app, `docs/decisions/0043`), batch/URL-list import, scheduler, FTP and FTPS (downloads only, `docs/decisions/0054`).

## Stack

| Part | Tech |
|---|---|
| Download engine, daemon, API | Rust (edition 2024), tokio, reqwest 0.13, axum, rusqlite (`bundled`), utoipa (OpenAPI) |
| UI (shared by web, desktop, Android) | Svelte 5 + TypeScript + Vite, static build |
| Desktop + Android shell | Tauri 2 |
| Browser extension | TypeScript, WebExtension Manifest V3 |
| JS tooling | pnpm workspaces |

Why: one Rust engine compiles for every target including Android; one web frontend is reused by the web UI, the desktop app and the Android app; the extension and the UI share a language.

reqwest runs with `default-features = false` and features `rustls` (aws-lc-rs crypto, OS certificate store), `http2`, `system-proxy`. See `docs/decisions/0002`. FTP and FTPS go through `suppaftp` (tokio, rustls with aws-lc-rs, `no-log`: it would log the password; `docs/decisions/0054`).

## Architecture

```
browser extension ──HTTP──┐
web UI (browser) ─────────┤
desktop app (Tauri) ──────┼──► crates/api (REST + WebSocket) ──► crates/core (engine)
Android app (Tauri) ──────┘    (on Android, core + api run inside the app)
```

- **The HTTP API is the only contract.** Every client talks to the engine through it. It's documented in `docs/api/` (`docs/decisions/0030`): a guide, and `openapi.json` generated from the handlers. Tauri commands are only for OS integration (tray, notifications, file pickers, open folder, autostart, the player), never for download operations. The app's own commands are listed in `app/build.rs` and granted to the daemon's pages in `PAGE_PERMISSIONS`; a page never passes a program or path to run.
- **One binary:** `ottie serve` runs the daemon (core + api). Of the other subcommands, `ottie device` is a client of the running daemon's HTTP API; the rest do this computer's side (the token file, a signed-in link, the service, the extension's native host). See `docs/decisions/0004`.
- **Desktop** (`docs/decisions/0017`): the daemon is its own process. The app starts it as a Tauri sidecar (`ottie serve --managed`: prints `ready`, stops when its stdin closes; started again if it stops unasked) unless one already answers (a terminal, or installed as a service), and its window shows the daemon's own page, signed in via `ottie link`. Closing the window hides it to the tray unless a setting says quit (`docs/decisions/0034`). At sign-in (`0039`) the system starts the app with `--autostart`: hidden in the tray, or, per `app.json`, it only starts `ottie serve` on its own (log in the app's log folder) and quits. A right click on the tray icon opens Ottie's own menu, the daemon's `/tray` page in a small borderless window; the app also follows the daemon itself (events WebSocket + the usual endpoints) for the system's menu (Linux, daemon unreachable), the tooltip and notifications (finished: all at once by default, or each, or never; failed: a switch; `docs/decisions/0035`; with their sounds: the system's, silent, a set of Ottie's or one for each kind, `0038`), with its own texts in `app/messages/`. When everything has finished (`0040`) it can quit, or put the computer to sleep, hibernate it or shut it down after a countdown (a small window on the daemon's `/finish` page, or a notification), chosen every time in Settings or for once from the tray menu or the Downloads page. It can watch the clipboard (`0043`, opt-in): a link copied elsewhere to a file type a category lists is offered in a small window that never takes the focus (the daemon's `/caught` page; Download, More… opens the main window's list, Ignore), or added at once with a notification. The window can also show another computer's Ottie (`0053`, a NAS, a server): a list in `daemons.json`, each token in the system's keychain, switched from beside the logo or Settings → Connection; such a page gets fewer app commands (`REMOTE_PERMISSIONS`: nothing that reaches this computer's files, programs, settings or power), and the tray, notifications and copied links follow the one shown, while "when everything has finished" stays with this computer's, which still starts with the app unless a setting says only once shown.
- **Android:** no separate daemon. The app embeds `core` + `api` and serves the API on 127.0.0.1, so the UI code is unchanged.
- **Other devices** (`docs/decisions/0052`): each gets a token of its own (full or read-only), and may only have downloads written into categories' folders and those listed; the token file's is this computer's, which alone manages them. In a browser, each address keeps its own list of other computers' Ottie and switching goes to that one's own page (`0053`), so a page always matches its daemon's version.
- **Headless:** the daemon runs standalone and serves the built web UI itself: `ottie service install` sets it up for you (systemd user unit, a task at sign-in, LaunchAgent) or, with `--system`, for the computer under an account of its own (systemd unit, Windows service); or the Docker image (`docs/decisions/0051`, `docs/service.md`).

## Repo layout

Library packages are prefixed `ottie-` (a package named `core` would clash with Rust's built-in crate). See `docs/decisions/0003`.

```
crates/core/       ottie-core: engine library. No HTTP server, no UI.       [exists]
  src/manager.rs     Manager handle + the actor task that owns all jobs, queue
  src/job.rs         JobState + state machine (can_become), JobSnapshot, JobEvent, Reorder
  src/id.rs          JobId (yyyy.mm.dd-hh.mm.ss-N, UTC) and its generator
  src/category.rs    CategoryId (General = 1), CategorySettings, extension matching
  src/backend.rs     Backend trait, HttpBackend, NativeBackend (HTTP(S) or FTP by the link; what `ottie serve` uses)
  src/store.rs       SQLite on its own thread: schema migrations, save/load jobs and settings
  src/settings.rs    Settings, RetryPolicy, Appearance, language, stream_media, max_batch, existing, setup_step;
                     SettingsChange (applied all at once)
  src/view.rs        saved views: ViewSettings (filters + sort/grouping), ViewId, normalizing
  src/webmark.rs     the Mark of the Web on finished files (Windows' Zone.Identifier stream, 0045)
  src/slot.rs        time slots: SlotSettings (target, days or date, hours, effects), whether one covers
                     a moment and its next change (jiff, the system's time zone), Effects (what the
                     active ones change, highest first)
  src/links.rs       the links in pasted text (find), LinkPreview: for adding several at once
  src/sequence.rs    sequences in one link, curl-style ([001-120], [a-z], {a,b}): count, then expand
  src/throttle.rs    speed limits: per-job token buckets, refill task, priority-weighted sharing, category caps
  src/speed.rs       the last hour of total speed, one sample per second, from each job's smoothed rate (0044)
  src/http.rs        HTTP transfer: first request, resume, restart rules, an HTTP range for segmented.rs
  src/ftp.rs         FTP and FTPS transfer (0054): the link (TLS as ftp/ftpes/ftps ask, account, path), the certificate
                     check (the system's, else trusted by hand), login, SIZE/MDTM/REST, a range per login
  src/transfer.rs    what both share: Body (an answer's bytes), Source (where a multi-connection run's ranges come
                     from), naming the file at the first answer, one connection's loop, completing
  src/segmented.rs   multi-connection transfer: coordinator, connection tasks, dynamic splitting (HTTP or FTP)
  src/certificate.rs certificates trusted by hand for FTPS servers (0054): TrustedCertificate, a host + SHA-256
  src/download.rs    Download state (what gets persisted), Placement, Event, Outcome
  src/problem.rs     Problem: why a job failed, as a kind + details (English via Display)
  src/stream.rs      Reader: a job's bytes at any offset, waiting until they're on disk (play links)
  src/playback.rs    where players read each job (Playback, one head per reader) and whether it's in
                     streaming mode, for segmented.rs; which extensions are media
  src/rename.rs      a rename that never replaces a file (0049): the system's call (MoveFileExW, renameat2,
                     renamex_np), else a hard link, else check then rename
  src/filename.rs    names from URL / Content-Disposition, sanitizing, collision-free names, .part.ottie → final rename,
                     Existing (a name taken: rename, replace, ask; 0049),
                     relocate (a job's file into another folder; copy + delete between drives)
  tests/support/     in-process axum test server with fault injection; ftp.rs: an FTP server (plain, explicit or
                     implicit TLS with the self-signed certificate beside it) with faults, for tests/ftp.rs
  examples/fetch.rs  manual CLI try-out
crates/api/        ottie-api: axum router under /api/v1 (static UI serving from 3b)  [exists]
  src/lib.rs         router(manager, token, shutdown), AppState, JSON 404 fallback; routes come
                     from each handler's #[utoipa::path] (utoipa-axum), openapi()
  src/openapi.rs     the OpenAPI description: info, token scheme, 401/503 everywhere,
                     GET /openapi.json; its test writes docs/api/openapi.json
  src/auth.rs        Token, Bearer / WebSocket-protocol check (constant-time), what a read-only token may do
  src/access.rs      who is asking (0052): this computer (token file, read again when it changes) or a device
                     (access.json: names, SHA-256 of tokens, read-only); Caller (extractor); the folders a device
                     may write to; GET /access, PUT /access/folders, GET/POST /access/devices, DELETE /access/devices/{id}
  src/types.rs       wire types (camelCase JSON) + ts-rs export, From<engine types>
  src/jobs.rs        GET/POST /jobs (a `requestId` sent again answers the first's ID: retried.rs, 0060),
                     GET /jobs/{id}, POST /jobs/{id}/pause|resume|cancel|category,
                     PATCH /jobs/{id} (own speed limit, priority, streaming mode, start/stop time),
                     DELETE /jobs/{id},
                     POST /jobs/reorder (move waiting jobs in the queue), POST /jobs/batch (several at
                     once), POST /jobs/preview (the links in some text; both take bodies up to 64 MB)
  src/categories.rs  GET/POST /categories, PUT/DELETE /categories/{id}, POST /categories/match
  src/settings.rs    GET/PATCH /settings: max concurrent, global limit, retry policy, shared
                     appearance + language, setupDone + setupStep (0050), streamMedia, maxBatch, existing; the ranges people may choose (its tests
                     write them to shared/api-types/Ranges.ts)
  src/views.rs       GET/POST /views, PUT/DELETE /views/{id} (saved views)
  src/certificates.rs GET/POST /certificates, DELETE /certificates/{id}: FTPS certificates trusted by hand (0054)
  src/slots.rs       GET/POST /slots (with the time zone), PUT/DELETE /slots/{id}, POST /slots/reorder
  src/speed.rs       GET /speed: the last hour of total speed, for the graph (0044)
  src/stream.rs      play links: POST /jobs/{id}/stream → GET /stream/{ticket}/{name} (no token, Range)
  src/retried.rs     `POST /jobs` sent again (0060): each token's `requestId`s of the last 10 minutes (10,000 at most)
                     and the job each added, in memory
  src/identity.rs    POST /identity (0058, no token): an HMAC of the client's nonce keyed with this computer's token;
                     nonce / proof / proven for clients (`ottie`)
  src/events.rs      GET /events WebSocket: changed / progress / removed / categoryChanged /
                     categoryRemoved / settingsChanged / viewChanged / viewRemoved / slotChanged /
                     slotRemoved / resync
  src/error.rs       engine Error → status + {"error": "…"}
  src/ui.rs          the web UI: rust-embed of ui-dist/ (from disk in debug), --ui-dir, SPA fallback; pages may be
                     framed only by its own and extensions' pages (`frame-ancestors`, 0055)
  ui-dist/           built UI (git-ignored, but packaged for crates.io)
  tests/api.rs       end to end: real server + small file server, reqwest + tungstenite
crates/ottie/      ottie: the binary  [exists]
  src/main.rs        the subcommands; its own tokio runtime (a Windows service hands its thread to the system first)
  src/serve.rs       `ottie serve`: logging (or a log file), folders, token, the data folder's lock (one daemon each, 0008),
                     database, General → Downloads folder
                     (or --downloads-dir), axum::serve, graceful stop (also the Windows service's Stop): the downloads
                     stop at once while the API drains, 5 s at most
  src/open.rs        `ottie open`: browser on the UI, signed in (via a private redirect page); `ottie link`
  src/config.rs      folders (OS defaults / flags / OTTIE_* env / --system: the system service's), config.toml,
                     listen address
  src/token.rs       token file: created on first use (never for --system), whole at once (written beside, then linked
                     into place; `--new` renamed over it), `ottie token` prints it (`--new` replaces it)
  src/device.rs      `ottie device add|list|remove` (0052): a client of the running daemon, with this computer's token,
                     sent only once what answers has proved it's this computer's Ottie (`check_identity`, 0058; `ottie open` too)
  src/native_host.rs the browser extension's native messaging host (0055): a browser starts `ottie` with the
                     extension's address (recognised before clap); one length-prefixed JSON message each way:
                     `pair` (starts Ottie if needed, adds a device named after the browser, gives its token) or `start`
  src/extension.rs   `ottie extension install|uninstall|status` (0055): the host's manifest for each browser found
                     (a folder of its own on Linux/macOS; on Windows a manifest in the data folder named by an HKCU
                     key, through reg.exe), written only when it changed; the Chromium IDs it allows
  src/service.rs     `ottie service install|uninstall|status` (0051): what to run, whether Ottie answers, the
                     system service's copy of ottie; per system in service/: systemd.rs (user or system unit,
                     the `ottie` account, lingering), launchd.rs (LaunchAgent), windows.rs (task at sign-in,
                     Windows service + running as one, the console let go of). Their texts are built and
                     tested on every system; installing them only on their own
  tests/cli.rs       runs the real binary
app/               ottie-app: Tauri 2 shell (desktop; Android later, then with core + api)  [exists]
  src/main.rs        frameless window on the daemon's page (it draws its own title bar, 0019),
                     single instance, closing hides it (or quits, per app.json), stops the daemon it
                     started on exit
  src/identity.rs    whether what answers on Ottie's port is this computer's Ottie: its proof of the token (0058)
  src/daemon.rs      find or start `ottie serve --managed` (the sidecar next to the app; one that answers counts only if
                     it proves itself, 0058), ready / stop, its exit heard (remote.rs starts it again: 1 s, 5 s, 30 s); at
                     sign-in, `ottie serve` on its own; at every start `ottie extension install --quiet` (0055)
  src/client.rs      the app as an API client: events WebSocket, jobs, settings, pause/resume, limit,
                     categories' file types, links in a text (preview), adding several, where a
                     job's file is, whether it answers and takes the token; HTTPS with the system's certificates
  src/remote.rs      Ottie on other computers (0053): the list (daemons.json), tokens in the keychain (keyring),
                     sign-in links, this computer's daemon started once, the one shown (a watch the tray,
                     clipboard and status follow), switching; commands daemons / switch_daemon / add_daemon /
                     remove_daemon, the pages hear changes as the `daemons` event
  src/status.rs      follows the events (reconnects; an event older than the list fetched then never takes a download
                     back out of its end: `can_follow`), sums them up once a second for the tray (counts, speed, progress of those of known size),
                     and passes downloads that finished or failed to notify.rs: this computer's daemon always (for
                     when everything has finished), another's while shown (0053)
  src/notify.rs      notifications (0035): what to tell (each, all at once, failed), with which sound, a click
                     opens the window
  src/sound.rs       notification sounds (0038): the system's, silent, one of Ottie's sets (sounds/, played with
                     rodio) or one for each kind
  src/tray.rs        tray icon (white/black by taskbar, size by scale), clicks, the system's menu and its
                     actions (Linux, or while the daemon is unreachable) (0034)
  src/autostart.rs   starting at sign-in (0039): the system's entry (auto-launch, path quoted;
                     rewritten at start only if it starts another path: security tools pause an app that
                     rewrites it each time), what it starts
                     (tray or daemon only), `--autostart`
  src/popup.rs       Ottie's own tray menu: the /tray page in a borderless window beside the click, hidden
                     on blur; commands menu_ready / show_main / quit_app
  src/finish.rs      when everything has finished (0040): what happens (every time, for once), when the last
                     download ends (fed by status.rs), the countdown (its window on /finish, or a notification),
                     Cancel / now; commands finish / set_finish / set_finish_once / set_countdown /
                     set_countdown_in / set_finish_waits / cancel_finish / finish_now / countdown_ready; the
                     pages hear changes as the `finish` event. OTTIE_POWER_DRY_RUN: the countdown only prints
  src/clipboard.rs   clipboard monitoring (0043): a thread looks twice a second (arboard; on Windows only
                     after the change counter moves, never what password managers mark private), the daemon
                     says which links count, then the window on /caught (in the corner nearest the tray
                     icon, not focusable) or added at once; commands caught / caught_ready / caught_done /
                     caught_more, events `caught` (its page) and `add-links` (the main window)
  src/power.rs       sleep, hibernate, shut down and whether the computer can: the system's calls (windows-sys on
                     Windows, logind via systemctl/busctl on Linux)
  src/texts.rs       the tray's and notifications' texts from messages/<tag>.json (the page's tag, else
                     the bare language), speeds, counts and failure reasons per language; right-to-left
                     ones embedded as such (0042)
  src/prefs.rs       app.json: close to tray, notifications and their sounds, what sign-in starts, what happens
                     when everything has finished, watching the clipboard and what a copied link does, whether
                     this computer's daemon always starts (0053), the page's language, last total limit; commands
                     desktop / set_startup / set_close_to_tray / set_watch_clipboard / set_copied /
                     set_local_daemon / set_notify / set_notify_failed / set_sounds / set_sound / play_sound /
                     set_language; the pages hear changes as the `desktop` event
  src/config.rs      load/save the app's JSON settings files in its config folder (written beside, synced, then renamed
                     over; one it can't read is kept as `<name>.bad` before the defaults apply)
  src/files.rs       a download's file and folder (0045): commands open_file / show_in_folder take a job
                     ID, ask the daemon where it is; a program asks first in a native dialog
  src/player.rs      "Play with" (0031): system's player via a .m3u, browser, or a picked program;
                     saved in the app's config folder; commands player / set_play_with / choose_player
  messages/          the tray's and notifications' texts, one JSON file per language
  sounds/            Ottie's notification sound sets (Ogg), built into the app; README.md: how each was made,
                     its licence, and how loud a new one should be
  sounds/tools/      Python scripts that made them (synthesis, CC0 sources, loudness, a local page to choose
                     pairs by ear, Ogg export); tools for the maintainer, not part of the build
  build.rs           lists the app's commands, so Tauri generates their permissions (allow-…)
  tauri.conf.json    product, identifier, icons; tauri.bundle.json adds the sidecar and macOS' ad-hoc signature
                     for installers only
  scripts/          sidecar.mjs (UI + release ottie → binaries/), ico.mjs (icons/icon.ico from res/icons)
  package.json       pnpm package `app`: the Tauri CLI
ui/                SvelteKit SPA shared by all front ends (pnpm package `ui`)  [exists]
  src/hooks.client.ts  which language the page shows: Paraglide strategies that save nothing (0041)
  src/routes/        +layout.svelte (fonts, app.css, theme, favicon, token → Login | Shell; /tray, /finish
                     and /caught without),
                     +layout.ts (ssr = false), +page.svelte (Dashboard), downloads/+page.svelte
                     (Downloads, ?status= filter), categories/+page.svelte, schedule/+page.svelte,
                     settings/+page.svelte, tray/+page.svelte (the desktop app's tray menu: TrayMenu),
                     popup/+page.svelte (the browser extension's popup, in its frame: Popup; the token handed
                     over by postMessage, 0055),
                     finish/+page.svelte (its countdown window: Countdown), caught/+page.svelte (its
                     window for links copied elsewhere: Caught)
  src/lib/api.ts     fetch + WebSocket client for /api/v1 (Unauthorized, ApiFailure)
  src/lib/session.svelte.ts  live job list: events WebSocket, reconnect, buffered resync (a snapshot from before a
                     reconnect is fetched again; a failed one is tried again, 1 s up to 10 s; session.test.ts)
  src/lib/context.ts what every page shares (session, run): Svelte createContext
  src/lib/jobs.ts, categories.ts, status.ts, selection.ts (clicks, Shift ranges, the arrows' cursor), filetypes.ts,
                     bulk.ts (actions on the selection; what Pause all / Resume all act on),
                     opening.ts (which download a double-click or Enter opens, why one didn't, 0045),
                     speed.ts (the speed history: the graph's spans, 5 min to 1 h, saved per device, 0044),
                     filters.ts (Downloads filters ↔ address, quick filters, the four status groups),
                     arrange.ts (sort + group by status, saved per device),
                     details.ts (the selected download's panel: main action, facts, priorities),
                     filemap.ts (ranges → the bar that maps the file, in the details and each row, 0027),
                     hint.ts (where an (i)'s explanation goes: above its button, inside the window),
                     views.ts (saved views ↔ filters + arrangement, which pill is lit),
                     languages.ts (each language's name, which are reviewed, the system's closest, the
                     device's choice, 0041; which way it's written, 0042), format.ts, token.ts (also links handed over after `#`, 0055), adding.ts (the add step), setup.ts (the setup's steps, where
                     it carries on, starter categories; 0050), choices.ts (choices Settings and the setup share),
                     theme.ts (light/dark/system + palette → theme (0044, 0047),
                     saved per device),
                     settings.ts (settings fields: kB/MB/GB per second, seconds, ranges; last limit
                     per device), sync.ts (following the shared look + language),
                     playlist.ts (a play link as a .m3u, saved through the browser, 0031),
                     tray.ts (the tray menu's status line and its running downloads),
                     queue.ts (queue order, the waiting ones in it, whether up/down would move anything, what a drag
                     carries and where it may drop, 0032),
                     startup.ts (the sign-in choices, for Settings and Setup, 0039),
                     schedule.ts (a download's start and stop time: date-and-time fields ↔
                     milliseconds, "today at 23:00", 0040),
                     slots.ts (time slots: their order, in words, the week view's blocks and drags, conflicts,
                     what applies to all downloads now, the editor's draft ↔ body, 0040),
                     batch.ts (several links at once: when a paste opens the list, its rows, what to
                     add, what was refused, 0036), sounds.ts (the notification sounds' choices: sets, or one for each kind, 0038),
                     finish.ts (when everything has finished: the choices in words, what this computer
                     can't do, the countdown's text, 0040),
                     caught.ts (links copied elsewhere: the window's rows and title, how long it stays,
                     what was added or refused, 0043),
                     access.ts (devices and the folders they may use, as Settings shows them, 0052),
                     certificates.ts (FTPS certificates: a fingerprint in pairs, the one a download stopped at, 0054),
                     popup.ts (the extension's popup: which downloads it lists, its messages with the extension, 0055),
                     others.ts (Ottie on other computers: sign-in links, the switcher's rows, a browser's list, 0053;
                     others.svelte.ts: that list live, the app's or this browser's),
                     shortcuts.ts (keyboard shortcuts: what a key press means on every page and in
                     Downloads, whether the focused control needs that key, what a paste holds, the
                     list of them with key names per language, 0046)
                     plain logic, unit-tested (*.test.ts, Vitest)
  src/lib/motion.ts  animations (0048): on or off (device's choice, else the system's), timings, what a
                     list's rows do after a change (plan); moving.svelte.ts: the transitions (pop, drop,
                     unfold, leave), shuffle (rows slide, data-flip), resizes (panels), swaps (one
                     element replacing another), reshapes (the pills' track)
  src/lib/appearance.svelte.ts  the appearance chosen on this device (state shared by layout, shell
                     (follows the shared one, docs/decisions/0022) and Settings)
  src/lib/window.ts  the desktop app: its window (minimize/maximize/close), the folder picker,
                     a download's file and folder (0045), and how Play opens a player (the app's commands, 0031), and what happens when
                     everything has finished (0040), and the clipboard's links (0043)
  src/lib/finish.svelte.ts  that, live in a page: asked once, then the app's `finish` events, the countdown's clock
  src/lib/addrequest.svelte.ts  what a shortcut asks of the add bar (a pasted link, or the cursor in its field)
  src/lib/explain.ts   daemon codes (Problem, Reason) → the page's language; English fallback
  src/lib/testing.ts   test helpers (sample job)
  src/lib/components/  Shell (TitleBar with NavMenu tabs + SpeedChip (limit popup: LimitControl + RatePresets), page,
                       dialog, BatchDialog for links sent by More… in the clipboard's window or pasted on a
                       page; the keys of every page and ShortcutsDialog on "?" (ShortcutList); Setup on first
                       start or run again from Settings: one step at a time under a bar of dots (language and look:
                       LookChoices; downloads; categories; in the desktop app starting and closing, clipboard,
                       notifications), each skippable, 0050),
                       Dashboard (AddCard, SpeedChart across the top with its span, then the four groups two
                       by two: running rows filling up, waiting ones in queue order, JobSummary rows),
                       Downloads (its keys, 0046; AddCard, then one pane: Toolbar (Pause all / Resume all) + MoveMenu + FinishMenu (desktop app), QuickFilters (with the
                       saved views' pills + their menu) or FilterPanel, ViewDialog (name a view),
                       JobTable (group rows, sortable headers, drag in the Waiting group) + JobRow
                       (a double-click opens a finished file, desktop app),
                       JobDetails under it for the one selected download (header bar with Open and Show
                       in folder in the desktop app; its facts, priority and
                       limit beside the bar that maps the file (FileBar, as in each row); a strip for category,
                       folder, streaming, start and stop; at most half the pane, giving way to the list's
                       few rows), or DetailsEmpty in its place for none or several, one line),
                       Hint (an (i) beside a setting: its explanation on hover, click or tap),
                       CategoriesPage (LinkTester, rows, CategoryEditor inline in place of a row),
                       SchedulePage (WeekView: drag a slot or its edges, click to edit; then the
                       slots' rows with SlotEditor inline),
                       SettingsPage (section list, Run the setup again; appearance + sync, LookChoices, language:
                       LanguagePicker; downloads (Stepper for how many at once);
                       desktop app (sign-in, closing), clipboard, notifications + playing (desktop app only); retries;
                       keyboard shortcuts (ShortcutList); connection: AccessSettings (other devices, their sign-in
                       links, the folders they may use; a device sees which it is, 0052), OthersSettings (Ottie on other
                       computers, 0053) and CertificatesSettings (FTPS certificates trusted by hand, once there's one,
                       0054)), Switch, NumberField, LimitControl,
                       TitleBar (+ WindowControls; Switcher beside the logo once there's another computer's Ottie, 0053;
                       folds to fit: tab icons, slot chip, logo and switcher names), AddCard (AddBar: link, then category + folder step;
                       BatchDialog: several links, from a paste, the list button or a file),
                       FileIcon, ConfirmDialog, Login, TrayMenu (the desktop app's tray menu, its RunningList), Countdown (its
                       countdown window before sleep, hibernate or shut down), Caught (its window for links
                       copied elsewhere)
  messages/<tag>.json  every UI text, per language (Paraglide); project.inlang/ its settings (the locales)
  src/app.css        the four themes as variables under [data-theme] (docs/decisions/0021), fonts (Outfit +
                     DM Mono, else the system's; see "Languages and fonts" below, 0041), radii and the
                     surfaces of the shapes (sunken, raised, the frame; 0044, 0047)
  vite.config.ts     SvelteKit + adapter-static → ../crates/api/ui-dist; Paraglide; dev proxy /api → daemon
extension/         the browser extension (pnpm package `extension`; Manifest V3, Chromium-based browsers and Firefox,
                   0055)  [exists]
  src/background.ts  takes the browser's downloads over: Ottie first, then the browser's cancelled and erased
                     (never dropped: kept if Ottie refuses or the browser finished first); notes forms' POSTs;
                     signs in through the host at install; the context menus
  src/lib/           plain logic, unit-tested (*.test.ts, Vitest): settings.ts (the extension's own, in
                     chrome.storage.local: pairing, take over, smallest size, sites left out), takeover.ts (which
                     downloads go, and why not), daemon.ts (Ottie's API with this browser's token; a
                     `requestId` per download handed over, kept for the second try, 0060), host.ts (Ottie
                     on this computer through its native host: sign in, start it), signin.ts
                     (a sign-in link), cookies.ts (the Cookie header), toolbar.ts (the toolbar's icon: the tray's otter in the
                     shade for the browser's dark mode), menus.ts (a page's links, Ottie's list for
                     adding several opened with them), sites.ts, posts.ts, look.ts (Ottie's shared
                     look and language, kept for the pages), sizes.ts
  src/manifest.ts    the manifest per browser (service worker or background script, downloads.ui, gecko ID)
  src/pages/start.ts what every extension page does first: fonts, app.css, theme, language (overwriteGetLocale)
  src/options/       the options page (Options.svelte: sign in, take over, smallest size, sites)
  src/offscreen/     Chromium-based browsers' hidden page: tells the background whether the browser is in dark mode
  src/popup/         the popup (Popup.svelte): its bar (take over, settings), then Ottie's /popup page in a frame,
                     given the token once it's ready and as tall as it says (lib/frame.ts)
  options.html, popup.html, offscreen.html, vite.config.ts  `vite build --mode chrome|firefox` → dist/chrome, dist/firefox; `#ui/…` is
                     ui/src/lib (its components, texts in ui/messages as `ext_*`, format, languages, theme)
shared/api-types/  TypeScript types generated from Rust (ts-rs, by `cargo test -p ottie-api`;
                   output folder set in .cargo/config.toml), plus Ranges.ts (settings' ranges).
                   Never edit by hand.  [exists]
docs/api/          README.md (how to talk to the daemon) + openapi.json (generated) [exists]
docs/screenshots/  the README's pictures: the real pages on a test daemon, with made-up downloads
                   (2× scale, lossless WebP)                                  [exists]
docs/decisions/    one short file per accepted decision (NNNN-title.md), and README.md,
                   one line per decision                                      [exists]
res/icons/         the drawn Ottie icons (app, tray), PNG + SVG               [exists]
Dockerfile         the headless image: UI build → release ottie → debian-slim, /config + /downloads (0051)
docs/service.md    running Ottie as a service: for you, for the computer, Docker
docs/remote.md     reaching Ottie from other devices: Tailscale, a VPN, a reverse proxy; a token per device; folders
.claude/           settings.json (SessionStart hook) + cloud-setup.sh: cloud sessions' setup [exists]
.github/           workflows/ci.yml (every push to main and pull request: fmt, clippy and tests on Linux and
                   Windows, generated files committed, the UI's and extension's check/test/lint, cargo audit),
                   workflows/release.yml (a tag v<version>[-<label>] → a draft pre-release: installers on Windows,
                   Linux, macOS, the extensions' zips; files replaced only while it's a draft; 0056) +
                   release-notes.md (its text, for testers, before the version's section of CHANGELOG.md) [exists]
CHANGELOG.md       what each version adds, changes and fixes, for the people using Ottie (Keep a Changelog, 0057)
                   [exists]
```

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo run -p ottie-core --example fetch -- <url> [folder]   # Ctrl+C pauses, Enter resumes
cargo publish --workspace --dry-run  # check a crates.io release of core → api → ottie (the maintainer publishes)
cargo run -p ottie -- serve          # daemon on 127.0.0.1:24068 (OTTIE_LOG=debug for more logs)
cargo run -p ottie -- token          # print the API token
cargo test -p ottie-api              # also regenerates shared/api-types/*.ts and docs/api/openapi.json: commit them
cargo run -p ottie -- open           # the web UI in a browser, signed in
cargo run -p ottie -- link           # print that signed-in address
cargo run -p ottie -- service status # Ottie as a service (install / uninstall; --system for the computer)
cargo run -p ottie -- device list    # other devices' tokens (add <name> [--read-only] / remove <id>); `token --new`
cargo run -p ottie -- extension install  # the extension's native host for each browser found (status / uninstall)
docker build -t ottie .              # the headless image (docs/service.md)
git tag v0.1.0-experimental && git push origin v0.1.0-experimental  # a draft alpha release on GitHub (0056);
                                     # first CHANGELOG.md's Unreleased → [0.1.0-experimental] (0057), or nothing builds

pnpm install                         # JS dependencies (workspace: ui, shared/api-types)
pnpm --filter ui dev                 # UI with live reload on :5173, /api forwarded to the daemon
pnpm --filter ui build               # → crates/api/ui-dist (then rebuild ottie to embed it)
pnpm --filter ui check               # svelte-check + tsc (fails on warnings)
pnpm --filter ui test                # Vitest unit tests
pnpm --filter ui lint                # Prettier check (`format` fixes)
pnpm --filter ui i18n                # recompile messages/*.json (check, dev, build and test do it too)
pnpm --filter app dev                # desktop app (builds the UI and ottie first; starts it unless one answers)
pnpm --filter app build              # installer: UI → release ottie → app (target/release/bundle/)
pnpm --filter app icon               # rebuild app/icons/icon.ico after changing res/icons

pnpm --filter extension build        # → extension/dist/chrome and dist/firefox (load unpacked from there)
pnpm --filter extension check        # svelte-check + tsc; also test, lint, format

# planned, not scaffolded yet:
pnpm --filter app tauri android dev  # Android device/emulator
```

Before calling a task done: fmt, clippy, tests for the crates you touched; for TS packages you touched, `check`, `test` and `lint`. A release builds the UI before the Rust binary. Update this section when a part comes to exist.

Development on Windows: Rust via rustup (stable, MSVC target) plus VS 2022 Build Tools with the C++ workload. Shells started before that install may lack `~/.cargo/bin` on `PATH`. Node 24 (nvm-windows) and pnpm 12 (`npm install -g pnpm`) are installed. The workspace includes the Tauri app, so `cargo build/test/clippy --workspace` compile Tauri too (WebView2 comes with Windows 11; Linux needs the webkit2gtk 4.1 dev packages, libayatana-appindicator3 for the tray, and libasound2 (ALSA) dev for notification sounds). In a dev run, set `OTTIE_CONFIG_DIR`/`OTTIE_DATA_DIR`/`OTTIE_PORT` to keep away from your real downloads: the app passes them on to the daemon. The dev app still reads and writes the real `app.json` (`%APPDATA%\io.github.matfalco.ottie` on Windows): back it up first. `OTTIE_POWER_DRY_RUN=1` keeps the computer on when trying "when everything has finished".

Cloud sessions (Claude Code on the web): Ubuntu 24.04 on x86_64, 16 GB of RAM, 30 GB of disk, Node 22, only the repo (nothing from the maintainer's machine). The SessionStart hook in `.claude/settings.json` runs `.claude/cloud-setup.sh`, which does nothing elsewhere: the Linux packages Tauri needs, current stable Rust with clippy and rustfmt, pnpm at the `packageManager` version, `pnpm install` when a lockfile or `package.json` changed, and `CARGO_PROFILE_DEV_DEBUG=line-tables-only` so `target/` fits on the disk. Its last line (`cloud-setup: ready …`) says what it found. There, `cargo build/test/clippy --workspace` and the UI's `check`/`test`/`lint`/`build` all run; the desktop app can't be seen on screen, and Windows-only code (`#[cfg(windows)]`: the Mark of the Web, power calls, the Run key) only compiles on Windows: say which of those a change touches, so the maintainer checks them on Windows. Code that only needs pure-Rust crates (like `crates/ottie/src/service/`) can still be checked for Windows and macOS: `rustup target add x86_64-pc-windows-msvc aarch64-apple-darwin`, then `cargo clippy --target …` on a scratch crate that links those files in (the whole workspace can't: aws-lc and SQLite need the target's C compiler). Pages can be checked in a headless browser on a test daemon (`OTTIE_CONFIG_DIR`/`OTTIE_DATA_DIR`/`OTTIE_PORT` in a temporary folder) over the DevTools protocol; synthetic events there must bubble (`new Event('change', { bubbles: true })`), since Svelte 5 listens once at the root. The extension runs in that Chromium too: Playwright (installed globally, `/opt/node22/lib/node_modules/playwright`) with `launchPersistentContext`, `--load-extension=extension/dist/chrome` and `--headless=new`; its service worker's `evaluate` can sign it in (`chrome.storage.local`). `target/debug/incremental` grows past 10 GB: delete it when the disk fills. Tests never hit the real internet anyway, and the network only reaches registries (crates.io, npm) and GitHub.

## Engine rules (crates/core)

- **One owner of job state.** The actor task in `manager.rs` owns all jobs; the cloneable `Manager` handle sends it commands over an `mpsc` channel, each with a `oneshot` reply. Job tasks report back on an internal channel (progress at most every 250 ms with one connection, every 200 ms from a multi-connection run). Events go out on a `broadcast` channel: `Changed(JobSnapshot)` on every state change (clients upsert by ID), plus `Progress` (multi-connection: each range as received so far, `docs/decisions/0025`) and `Restarted`. A subscriber that lags misses events and should resync with `list()`. No shared mutable job state behind ad-hoc locks. Download and move tasks run under `supervised` (`manager.rs`): if one panics (a bug), the actor still hears of its end, so pause, cancel and shutdown answer; the job fails (`Error::Crashed`, `Problem::Other`) and its next run starts over.
- **Explicit state machine:** Queued → Connecting → Downloading → Paused | Completed | Failed | Cancelled | Retrying | Queued (stopped for a hold that went away meanwhile); Retrying → Connecting | Queued (gave way) | Paused | Cancelled; Paused/Failed → Queued on resume. Scheduled (waiting for its start time): Queued | Paused | Failed | Retrying | running → Scheduled; Scheduled → Queued | Paused | Cancelled. The rules live in `JobState::can_become`, and every change goes through `Actor::transition`; invalid transitions are errors.
- **Pause/cancel of a running job** answers only once the task has really stopped and flushed. If the job completes first, it stays Completed. If the file fails as it stops (a pause or a shutdown), it ends Paused with the disk's `Problem`, the pause answers with that error, and one connection's `downloaded` is reread from the file. Cancel deletes the `.part.ottie` before it answers (the actor waits for the drive 1 s at most, `DELETE_WAIT`; then the deletion carries on in its task). Remove (`docs/decisions/0018`) works in any state: cancel unless completed, then forget the job (a completed file stays); the highest ID handed out is saved so a removed job's ID never comes back. Shutdown stops every running job, waits for them and flushes the database. Lowering `max_concurrent` never stops running jobs.
- **Job IDs:** see `docs/decisions/0006`. IDs only ever increase; `BTreeMap<JobId, _>` order = creation order. Queue order is not map order: jobs that never gave way first, then priority, then those that gave way earliest, then `place` (`next_queued`; `docs/decisions/0032`, `0013`).
- **Probe with the first request:** it asks for `bytes=0-`; a `206` proves ranges work, and that response becomes the first range. Split only with a 206, a validator, no `Accept-Ranges: none` and at least 2 × `min_split` bytes (`docs/decisions/0009`); otherwise one connection.
- **Multi-connection** (`segmented.rs`): the coordinator owns the ranges; connections share `end`/`pos`/`durable` atomics with it. Full-size `.part.ottie` up front, once the manager has committed the new plan (`transfer::persisted`, `Download::persist`; `docs/decisions/0059`). Extra requests are closed ranges with `If-Range`. A changed file restarts the run from zero at most once. Idle connections take an unfinished range, else half of the busy range with the most left. A connection failing with a retryable error while others work doesn't stop the run: its range waits for a free connection and the run allows one fewer; the run fails only when the last one does (`docs/decisions/0033`). A connection the limit holds back over `HttpConfig::starved_after` (20 s) ends its request, and a connection waits for budget before requesting, so servers never wait on an unread request. Known cost: a connection whose range ends (or is split) discards what it had buffered past the end, up to ~400 KiB (hyper's read buffer fills while throttled). That's <1% on large files but ~20% on a 2 MiB one; it counts against speed limits. Possible fix later: re-request the first range closed once the size is known. While players read (`Playback`, `docs/decisions/0029`), every 200 ms each point they need next (from each head, and the last 1 MiB) gets its own range unless a connection is within `min_split` of it; a free connection takes it first, else the busy one serving no player with the most left stops where it is. Connections then also hand their bytes to the system every 200 ms (no `sync_data`: a disk sync can take seconds on Windows) and readers see those ranges through `Playback`; only synced ranges are saved. **Streaming mode** (a job's `streaming`, live through `Playback`): the same as if a player read from the start, and idle connections take the next range in order, cut to a piece (~2 s of one connection at the measured speed, `HttpConfig::piece_time`, at least 4 × `min_split`); busy ranges over 4 pieces are cut to one. New jobs get it from `NewJob::streaming`, else for media extensions if `Settings::stream_media`, decided again by the real name at the first response unless `streaming_chosen`.
- **Resume safely:** `Range` + `If-Range` with a strong ETag (weak `W/` ETags are invalid in `If-Range`), else `Last-Modified`. No validator, a 200 instead of 206, or a `Content-Range` that doesn't match → restart from zero and emit `Event::Restarted(reason)`. A restart (`transfer::restarting`, HTTP and FTP) empties and syncs the `.part.ottie` before its new request, so a file and its map always agree. Never splice bytes from two versions of a file.
- **Single connection: the `.part.ottie` length is the truth** for the resume offset (multi-connection: the flushed ranges are). Synced to disk every second while it runs (`transfer::SYNC_EVERY`), as multi-connection runs are. Bytes in flight when a connection dies are lost, so flush on every exit path (pause, error, completion) to keep `downloaded` equal to the file length.
- **Resume re-requests the original URL,** not the redirect target: signed CDN links expire, and `If-Range` still guards consistency.
- **Request `Accept-Encoding: identity`** so byte ranges map to file offsets.
- **Speed limits and priorities** (`docs/decisions/0010`): one token bucket per running job, shared by its connections, spent after each chunk (pause still interrupts the wait). A 50 ms refill task shares the global limit: Urgent first, then weights High 4 / Normal 2 / Low 1, capped by each job's own limit, unused share redistributed (`share()`). Per download, not per connection. A category's limit caps its running jobs together (`allocate()`). Limits change live; 0 = unlimited. A running job that owes budget and gets none for 1 s is reported to the manager (`Limiter::new`'s callback), which sends `waiting_for_bandwidth` in a `Changed` (not saved).
- **Several at once** (`docs/decisions/0036`): `Manager::add_many` adds each job as `add` would (the good ones even if others are refused), saves them in one transaction and schedules once; more than `Settings::max_batch` refuses them all (`TooManyLinks`). `Manager::preview` finds the links in text (`links::find`, in the caller's task) and tells, for each, its name, the category it would get and whether a job has that URL already. Sequences in a link (`docs/decisions/0037`, `sequence.rs`) give each link they hold; `find` counts them before making any, so the cap refuses a huge one at once.
- **Queue order** (`docs/decisions/0032`): highest priority first, then the lowest `place` (new jobs go last). Urgent waits for a free slot (no preemption). `Manager::reorder(ids, Reorder)` (up, down, top, bottom, before/after a job) moves waiting (queued/scheduled/paused) jobs within their priority by swapping places among them, so other jobs keep theirs; each moved job is saved and gets a `Changed`.
- **Start and stop times** (`docs/decisions/0040`): a job's own `start_at`/`stop_at` (`NewJob`, `JobChange`; saved in ms since 1970, UTC). A start time to come makes it `Scheduled`; a running job stops first (`Stop::Hold`). At its start time it joins the queue at its place; at its stop time it pauses wherever it is. Each time is used once, then cleared (`transition` clears the start time on leaving for the queue or a stop). The actor's loop sleeps until the next time (`Actor::wake_at`, `check_times`), at most `CLOCK_CHECK` (30 s), and compares with the wall clock, so sleep or a clock change can't make it miss one. Resume on a scheduled job queues it now.
- **Time slots** (`docs/decisions/0040`, `slot.rs`): kept by the actor like views (`Manager::add_slot` …, `SlotChanged`/`SlotRemoved`, also when one starts or stops applying), in list order (`place`). `check_times` works out the active ones (`update_slots`; a slot that happens once is removed once over) and what they change (`Effects`: per effect, the highest slot that sets it, for the queue and each category), then applies it: limits through the limiter (`apply_limits`: a slot's, else the setting's), caps in `schedule`/`has_room`, and holds in `settle` (a queued job a slot stops becomes `Scheduled` with `held_by`; a running one gets `Stop::Hold` unless the slot lets it finish; released when no slot holds it and its start time has passed: to the queue, or `Paused` if the slot that held it (`held_by`, saved) ended at its time and keeps them paused (`stay_paused`; `keeps_paused`, `Actor::ended` for one that happened once). Settings are never changed by a slot. A job resumed by hand while held gets `ignores_slots` (saved) until it pauses, fails or ends. Wake-ups include the slots' next changes.
- **Retries** (`docs/decisions/0013`): only `Error::is_retryable()` errors (network, drop, stall, 5xx, 408, 429). A retrying job keeps its slot for `retries_before_yield` retries, then gives way if a queued job could start in its place (a total slot, or its category's at its cap; queues behind every job that never gave way); fails after `max_retries`. Waits double from `first_wait` up to 60 s; `Retry-After` (seconds) can ask for up to 10 min. The count resets when bytes arrive or on a manual resume. Stalls: `tokio::time::timeout` around each network wait (`HttpConfig::stall_timeout`), never reqwest's `read_timeout` (it would fire while our throttle waits). Each run of a job (start, resume, retry) gets its own reqwest `Client` (`HttpBackend`), so it never reuses a pooled connection that died silently when the network changed (VPN, Wi-Fi); HTTP/2 connections are also pinged after 10 s of silence.
- **Failures are `Problem`s** (`docs/decisions/0016`), never bare strings: a kind plus details, saved in the `problem_*` columns. Add a kind = a migration (CHECK list), a `Display` wording, an API `Problem` variant and UI messages.
- **Speed history** (`docs/decisions/0044`): the actor feeds each `Event::Progress` to `SpeedHistory`, which keeps each job's speed as the UI's `nextRate` does (a moving average between reports), and adds them up once a second (an hour, in memory): `Manager::speed_history`.
- **Settings** live in the single-row `settings` table; `Manager::start(backend, store)` loads them. They change through `Manager::change_settings` (several at once): one save and one `JobEvent::SettingsChanged` per change, none if nothing changed (`docs/decisions/0022`). A job's own settings (speed limit, priority) change the same way through `Manager::change_job(id, JobChange)`: one save and one `Changed` (`docs/decisions/0024`).
- **Categories** (`docs/decisions/0011`, `0020`): every job has one. Two built-ins can't be removed: General (ID 1: unmatched extensions, default folder) and Other (folders of no category; no folder or extensions of its own; ID in `Actor::other`). `add(NewJob)` uses the caller's category, else, if the caller gave a folder, the category whose folder it is (preferring the extension's), else Other; both are fixed (`category_chosen`, never re-matched). Without either: the category owning the URL's extension, else General. Each extension belongs to one category at most. The job's folder is copied at add (caller's, else the category's, else `NoFolder`). Removing a category moves its jobs to General first (saved before the row is deleted); their files stay where they are. Moving a job to another category moves its file to that category's folder (`docs/decisions/0028`): a running job stops (`Stop::Move`, state and slot kept), its file moves in a task of its own (`Job::moving`, which refuses pause/resume/cancel/remove), its destination reserved and saved first (`Job::moving_to`, settled at the next start if a crash cuts the move short; `docs/decisions/0059`), then it carries on; a job that can't resume is refused while running; a category without a folder leaves the file in place. A category's `max_concurrent` caps its running jobs under the global cap. At the first response, an auto-matched job is re-matched by its real name; its folder follows (via `Placement`, before the file is created) only if it was in its category's folder (`docs/decisions/0012`). Migrations run with foreign keys off and check them before committing. The daemon never creates starter categories itself; the UI's first-start setup offers them (`docs/decisions/0020`).
- **Saved views** (`docs/decisions/0026`): name + filters + sort/grouping, kept by the actor like categories (`Manager::add_view` …, `ViewChanged`/`ViewRemoved`). The engine checks and normalizes them but never filters jobs by them; front ends do. Removing a category replaces it with General in every view (saved before the category row goes).
- **Persistence** (`docs/decisions/0008`): SQLite holds jobs, and later categories, settings and per-segment progress. `STRICT` tables, numbered migrations in `store.rs` (never edit a released one). Saves happen on add, on every state change (inside `transition`), on `Event::Started` (file name + validator known) and progress at most every 2 s. **From 2c: flush file data before saving the progress that covers it.** On load, running jobs become Queued and `downloaded` is reset from the `.part.ottie` length; one whose `.part.ottie` is gone and whose name holds a file of its full size (renamed before `Completed` was saved) is Completed, unless it was to replace a file (`finished_on_disk`). A write that fails is remembered (`docs/decisions/0059`): `Settings::storage_failing` (not saved) holds its text until a write works, `flush` returns the first failure of a queued write since the last one, and `shutdown` answers with it. What the person asks for (add, remove, settings, categories, views, slots, certificates) is saved with `Store::write`, awaited, before it's made in memory (add: taken out again), else `Error::StorageFailed` and nothing changes; progress and state changes stay `Store::queue`d.
- **Shutdown keeps running jobs running** in the database (it doesn't mark them Paused), so the next start continues them; user-paused jobs stay paused. One whose file failed as it stopped is Paused with the problem instead.
- **Files:** write to `<name>.part.ottie` (`PART_SUFFIX`, see `docs/decisions/0007`), rename on completion. Names come from `Content-Disposition` (`filename*`, then `filename`), else the final URL; chosen once at the first response. Sanitize them; never write outside the target folder. A name already taken in the folder (`docs/decisions/0049`) follows the job's `existing`, else the setting's: Rename (` (1)`, ` (2)`), Replace (reserves the name itself, renamed over the old file only once complete; another download's `.part.ottie` still counts as taken) or Ask (`Error::FileExists` before any file is made: the job fails with `Problem::FileExists`). Reserve names with `create_new` so it's atomic, and take the final name with `rename::rename_new` (refuses a taken name; never `fs::rename` under Rename or Ask). Close the file before renaming (Windows refuses to rename open files). On Windows the finished file gets the Mark of the Web (`webmark.rs`, `docs/decisions/0045`), unless policy says not to.
- **Cancellation:** `tokio_util::sync::CancellationToken`, raced against network reads with `tokio::select! { biased; … }`, and against every step before the bytes flow (a request until its headers, an FTP login or command) through `transfer::step`: a stop there is `Error::Stopped`, which the run's entry point turns into `Outcome::Paused` (`transfer::ended`), so it never leaves the backend. Pausing must stop all segments promptly and leave state consistent.
- **Backends:** downloads go through the `Backend` trait (`backend.rs`); the manager is generic over it. `NativeBackend` sends HTTP(S) links to `HttpBackend` and FTP ones to `ftp.rs`; yt-dlp (step 9) plugs in beside it, so keep each protocol's assumptions inside its module. What every protocol's run shares is in `transfer.rs`, and `segmented.rs` asks a `Source` for its ranges, never HTTP itself.
- **FTP** (`docs/decisions/0054`): `ftp://` uses TLS when the server offers it, `ftpes://` requires it (refused before the password goes out), `ftps://` is implicit (990); data connections over TLS too (`PROT P`); passive mode only, the `PASV` address replaced by the server's (`EPSV` over IPv6). The account comes from the link, else anonymous. A certificate the system refuses is accepted only if its SHA-256 is one of `Download::trusted` (set for each run from the manager's trusted certificates for that host); else `Error::UntrustedCertificate` with the fingerprint, and nothing more is sent. Resumable and splittable when the server gives `SIZE` and `MDTM` and lists `REST STREAM` (without `MDTM`, from zero each run: the size alone can't tell versions apart); the validator is `Validator::Ftp("<size> <MDTM>")`, asked again before each resume and range (a change restarts). Each range is a login of its own, closed at its end; a 421 at login gets one more try after `BUSY_WAIT`. Without a size, the server's 226 tells a whole transfer from a cut one. Every exchange runs within the stall timeout. A reply refusing is `Error::FtpReply` (4xx retryable); a connection or TLS failure `Error::Ftp` (retryable).

## Platform gotchas

- **TLS:** rustls everywhere. No OpenSSL/native-tls; it breaks Android and Windows builds. On Android, `rustls-platform-verifier` must be initialised with the app's JNI context and ships a small Kotlin component. Check its docs in the Android step (10).
- **Android background work:** downloads must keep going when the app is in the background. Android 15 limits `dataSync` foreground services to 6 hours per 24 h; prefer user-initiated data transfer jobs. This needs a small Kotlin Tauri plugin. Check current Android docs before implementing.
- **Android storage:** the engine needs a seekable file. Write `.part.ottie` files to app storage and publish finished files to Downloads through MediaStore.
- **Android browser integration:** no reliable extension route (Chrome for Android has no extensions; Firefox for Android only partly supports the downloads API). Take URLs from the share sheet and "Open with" intent filters instead.
- **Extension:** Chrome and Firefox downloads APIs differ; keep browser-specific code behind a small adapter (`manifest.ts` builds one manifest each; `chrome.*` with promises works in both). Chrome can hide its own download UI with `downloads.setUiOptions` (needs the `downloads.ui` permission). If the daemon is unreachable, let the browser download normally. Never drop a download.

## Security

- The daemon binds 127.0.0.1:24068 by default (`address`/`port` in config.toml, `--address`/`--port`, `OTTIE_ADDRESS`/`OTTIE_PORT`; flags win over the file). Remote access is opt-in, always requires a token, and should sit behind Tailscale, a VPN or a reverse proxy with HTTPS rather than be exposed directly (`docs/remote.md`).
- Every API request carries a token (`docs/decisions/0014`): `Authorization: Bearer`, or for the events WebSocket the offered protocol `ottie.token.<token>` (never in a URL or a cookie). This computer's (32 random bytes, hex) lives in `<config>/token`, created on first use; `ottie serve` mentions where only when stdout is a terminal, never in logs. Unknown API paths also need it.
- This computer's token goes only to a daemon that proved it knows it (`docs/decisions/0058`): `POST /api/v1/identity` answers an HMAC of a random nonce keyed with the token. The desktop app (for a daemon it didn't start), `ottie device`, the native host and `ottie open` check it first, and the app gives the local page its commands only after that. A `401` never counts as Ottie.
- The extension's native host (`docs/decisions/0055`) gives a browser a full device token of its own; only the extensions its manifests name can start it (Chromium IDs in `extension.rs`, Firefox's gecko ID), and anyone who can run `ottie` here can read the token file anyway. It starts `ottie serve` out of the browser's reach (own process group; on Windows out of its job object when allowed), never with the browser's pipes.
- Other devices get tokens of their own (`docs/decisions/0052`), full or read-only, kept as SHA-256 hashes in `<config>/access.json`; only this computer's token manages them and the folders list. A read-only token passes `auth::only_reads` (GET, and a few POSTs that change nothing): a new endpoint that changes nothing but isn't a GET must be listed there. Every handler that takes a folder from a client checks it with `Access::folders_for` (devices: categories' folders and those listed only).
- The daemon's pages may be shown in a frame only by its own pages and browser extensions' (`frame-ancestors`): the extension's popup frames `/popup` and hands it the token by `postMessage`, which that page takes only from an extension's page that is its parent (`docs/decisions/0055`).
- Play links (`/stream/{ticket}/…`, `docs/decisions/0029`) are the one exception: the random ticket reads one job's file and expires after 12 h unused. They're served with `Content-Security-Policy: sandbox`, so a downloaded page or SVG can't run scripts as the daemon's pages.
- Cookies and auth headers forwarded by the extension belong to that one job: never log them, never reuse them for another job (`Download::headers`, `RequestHeaders`, `docs/decisions/0055`; the API never gives them back).
- An FTP link's password (`docs/decisions/0054`) stays in the daemon's database: the API (jobs, events) gives links without it, logs go through `redact()`, `suppaftp` logs nothing (`no-log`). An FTPS certificate the system doesn't trust is accepted only when trusted by hand for that host, by its exact SHA-256 (`POST /certificates`, from the download that stopped at it); its handshake's signatures are still checked.
- Another computer's Ottie shown in the desktop app (`docs/decisions/0053`) is only as trustworthy as that computer: its pages get `REMOTE_PERMISSIONS` (window, tray menu, copied links, switching), never a command that reaches this computer's files, programs, settings or power; a new app command is left out of that list unless it can't. Its token lives in the system's keychain, and no page gets a token back.
- The desktop app opens files only through commands that take a job ID (`docs/decisions/0045`): it asks the daemon for the path and refuses a name that's more than a name. Running a program always asks first, in the app's own dialog.
- Logs and errors never contain query strings or credentials: log URLs through `redact()`, and wrap `reqwest::Error` with `without_url()` (done in `Error::from`).
- Only `http`/`https` (plus schemes a backend explicitly supports). The redirect policy in `build_client()` rejects anything else, and caps redirects at 10.

## Code conventions

- Rust: `thiserror` in library crates, `anyhow` only in binaries. `tracing` for logs. No `unwrap()`/`expect()` outside tests unless the invariant is explained in a comment. Clippy enforces this (`[workspace.lints]` + `clippy.toml`). Integration test files start with `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
- New crates inherit `edition`, `license`, `repository` and `publish` (false) from `[workspace.package]`, and set `[lints] workspace = true`. `ottie`, `ottie-core` and `ottie-api` set `publish = true` (the binary depends on the other two, so they're published first); their dependencies on each other need `path` + `version`. The Tauri app stays unpublished.
- License header: published crates' entry files carry the MPL "Exhibit A" notice, because the root `LICENSE` file isn't packaged.
- TypeScript: `strict`, no `any`. API types come only from `shared/api-types/` (`import type { Job } from '@ottie/api-types/Job'`). In the UI, `#lib/…` imports of TS files end in `.js` (`#lib/format.js`). Logic lives in plain `.ts` files with unit tests; components stay thin. Prettier formats (tabs, single quotes).
- UI (`docs/decisions/0015`, `0021`): runes mode; styles in each component's `<style>` using the variables in `app.css` (colours, `--font`/`--mono`, `--radius-s/m/l`, `--radius-pill`, `--edge`), never raw colours; the page talks to the daemon only through `api.ts`. Shapes follow `docs/decisions/0044` (pills, `--sunken` and `--raised` with their shadows; one look since 0047); check light and dark when adding colours or shapes. Every page must also work at the desktop app's minimum size (720 × 480, `min_inner_size` in `app/src/main.rs`) and on a phone (360 px wide and up; under 40rem the tabs are a bar along the bottom; there and in windows under 40rem tall, the download list keeps a height of its own with the details panel under it, and the page scrolls). Pages are mirrored for languages written right to left (`docs/decisions/0042`): use logical properties (`margin-inline-start`, `inset-inline-end`, `text-align: start`, `style:inset-inline-start=…`), never left/right; a name, link or folder shown or typed gets `dir="auto"`; something drawn in a direction (a chart, an arrow) flips under `:dir(rtl)`. Two traps: a visually hidden `position: absolute` input needs a positioned parent, or it hangs from the window and makes it scroll; and a flex item with `overflow: hidden` shrinks below its content unless it's `flex: none`.
- Translations (`docs/decisions/0016`): no visible text in components or logic: every string is a message in `messages/en.json` *and* `fr.json` (French typography: no-break space before `: ? !` and inside « »); numbers, sizes and durations go through `format.ts` (`Intl`); daemon text is shown through `explain.ts`. Functions that build text take a `locale` parameter (default `getLocale()`) so tests can check both languages. Other languages (`docs/decisions/0041`): write a count as a label ("Pobierane: {count}"), not with plural forms; a new language is a file in `ui/messages/` and `app/messages/`, its tag in `project.inlang/settings.json`, its name in `NAMES` (`languages.ts`) and its file in `LANGUAGES` (`app/src/texts.rs`); once a native speaker has checked it, add it to `REVIEWED`. A language Outfit can't fully draw goes in the `:lang()` list in `app.css` (the system's font throughout; a test fails until it does) and in the table under "Languages and fonts". A language written right to left goes in `RIGHT_TO_LEFT` in `languages.ts` and `app/src/texts.rs`, and every placeholder in its file sits between U+2068 and U+2069 (a test checks). A language the browsers lack `Intl` data for (check in Edge: `Intl.NumberFormat.supportedLocalesOf`) gets a stand-in in `INTL` (`languages.ts`), and its tray texts follow the stand-in.
- API docs (`docs/decisions/0030`): every handler has a `#[utoipa::path]` (route, body, answers, the `Reason` codes it can refuse with) and is registered with `routes!`; wire types derive `ToSchema` beside `TS`. Fields inside the tagged enums (`Problem`, `Reason`, `Event`) stay one word (utoipa ignores `rename_all_fields`; a test checks). A change in how to use the API also updates `docs/api/README.md`.
- API errors (`docs/decisions/0016`): every refusal has a `Reason` code (`crates/api/src/types.rs`) besides the English `error`; a new engine `Error` variant gets a code in `error.rs`.
- Tests: engine behaviour is tested against the in-process server in `crates/core/tests/support/` (dropped connections, no range support, slow chunks, changed ETag, `unplug()`: open connections go silent as after a network change). Speed-limit tests (`tests/throttle.rs`) assert shares and minimum durations with wide margins, never exact speeds; progress snapshots lag up to ~0.5 s, so don't measure rates over short windows. Multi-connection tests (`tests/segmented.rs`) use `HttpConfig { connections: 4, min_split: 64 KiB }`; the other test files pin one connection or use files too small to split. Tests never hit the real internet. A resumed download must be byte-identical to a clean one. Don't assert exact byte counts after an injected connection drop; assert `.part.ottie` length == `downloaded` instead. Manager tests subscribe before adding jobs, and wait for several concurrent jobs with `wait_for_all` (any order), never one after another.
- Line endings: LF everywhere (`.gitattributes`).
- Commits: Conventional Commits (`feat(core): …`, `fix(extension): …`), one per slice: the code, its tests and its docs (decision record + its line in `docs/decisions/README.md`, CLAUDE.md) together. Unrelated changes (e.g. a test fix found along the way) get their own.
- Changelog (`docs/decisions/0057`): a commit that changes what people using Ottie get (a feature, a change in behaviour, a fix of something already released) adds its line under `## [Unreleased]` in `CHANGELOG.md`, in the same commit, under Added / Changed / Removed / Fixed / Security, in plain words for them, not for developers. Refactors, tests, CI, developer docs and fixes of what isn't released yet get none. A release renames Unreleased to `[<version>[-<label>]] - <date>` (as its tag), opens a new empty Unreleased and updates the links at the bottom.
- New dependency: say what it's for and why it beats the alternatives before adding it.

## Languages and fonts

Ottie embeds two fonts, Outfit and DM Mono for numbers in columns (`docs/decisions/0041`). A page in a language Outfit draws in full uses them; a page in any other language uses the system's fonts throughout, monospace included, picked by the browser for the page's `lang` (`app.css`; a test checks the list against Outfit's characters). "Numbers and dates" says what `Intl` writes them in when the browser has no data for the language (Chrome, Edge, the desktop app).

| Language | Tag | Drawn in | Numbers and dates |
|---|---|---|---|
| English | `en` | Outfit | own |
| French | `fr` | Outfit | own |
| German | `de` | Outfit | own |
| Spanish (Spain) | `es-ES` | Outfit | own |
| Spanish (Latin America) | `es-419` | Outfit | own |
| Italian | `it` | Outfit | own |
| Dutch | `nl` | Outfit | own |
| Polish | `pl` | Outfit | own |
| Portuguese (Portugal) | `pt-PT` | Outfit | own |
| Portuguese (Brazil) | `pt-BR` | Outfit | own |
| Swedish | `sv` | Outfit | own |
| Romanian | `ro` | Outfit | own |
| Czech | `cs` | Outfit | own |
| Hungarian | `hu` | Outfit | own |
| Danish | `da` | Outfit | own |
| Finnish | `fi` | Outfit | own |
| Slovak | `sk` | Outfit | own |
| Croatian | `hr` | Outfit | own |
| Slovene | `sl` | Outfit | own |
| Lithuanian | `lt` | Outfit | own |
| Latvian | `lv` | Outfit | own |
| Estonian | `et` | Outfit | own |
| Irish | `ga` | Outfit | own |
| Maltese | `mt` | Outfit | own |
| Indonesian | `id` | Outfit | own |
| Turkish | `tr` | Outfit | own |
| Swahili | `sw` | Outfit | own |
| Zulu | `zu` | Outfit | South African English (`en-ZA`) |
| Greek | `el` | system font (Greek script) | own |
| Bulgarian | `bg` | system font (Cyrillic) | own |
| Ukrainian | `uk` | system font (Cyrillic) | own |
| Vietnamese | `vi` | system font (ạ, ư, ổ…) | own |
| Hindi | `hi` | system font (Devanagari) | own |
| Chinese (simplified) | `zh-Hans` | system font (Han) | own |
| Chinese (traditional) | `zh-Hant` | system font (Han) | own |
| Japanese | `ja` | system font (kana, Han) | own |
| Korean | `ko` | system font (Hangul) | own |
| Amharic | `am` | system font (Ethiopic) | own |
| Hausa | `ha` | system font (ɓ, ɗ, ƙ) | Nigerian English (`en-NG`) |
| Yoruba | `yo` | system font (ẹ, ọ, ṣ, tone marks) | Nigerian English (`en-NG`) |
| Arabic | `ar` | system font (Arabic script), monospace too | own, Western digits (`ar-u-nu-latn`) |
| Persian | `fa` | system font (Arabic script), monospace too | own (Persian digits and calendar) |
| Urdu | `ur` | system font (Arabic script), monospace too | own |

## How we work

- For decisions that are hard to reverse (data model, API shape, crate choices, architecture), present 2–3 options with trade-offs and let the maintainer choose. For small implementation details, decide and say what you chose.
- Work in small vertical slices that run end to end. Don't scaffold code ahead of need.
- Show the diff and wait for a go-ahead before committing.
- Record each accepted decision in `docs/decisions/` (one short file, `NNNN-title.md`) and add a line for it to `docs/decisions/README.md`, which sums up every decision. Update this file when commands, layout or rules change.

## Build order

Done: ✅ and struck through. 🟡: built, waiting for the maintainer's review. ‼️: in progress (stopped partway: what's there is committed and documented).

1. ✅ ~~`core`: single-connection HTTP download with pause/resume, plus the test server.~~
2. ✅ Engine slices:
   - ✅ ~~2a job manager + state machine, `Backend` trait, queue~~
   - ✅ ~~2b SQLite persistence~~
   - ✅ ~~2c multi-connection~~ (streaming mode comes in 5f3)
   - ✅ ~~2d speed limits + priorities~~
   - ✅ ~~2e categories~~
   - ✅ ~~2f `Content-Disposition` filenames + category re-match~~
   - ✅ ~~2g retry with backoff~~
3. ✅ Usable app, early:
   - ✅ ~~3a minimal `ottie serve` (jobs API + events WebSocket, token, folders)~~
   - ✅ ~~3b first web UI (SvelteKit SPA built into the binary; list, add, pause/resume/cancel; token link + login page; `ottie open`)~~
   - ✅ ~~3b2 translations (en, fr; Paraglide) + error codes from the daemon~~
   - ✅ ~~3c first desktop app (Tauri window on the same UI, starts the daemon as a sidecar, Ottie icons from `res/icons/`)~~
4. ✅ Features grow through API, UI and desktop app together, one slice each:
   - ✅ ~~4a list layout (sidebar views, remove, clear finished)~~
   - ✅ ~~4b table list with selection + toolbar, own title bar~~
   - ✅ ~~4c left menu (Dashboard, Downloads), status filter in Downloads, dashboard~~
   - ✅ ~~4d categories (API, Categories page, folder picker, Category column, move to category)~~
   - ✅ ~~4e category step when adding + first-start setup with editable starter categories~~
5. The redesign from the mockups (`docs/decisions/0021`), then more features the same way:
   - ✅ ~~5a look (four themes, tabs in the title bar, add card, the four tabs laid out as in the mockups, Settings with appearance + language)~~
   - ✅ ~~5b settings (API + page: max concurrent, global limit with the title-bar chip, retry policy, appearance sync)~~
   - ✅ ~~5c1 Downloads filters (quick-filter pills, Filters and sorting panel, group by status; `docs/decisions/0023`)~~
   - ✅ ~~5c2 detail panel with per-download speed limit + priority (`PATCH /jobs/{id}`, `docs/decisions/0024`)~~
   - ✅ ~~5d per-connection progress + file map (ranges in progress events, `docs/decisions/0025`)~~
   - ✅ ~~5e saved views (stored in the daemon, `docs/decisions/0026`), the detail bar as file map (`0027`), files move with their category (`0028`)~~
   - ✅ 5f play while downloading (`docs/decisions/0029`):
     - ✅ ~~5f1 play links + reader~~
     - ✅ ~~5f2 follow the player (seek, last chunk early)~~
     - ✅ ~~5f3 streaming mode per download (on by itself for videos and music, a setting turns that off)~~
   - ✅ ~~5g queue reorder (`docs/decisions/0032`)~~
   - ✅ ~~5h tray (`docs/decisions/0034`)~~
   - ✅ ~~5i notifications (`docs/decisions/0035`)~~
   - ✅ ~~5j several links at once (`docs/decisions/0036`)~~
   - ✅ ~~5k sequences in one link (`[001-120]`, curl-style; `docs/decisions/0037`)~~
   - ✅ ~~5l starting at sign-in (`docs/decisions/0039`)~~
6. ✅ Scheduler (`docs/decisions/0040`):
   - ✅ ~~6a a download's own start and stop time (the `Scheduled` state)~~
   - ✅ ~~6b time slots + the Schedule tab~~
   - ✅ ~~6c when everything has finished (desktop app)~~
7. More features:
   - ✅ ~~7a clipboard monitoring (desktop app, `docs/decisions/0043`)~~
   - ✅ ~~the rounded style, the look from before as legacy palettes, the new Dashboard and an hour of speed (`docs/decisions/0044`)~~
   - ✅ ~~one look: the legacy palettes go, their CSS folded in (`docs/decisions/0047`)~~
   - ✅ ~~7b open the file and its folder, Pause all / Resume all in the main window (`docs/decisions/0045`)~~
   - ✅ ~~7c keyboard shortcuts (`docs/decisions/0046`)~~
   - ✅ ~~7d animations (dialogs, menus, rows coming and going, the queue's reorder, toggles and what
     unfolds), none under `prefers-reduced-motion` (`docs/decisions/0048`)~~
   - 🟡 7e a file already on disk: the add step asks (rename, replace, or skip), at the first response a
     download does what was chosen, else the setting (rename, replace or ask; `docs/decisions/0049`).
     Built, not reviewed yet
   - 🟡 7f onboarding in several steps, each skippable (and the whole of it): language + look, downloads
     (folder, how many at once, a file already there), starter categories, sign-in and closing, clipboard,
     notifications (desktop app), saved step by step, run again from Settings (`docs/decisions/0050`);
     later the extension and other devices. Built, not reviewed yet
   - 🟡 7g Ottie as a service only (no app): `ottie service install|uninstall|status`. Per user by default
     (systemd user unit with `--at-boot` for lingering, LaunchAgent, Task Scheduler at sign-in: the user's
     own folders), system-wide with `--system` (systemd unit as `ottie`, Windows service as
     `NT SERVICE\Ottie`; not macOS yet) and a Docker image (`docs/decisions/0051`). People connect as
     today: the daemon's page in a browser (`ottie open`/`ottie link`, `--system` for the system's),
     the desktop app (uses a daemon that answers), the extension. Built, not reviewed yet: what was tried
     and what's left to check (real systemd, Windows, macOS) is at the end of 0051
   - 🟡 7h remote access (`docs/remote.md`: Tailscale, a VPN or a reverse proxy with HTTPS, never the port
     open) and hardening (`docs/decisions/0052`): a token per device, full or read-only, removable
     (Settings → Connection, `ottie device`), this computer's replaced by `ottie token --new`; devices write
     only in categories' folders and those listed (Settings, `folders` in config.toml); the system service
     under its own account came with 7g. Built, not reviewed yet: what's left to check is at the end of 0052
   - 🟡 7i several daemons from one place (`docs/decisions/0053`): in the desktop app, a list (name,
     address, token in the system's keychain) and a switcher beside the logo, another computer's page
     with fewer app commands, the tray, notifications and copied links following the one shown, this
     computer's still starting (a setting: only once shown); in a browser, a list per address, switching
     to the other page. Built, not reviewed yet: what's left to check is at the end of 0053
   - 🟡 7j FTP and FTPS downloads (`docs/decisions/0054`): `ftp://` (TLS when offered), `ftpes://`, `ftps://`;
     the account in the link, its password never shown again; a certificate nobody trusts trusted from the
     download (Settings → Connection lists them); resuming and several connections as for HTTP. Built, not
     reviewed yet: what's left to check (real servers, a NAS's own certificate on Windows) is at the end of 0054.
     SFTP later
8. 🟡 Browser extension (`docs/decisions/0055`), Chromium-based browsers and Firefox: every download taken over
   (unless off, private, from a form, smaller than a chosen size or from a site left out) with the browser's cookie,
   referrer and user agent (`POST /jobs`), never dropped; the popup is the daemon's own `/popup` page; signed in by
   itself where Ottie is installed (`ottie` as the native messaging host, `ottie extension install`, run by the app),
   else by a sign-in link; "Download with Ottie" and "Download all links with Ottie" in the context menu. Built in
   slices 8a–8e, not reviewed yet: what's left to check (Firefox, Windows' registry, real sites) is at the end of 0055
9. yt-dlp backend (its own updates, apart from the app's).
10. Tauri Android, last, once everything else is done: embedded core, background downloads, share-sheet intake.

Languages, a batch per slice, between the steps above (`docs/decisions/0041`; no Russian or Hebrew for now):
- ✅ ~~L1 "System" by default (also as the shared language), matching the system's tag, unreviewed note; de, es-ES, es-419, it, nl, pl, pt-PT, pt-BR~~
- ✅ ~~L2 the rest of the EU in Latin script: sv, ro, cs, hu, da, fi, sk, hr, sl, lt, lv, et, ga, mt~~
- ✅ ~~L3 Greek and Cyrillic: el, bg, uk (Manrope + JetBrains Mono, bundled)~~
- ✅ ~~L4 Asia: zh-Hans, zh-Hant, ja, ko, id, hi, vi, tr (Manrope for vi; Noto cut to our messages for hi and CJK)~~
- ✅ ~~L5 Africa: sw, am, ha, yo, zu (only Outfit + DM Mono embedded from then on, the system's fonts for the rest; ha, yo, zu numbers and dates in their country's English)~~
- ✅ ~~L6 Arabic, Persian and Urdu, with the whole layout mirrored (right to left; `docs/decisions/0042`)~~

Each translation is drafted, then checked by a native speaker when one helps (a platform like Weblate); checked ones go in `REVIEWED`.

## Open decisions

- yt-dlp: bundled or found, how it updates, ffmpeg, format choice.

Settled: extension pairing (a native messaging host where Ottie is installed, else a sign-in link; `docs/decisions/0055`); no uGet import; no aria2 (its HTTP strengths are in our engine already) and no BitTorrent/magnet links.

## Later (TODO)

- Metalink? (one file from several mirrors, checked by checksum).
