# 0054: FTP and FTPS downloads

Accepted 2026-10-17. Builds on `docs/decisions/0009` (several connections), `0013` (retries), `0016` (problems as codes), `0029` (play while downloading) and `0033` (a connection that fails).

**Why:** uGet downloads from FTP servers, and many NAS boxes, routers and old mirrors still serve files that way. Downloads only: Ottie never uploads.

**Decision** (Mathieu's choices: the password in the link, hidden; FileZilla's names; a certificate trusted from the download; several connections as for HTTP):
- **Links:** `ftp://` uses TLS when the server offers it (`AUTH TLS` in its features), plain otherwise; `ftpes://` requires TLS and fails with `noTls` without it, before the password is sent; `ftps://` speaks TLS from the first byte (port 990). Data connections go over TLS too (`PROT P`). The path is relative to where the login starts, as RFC 1738 has it (`%2F` for an absolute one); a `;type=i` ending is ignored; a link to a folder (ending in `/`) is refused when added.
- **Accounts:** `ftp://user:password@host/…`, else anonymous (`anonymous` / `ottie@`). The daemon keeps the link as given, in its database, in plain text, like the cookies the extension will forward; pages, the API, events, logs and the Mark of the Web get it without the password (`ftp://user@host/…`).
- **Certificates:** the system's check first (rustls with the platform verifier, as HTTPS). One it refuses (no known issuer, another name, expired) stops the download with the problem `certificate`: the server's name and the certificate's SHA-256. "Trust this certificate" in its details (`POST /certificates`, then resume) trusts that exact certificate for that server from then on; a server showing another one is refused again. The handshake's signatures are always checked. The list is in Settings → Connection once there's one, where each can be removed; the daemon keeps it in its database (migration 20) and hands each run the fingerprints trusted for its server.
- **Resuming and splitting:** a file the server gives the size and modification time of (`SIZE`, `MDTM`), on a server that supports `REST STREAM`, resumes and splits as HTTP does: each range is its own login, `REST` to its start, `RETR`, closed once it reaches its end. Before each one, the size and modification time are asked again and compared, as HTTP's `If-Range`; a change starts the download over. Without `MDTM`, one connection, from the start each time (changed 2026-10-20, Mathieu's choice): the size alone can't tell two versions of a file apart, and resuming then could join the start of one to the end of the other. Streaming mode and players' seeks work the same. A server that refuses a login for too many connections (421) gets one more try after half a second (it often still counts the one just closed), else the run carries on with fewer, as for HTTP (`0033`). Without a size or `REST`, one connection, from the start each time; the server's final reply (226) then tells a cut transfer from a whole one.
- **Problems:** a refused command is `ftpReply` with its code and the server's words (530, 550 and 421 in words; 4xx retried, 5xx not), a failed connection or TLS handshake `network` (retried), `certificate` and `noTls` as above.
- **In the code:** `NativeBackend` (what `ottie serve` uses) sends HTTP(S) links to `HttpBackend` and FTP ones to `ftp.rs`. What both share moved to `transfer.rs`: an answer's bytes (`Body`), where a multi-connection run's ranges come from (`Source`, so `segmented.rs` serves both), naming the file at the first answer, one connection's loop, completing.
- **Crate:** `suppaftp` (MIT/Apache), with its tokio and rustls (aws-lc-rs) features: maintained, async, explicit and implicit TLS, `REST`, `SIZE`, `MDTM`, `FEAT`. Its `no-log` feature turns its logging off: it would log the password at trace level. 5 new crates in all (`suppaftp`, `lazy-regex`, `pin-project` and their macro crates).

**Known limits:**
- Passive mode only (the usual one behind routers and firewalls); the address in a `PASV` answer is replaced by the server's own (many servers behind NAT name a private one), `EPSV` over IPv6. No active mode.
- The system's proxy settings don't apply to FTP (HTTP proxies don't carry it).
- An `ftp://` link whose server offers TLS with a certificate nobody trusts fails until it's trusted, as FileZilla asks; it doesn't fall back to plain.
- Certificates are trusted per server name, whatever the port; only for FTPS (HTTPS downloads still need a certificate the system trusts).
- A link to a folder isn't downloaded (no listing of what's in it).
- An HTTP redirect to an FTP link is still refused.
- The test certificate and its key are in `crates/core/tests/support/` (for the tests' FTPS server only).

**Alternatives:**
- An FTP client of our own (the commands Ottie needs are few): no new crate, but TLS on the data connections, its session reuse and the servers' many quirks are what `suppaftp` already handles.
- Saved servers in Settings (host, user, password) filling in links without one: more to build; may come on top later.
- Trusting the first certificate seen (as SSH does): nothing to click, but a first connection intercepted goes unnoticed. Refusing any certificate the system doesn't trust: a NAS's own would only work over plain FTP.
- `ftps://` as explicit TLS on port 21: closer to the RFC, but curl and FileZilla read it as implicit, so copied links would mean something else.
- One connection per FTP download: simpler and gentler on servers that cap logins, but slower from far away, and a player's seek would wait.

**Tried so far** (in a Linux cloud session, 2026-10-17):
- An in-process FTP server in the core's tests (`tests/support/ftp.rs`: plain, explicit and implicit TLS, a capped number of logins, without `SIZE`, `MDTM` or `REST`, a transfer cut short): anonymous and named logins (a wrong password is `ftpReply` 530, not retried, and not in the error), a missing file (550), several connections with `REST`, fewer logins allowed, pause and resume over several connections, resuming one connection where it stopped, a changed file starting over, without `REST` starting over, an untrusted certificate (its fingerprint; no login before it) then trusted, another certificate still refused, `ftpes://` to a server without TLS (no password sent), implicit TLS, several connections over TLS, without `SIZE`; the manager handing a run the certificates trusted for its server. The store: FTP validators, the new problems, trusted certificates. The API: `/certificates`, and a job's `url` without its password.
- The real daemon with that server on its own: a download stopped at the certificate, its details (the problem with the fingerprint, "Trust this certificate"), trusted from the page and finished; Settings → Connection's list, at phone width, in Arabic.
- `cargo clippy -D warnings`, the crates' tests; the UI's check, lint and tests (one test needs Node 24's `Intl.Locale#getTextInfo`). Two older timing tests in `crates/core/tests/manager.rs` and four in `segmented.rs` fail in this session on the code before this change too: their test server's 2 ms pacing now finishes a download before the first progress report.

**Still to check:**
- Real servers: FileZilla Server, vsftpd (it requires the data connection to reuse the TLS session), ProFTPD, Pure-FTPd, IIS, a Synology or QNAP NAS, a router's USB share; anonymous mirrors (e.g. a Linux distribution's).
- On Windows, a NAS's own certificate: Windows' verifier refusing it, then trusting it from the download.
- A server behind NAT giving a private address in `PASV`; a server limiting connections per address.
- Play while downloading from FTP, and a seek.
- The new texts in the other 41 languages (and the app's six) were drafted, not reviewed.
