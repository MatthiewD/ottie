# 0058: Proving the daemon's identity

Accepted 2026-10-20 (Mathieu's choice). Builds on `docs/decisions/0014` (the token), `0017` (the desktop app), `0052` (devices) and `0055` (the extension's native host).

**Why:** the desktop app, `ottie device`, `ottie open` and the extension's native host sent this computer's token to whatever answered on Ottie's port, and the app counted even a `401` as "Ottie is there". A program from another account that took the port first (Ottie not running yet) would receive the token, and the app would give its page the commands meant for this computer's Ottie (opening files, power, sign-in). An address, a port and a status code don't say which process answers; a token sent to a server doesn't prove the server either.

**Decision:**
- **A public route, `POST /api/v1/identity {nonce}`** answers `{proof}`: HMAC-SHA256 keyed with this computer's token, of `ottie identity v1`, a line break and the nonce, as hex. It needs no token and gives none away: only a process that can read the token file can answer, and another account can't read it. The nonce is 32 to 128 hex digits, random and new for each check, so a recorded answer can't be replayed. Device tokens play no part: only this computer's token is proved.
- **Before this computer's token goes anywhere, the client checks the proof:** the desktop app for a daemon it didn't start itself (one it started, which said `ready`, is its own child), `ottie device` (and so the native host's pairing, which adds a device with it), and `ottie open` (whose page gets the token). Anything else on the port gets nothing but the nonce.
- **The app gives the local page its commands** (`PAGE_PERMISSIONS`) only once that daemon is its own or has proved itself (`remote::start_local`), no longer before.
- **Something that can't prove it** gets a message instead of the token: "what answers at … isn't your Ottie … It may be Ottie started with other folders, an Ottie older than this one (restart it), or another program on that port". A `401` no longer counts as Ottie: Ottie started with other folders isn't shown any more (its page used to ask for its token).
- **The HMAC** is the RustCrypto `hmac` crate with `sha2`, already in the lockfile; the daemon's side is `ottie_api::identity` (used by `ottie` too), the app's `app/src/identity.rs`. A test on each side expects the same proof for the same token and nonce, so they can't drift apart.
- Another computer's Ottie (`0053`) is unchanged: the user gave its address and token, and HTTPS proves the server where it's used.

**Alternatives:**
- **A password:** it proves who the user is to the server, not the server to the app. An impostor would show its own sign-in page and collect the password.
- **A passkey (WebAuthn):** tied to the origin, `http://127.0.0.1:24068`, which is exactly what the impostor holds; the browser would sign its challenge. It wouldn't get a reusable secret, but it would still be shown with the local commands, and every start would need a sign-in.
- **Asking the system who owns the port** (`GetExtendedTcpTable`, `/proc`, `lsof`) and checking its account and program: different on each system, and fragile with containers, Docker's forwarding and the system service's own account.
- **A self-signed certificate pinned by the app:** heavier to make, store and renew. **A named pipe or Unix socket:** the webview needs HTTP.

**Limits:** an Ottie older than this one has no `/identity`: the app and `ottie` refuse it until it's restarted on the new version. The proof says the process can read the token file; anyone who can read that file can do everything anyway.

**Tried:** the HMAC against RFC 4231's test vector; the route end to end (a right proof for this computer's token, none for another, a bad nonce refused); a fake server on the port answering `401` to everything: `ottie device list` and `ottie open` refuse, and it never receives the token. `ottie device` against a real daemon still works.

**Still to check:** the desktop app on Windows: it starts as usual, with its own daemon and with one already running (a terminal, the service); with a fake server on the port it shows the message and opens no window on it.
