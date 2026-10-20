# 0056: Alpha releases

Accepted 2026-10-17. Builds on `docs/decisions/0017` (the desktop app starts `ottie` as its sidecar) and `0055` (the browser extension).

**Why:** testing Ottie on other computers and systems meant building it there from source. Testers need installers, and the extension packaged beside them.

**Decision** (Mathieu's choices: GitHub's machines for all three systems; the Firefox extension unsigned; the label beside the version, not in it):
- **A tag makes a release.** Pushing a tag `v<version>[-<label>]` (`v0.1.0-experimental`) runs `.github/workflows/release.yml`: a draft pre-release named "Ottie 0.1.0 Experimental", its text from `.github/release-notes.md`. The maintainer reads it over and publishes it by hand. The tag's version must be `app/Cargo.toml`'s, or nothing is built.
- **The version stays a number** (0.1.0 in the Cargo packages and the extension; the UI's package, never released on its own, stays 0.0.1): Windows' `.msi` refuses anything else. The label goes in the release's name, the tag and the files' names (`Ottie_0.1.0-experimental_x64-setup.exe`).
- **Installers built on each system** by `pnpm --filter app build` (the UI, the release `ottie` as the sidecar, then the app): Windows `.exe` (NSIS) and `.msi`; Linux `.deb`, `.rpm` and `.AppImage`, built on Ubuntu 22.04 so the AppImage runs on older systems too; macOS `.dmg` for Apple silicon only. One system failing leaves the others' files in the draft.
- **Not signed.** Windows' SmartScreen and macOS' Gatekeeper warn; the notes say how to go past them. macOS gets an ad-hoc signature (`signingIdentity: "-"` in `tauri.bundle.json`): without one, Apple silicon calls the app damaged.
- **The extensions** as zips: Chromium-based browsers load the folder unpacked (its fixed ID still matches the native host's), Firefox loads the zip as a temporary add-on, gone at each restart.

**Alternatives:**
- Building locally (Windows here, Linux in Docker): no macOS, and the files on this computer only.
- Signing the Firefox extension on addons.mozilla.org, unlisted: it would install for good in any Firefox, but needs the maintainer's account keys. Later, with the listing.
- `0.1.0-experimental` as the version: Windows would lose the `.msi`.

**Still to check:** the first run on GitHub (macOS has never been built: it may need fixes); each installer on its system; the app started from an installer finding its sidecar and setting up the extension's native host for the installed `ottie`; the `.rpm` on Fedora, the AppImage on an older distribution; Intel Macs aren't covered.
