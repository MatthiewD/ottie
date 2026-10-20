# 0003: The project is called Ottie

Accepted 2026-10-12. Supersedes [0001](0001-package-names.md).

**Decision**
- Product name **Ottie**. Store and extension listings use "Ottie – Download Manager".
- The mascot is an otter. Sea otters stash food and a favourite stone in a skin pouch, so Ottie "tucks your files away".
- Packages: `ottie` (the binary), `ottie-core`, `ottie-api`, `ottie-app` (Tauri).
- Config/data folders `ottie` / `Ottie`, environment variable prefix `OTTIE_`, systemd unit `ottie.service`, Docker image `ottie`.
- App identifier `io.github.matfalco.ottie`, written in lowercase as Android package names conventionally are; GitHub usernames are case-insensitive. This is the reverse of the GitHub Pages domain `matfalco.github.io`, so no domain needs buying.

**Why the identifier is permanent:** Tauri's `identifier` becomes the Android package name, and on desktop it names the app-data folder. Changing it after the first Android release makes a different app with no upgrade path. If Ottie ever gets its own domain, only the website changes, not the identifier.

**Reserving the name** (checked 2026-10-12)
- crates.io: `ottie` is free. `ottie` 0.0.1 is prepared in `crates/ottie` as a small, genuine release (`--help`, `--version`); Mathieu publishes it. `ottie-core`, `ottie-api`, `ottied`, `ottie-cli`, `ottie-app` and `ottie-daemon` were also free.
- npm: `ottie` is free. Only worth taking if we ever publish a JS package.
- GitHub: `github.com/ottie` is an existing user account, so the repo was `github.com/Matfalco/ottie`; it's `github.com/MatthiewD/ottie` since the account was renamed (2026-10-20).
