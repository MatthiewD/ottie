# 0017: The first desktop app (Tauri 2)

Accepted 2026-10-12.

**The window shows the daemon's own page.** It opens `http://127.0.0.1:24068/#token=…`, signed in like `ottie open`.
- Why not a copy of the UI inside the app: the UI and the daemon can never be on different versions, and the daemon needs no CORS and the UI no "daemon address" setting.
- The page gets no access to Tauri: no capabilities are declared, and nothing is granted to the daemon's origin. (Since 0019, the page may control its own window, for the title bar it draws. Nothing else.) Features like the tray and notifications will run on the Rust side, and anything the page needs later (a folder picker) is granted to it explicitly.
- The window only navigates within the daemon's origin. A link elsewhere must not turn it into a browser.
- The app learns the address and token from `ottie link` (new; also handy for another browser). It never reads Ottie's files itself.

**The daemon stays its own process** (CLAUDE.md, Architecture): `ottie serve --managed`, the app's sidecar.
- `--managed` (hidden from `--help`): the daemon prints `ready` on stdout once it listens, and stops gracefully when its stdin closes. The OS closes that pipe if the app crashes, so a daemon the app started never outlives it.
- **Start first, then probe.** On Windows, connecting to a port nobody listens on takes 2 s to fail, so probing on every launch would add 2 s. `ottie serve` now binds its port before it opens the database (otherwise a second daemon would resume the first one's downloads), so a second one gives up at once. If it fails and an Ottie answers at the address (200, or 401 for a daemon with other folders), the app uses that one.
- **Closing the window quits the app.** It stops the daemon it started: a stdin close, then a kill after 10 s; the next start resumes from the files. A daemon that was already running (a terminal, a service) keeps running. Once the tray exists (phase 4), closing will hide to the tray instead. (Done in 0034: closing hides, "Quit Ottie" in the tray quits, and a setting can make closing quit again.)
- **A daemon the app started that stops unasked is started again** (added 2026-10-20, after the audit): a crash, or ended from the task manager. The app waits 1 s, then 5 s, then 30 s between tries; one that ran a minute or more starts that count over. Then it gives up and says so in a notification, with the reason. It's the same address and token, so the window, the tray and the page carry on with the new one by themselves. Never as the app quits, and not once another Ottie answers there (a terminal, a service): that one isn't the app's. A daemon the app found running isn't watched; its own service manager restarts it, or not.
- One app at a time (`tauri-plugin-single-instance`): a second launch brings the window forward. Otherwise its close could stop the daemon under the first window.
- If Ottie can't start, a dialog shows the daemon's last error line (`tauri-plugin-dialog`, also needed later for folder pickers). That message is in English for now: the app has no texts of its own yet. App-side translations come with the tray menu (0034: `app/messages/`; this dialog still shows the daemon's English error).

**Packaging.**
- Bundled with `externalBin`, so the installer puts `ottie(.exe)` next to the app. That setting lives in `app/tauri.bundle.json`, used only by `pnpm --filter app build`, because tauri-build fails when the sidecar file is missing, and that would break `cargo build --workspace`. In development, `ottie` is found in the same `target/` folder.
- `scripts/sidecar.mjs` builds the UI, then the release `ottie`, and copies it to `binaries/ottie-<target>`.
- The app's binary is `ottie-app` (`mainBinaryName`): the default, "Ottie.exe", would clash with the sidecar `ottie.exe`, because Windows file names ignore case.
- The Windows icon is built from the drawn PNGs in `res/icons` (`pnpm --filter app icon`), so 16 and 32 px keep their simplified design. Linux uses the PNGs directly.
- Windows installer: NSIS, per user (no admin rights). Installer choices (MSI, signing, updates) come with the first public release.
- The identifier `io.github.matfalco.ottie` also names the WebView's data folder on Windows (`%LOCALAPPDATA%\io.github.matfalco.ottie`), where the page keeps its token and language.

**Tools.** Tauri 2, the stable line (3.0 is in alpha). The Tauri CLI is a pinned dev dependency of the pnpm package `app` (`@tauri-apps/cli`, prebuilt), not `cargo install tauri-cli`, which compiles for minutes and isn't pinned per project.

**Later:** connecting to a remote daemon (done in 0053: a list, switched from the title bar), the tray, notifications, autostart (done in 0039), and window size and position remembered.
