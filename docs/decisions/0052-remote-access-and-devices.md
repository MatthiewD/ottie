# 0052: Reaching Ottie from other devices: a token for each, read-only ones, the folders they may use

Accepted 2026-10-16. Builds on `docs/decisions/0014` (the token) and `0051` (the system-wide service under an account of its own).

**Why:** one token did everything. Shared with a phone, it was this computer's own: it couldn't be taken back without signing every browser and the desktop app out, a TV couldn't be given less, and whoever held it could have a download written in any folder the daemon's account can write to (a Startup folder: a program run at the next sign-in).

**Decision** (Mathieu's choices: everything at once; the safest default for folders, which Settings and config.toml can widen; devices from Settings and the command line; read-only tokens now):
- **This computer:** the token file's token (`ottie token`, `ottie link`, the desktop app) can do everything. Only it manages devices and the folders they may use (`ownerOnly` otherwise). `ottie token --new` replaces it; the daemon looks at the token file's date at every request, so the old token stops working at once and its event streams close.
- **A token for each other device**, full or read-only, from **Settings → Connection** (a name, Read-only, then its sign-in link, shown once) or `ottie device add <name> [--read-only] | list | remove <id>` (a client of the running daemon, with this computer's token). The API: `GET /access` (who this token is, the folders), `PUT /access/folders`, `GET`/`POST /access/devices`, `DELETE /access/devices/{id}`. Kept in `access.json` beside the token file, readable by its owner only: each device's name, read-only or not, when added and last used (written at most hourly), and a SHA-256 of its token, never the token. IDs never come back. Removing a device refuses its token at once and closes its event streams. Settings lists the devices with when each was last used; a device sees which it is.
- **Read-only:** every `GET`, the events, and the `POST`s that change nothing (`/jobs/preview`, `/categories/match`, a play link); anything else is `403 readOnly`, decided by the auth layer from the method and path, so a new endpoint is refused until listed. The pages say so once at the top and drop what can't be done: the add card, pasting links, actions on downloads and in the queue, saving views, editing categories and time slots; shared settings and a download's own are shown disabled; the setup isn't offered; the look and language stay per device.
- **Folders:** a device may only have downloads written (`destDir`, a category's `folder`) inside a category's folder or one listed: in Settings (this computer only) or in `config.toml` (`folders = [...]`, absolute, which Settings can't remove). No list means categories' folders only. Folders are compared once links are resolved (as far as the folder exists), ignoring case on Windows. A device's folder with `..` in it is refused (added 2026-10-20): after a link, `..` means the parent of the link's target, so `link/../x` could leave the allowed folder while looking inside it. Without `..`, the system follows the same links at the check and at the write; only a link changed in between could differ, and a device can't make links on this computer. This computer isn't limited: it could widen the list anyway. Moving a download to another category needs no check (its folder is a category's).
- **The guide** `docs/remote.md`: Tailscale (`tailscale serve`, Ottie staying on 127.0.0.1, HTTPS within the tailnet; never `funnel`), another VPN (`address` = the VPN's), or a reverse proxy with HTTPS (Caddy, nginx with WebSockets, long reads, 64 MB bodies, no HTTP Basic); never the port open to the internet; a checklist.
- The system-wide service under an account of its own, the last point of this step, came with 0051.

**Alternatives:**
- Tokens kept in the database: the engine would hold sign-in data, and a migration for something only the API uses; a file beside the token file keeps secrets together.
- Device tokens in the clear, shown again later: one file read would leak them all.
- The folder check only for listening beyond 127.0.0.1: Tailscale and proxies reach the daemon from 127.0.0.1 too.
- Limiting this computer as well: no gain, since it can change the list.
- A fixed list of what read-only may do in each handler: easy to forget in a new one; one check at the door refuses by default.
- Read-only later: the UI would have shown buttons that only fail.

**Tried so far** (in a Linux cloud session, 2026-10-16):
- API tests: devices added, listed and removed (their event stream closes); a read-only device refused what changes; folders inside and outside the allowed ones, a link out of an allowed folder, a category's folder, this computer unlimited.
- A CLI test against the real binary: `device add|list|remove`, and `token --new` (the old token refused at once, devices' kept).
- The pages in headless Chromium on a test daemon: Settings → Connection as this computer (devices, adding one and its link, folders) and as a read-only TV (which device it is, the folders), Downloads read-only with a download selected (no actions, its controls disabled).
- `cargo clippy -D warnings`, the Rust tests of `ottie` and `ottie-api`; the UI's check, lint and tests (one test needs Node 24's `Intl.Locale#getTextInfo`, missing from the session's Node 22).

**Still to check:**
- `tailscale serve --bg 24068` and `tailscale serve --https=443 off` (written from memory), a VPN address and a proxy (Caddy, nginx) for real, from a phone.
- On Windows: the desktop app (it signs in with this computer's token; after `ottie token --new` it needs a restart), folder checks (case, `\\?\` paths, another drive, a network share), `access.json` in `%APPDATA%\Ottie` and `%ProgramData%\Ottie`.
- A read-only device's pages on a phone and right to left (ar, fa, ur), every tab; disabled fields now dimmed everywhere (`app.css`), in light and dark.
- The new texts in the other 41 languages were drafted, not reviewed.
