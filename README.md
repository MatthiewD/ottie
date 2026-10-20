<p align="center">
  <img src="res/icons/png/app/ottie-icon-rounded-256.png" alt="" width="128" height="128">
</p>

<h1 align="center">Ottie</h1>

<p align="center">
  <strong>Ottie tucks your files away.</strong><br>
  A download manager for Windows, Linux and macOS, with a web UI and a browser extension.
</p>

<p align="center">
  <a href="https://github.com/MatthiewD/ottie/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/MatthiewD/ottie?include_prereleases&label=release"></a>
  <a href="LICENSE"><img alt="License: MPL-2.0" src="https://img.shields.io/badge/license-MPL--2.0-blue"></a>
</p>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screenshots/dashboard-dark.webp">
  <img alt="Ottie's dashboard: the add bar, an hour of download speed, then the running and waiting downloads" src="docs/screenshots/dashboard-light.webp">
</picture>

Sea otters keep their food and a favourite stone in a pouch of skin; Ottie keeps your downloads safe until they're done.

Ottie started because uGet, the download manager that inspired it, is no longer maintained. It keeps what made uGet good (categories, a queue, a scheduler, speed limits) and adds what a download manager needs today.

> [!WARNING]
> **Ottie is experimental.** The first test release is out for people willing to try it and report what breaks. Read the [disclaimer](#disclaimer) before you rely on it.

## Features

**Downloading**

- **Pause and resume**, even after a restart or a crash. Ottie checks the file hasn't changed on the server before it continues, and never mixes bytes from two versions of a file.
- **Several connections per download**, split again as connections finish, so the slow end of a file doesn't hold up the rest.
- **Retries** after network errors, waiting longer each time; downloads carry on when the Wi-Fi or VPN changes.
- **HTTP, HTTPS, FTP and FTPS**, including FTP servers with their own certificate (you trust it once, from the download).
- **Many links at once**: paste a whole text, pick a file of links, or write a sequence like `photo-[001-120].jpg`.
- **A file already there?** Rename, replace or ask, for every download or one at a time.

**Speed and order**

- **Speed limits** for one download, a category or everything, changed while downloads run.
- **Priorities** (low, normal, high, urgent) and a **queue** with as many downloads at once as you choose; drag to reorder.
- **Play while downloading**: watch a video in your browser, VLC, mpv or another player before it has finished. Ottie fetches the part the player needs next, and the end of the file early (where many videos keep their index).

**Organizing**

- **Categories** with their own folder and file types (videos go to Videos, ISOs to Disk images…), limits and number of downloads at once.
- **Filters, sorting and saved views** in the Downloads list, plus a **dashboard** with the last hour of download speed.
- **Scheduling**: a start or stop time for one download, and weekly **time slots** ("slow down during work hours", "no videos on movie night").
- **When everything has finished**, the desktop app can quit, or put the computer to sleep, hibernate it or shut it down after a countdown.

**Everywhere you are**

- **Desktop app** for Windows, Linux and macOS: lives in the tray, notifications with sounds, starts at sign-in, opens files and their folders, and can watch the clipboard for links (off unless you turn it on).
- **Browser extension** for Chrome, Edge, Brave, Vivaldi and Firefox: takes downloads over from the browser (with the site's cookies, so downloads behind a login work), shows Ottie's list in its popup and adds "Download with Ottie" to the right-click menu.
- **Web UI** from any browser, phone included.
- **Runs without a desktop** too: as a service at sign-in or at boot (systemd, Windows service, launchd) or in Docker, on a server or a NAS.
- **One app, several computers**: switch between the Ottie on this computer and the one on your NAS. Each device gets its own sign-in, full or read-only, that you can remove.
- **43 languages**, including Arabic, Persian and Urdu written right to left. Ottie follows your system's language unless you choose another.
- **Keyboard shortcuts** (press <kbd>?</kbd> to see them), light and dark themes, animations that stay off if your system asks for reduced motion.

## Screenshots

The downloads in these pictures are made up.

### Desktop

| Downloads, with the selected one's details | The same in the dark |
|---|---|
| <img src="docs/screenshots/downloads-light.webp" width="380" alt="The Downloads list grouped by status, the Ubuntu ISO selected, its details below: speed, time left, priority, a bar showing each connection's part of the file"> | <img src="docs/screenshots/downloads-dark.webp" width="380" alt="The same page in the dark theme"> |
| **Categories**: a folder and file types each | **Schedule**: weekly time slots |
| <img src="docs/screenshots/categories-light.webp" width="380" alt="The Categories page: General, Videos, Music, Documents, Software, Disk images and Archives with their folders and file types"> | <img src="docs/screenshots/schedule-light.webp" width="380" alt="The Schedule page: a week with Work hours, Night owl and Movie night slots"> |
| **Settings** | |
| <img src="docs/screenshots/settings-dark.webp" width="380" alt="The Settings page: appearance, language, animations"> | |

### Browser extension

| The popup | The same in the dark |
|---|---|
| <img src="docs/screenshots/popup-light.webp" width="280" alt="The extension's popup: the running downloads with their progress, Open Ottie, Pause all, Resume all"> | <img src="docs/screenshots/popup-dark.webp" width="280" alt="The same popup in the dark theme"> |

### Phone browser

| Dashboard | Downloads, in the dark |
|---|---|
| <img src="docs/screenshots/phone-dashboard-light.webp" width="260" alt="The dashboard on a phone: the add bar, the speed graph, the running downloads; the tabs move to a bar along the bottom"> | <img src="docs/screenshots/phone-downloads-dark.webp" width="260" alt="The Downloads page on a phone, in the dark theme"> |

## Install

Ottie is in its first test release: download it from the [Releases page](https://github.com/MatthiewD/ottie/releases) (pick the latest "Experimental" one).

### Desktop app

| System | File |
|---|---|
| Windows 10 and 11 | `…_x64-setup.exe` (or the `.msi`) |
| Ubuntu, Debian, Mint | `…_amd64.deb` |
| Fedora, openSUSE | `….x86_64.rpm` |
| Any other Linux | `…_amd64.AppImage`: make it executable, then run it |
| macOS on Apple silicon (M1 and later) | `…_aarch64.dmg` |

The installers aren't signed yet, so your system will warn you:

- **Windows:** SmartScreen says "Windows protected your PC": click **More info**, then **Run anyway**.
- **macOS:** after copying Ottie to Applications, open it once, then go to **System Settings → Privacy & Security** and click **Open Anyway**. If macOS says the app is damaged, run `xattr -dr com.apple.quarantine /Applications/Ottie.app` in Terminal. macOS is the least tested system so far.

The app comes with `ottie`, the program that does the downloading. It starts with the app and keeps going in the tray when you close the window. On first start, a short setup (language and look, download folder, categories…) gets you going; every step can be skipped.

### Browser extension

The extension isn't in the browsers' stores yet. Install the desktop app first: the extension then signs in to it by itself.

- **Chrome, Edge, Brave, Vivaldi:** unzip `…_extension-chrome.zip` into a folder you keep. Open the extensions page (`chrome://extensions`, `edge://extensions` or `brave://extensions`), turn on **Developer mode**, click **Load unpacked** and pick that folder.
- **Firefox:** open `about:debugging#/runtime/this-firefox`, click **Load Temporary Add-on…** and pick `…_extension-firefox.zip`. Firefox removes it when it closes, so this needs doing again after each restart. In `about:addons` → Ottie → **Permissions**, allow access to all websites.

Without the desktop app (Ottie on a server, say), sign the extension in with a link from Ottie's Settings → Connection.

### On a server or a NAS

Ottie runs fine without a screen: `ottie serve` downloads and serves the web UI. [Running Ottie as a service](docs/service.md) explains how to start it at boot (systemd, a Windows service, launchd) or run it in Docker, and [Reaching Ottie from other devices](docs/remote.md) how to use it from your phone or laptop safely.

### From source

You need [Rust](https://rustup.rs/) (stable), [Node.js](https://nodejs.org/) 22 or later and [pnpm](https://pnpm.io/). On Linux, the desktop app also needs the WebKitGTK 4.1, AppIndicator and ALSA development packages ([Tauri's list](https://v2.tauri.app/start/prerequisites/)).

```bash
git clone https://github.com/MatthiewD/ottie.git
cd ottie
pnpm install
pnpm --filter ui build                 # the web UI, built into the daemon
cargo build --release -p ottie         # the daemon alone: target/release/ottie
pnpm --filter app build                # or the desktop app and its installer
pnpm --filter extension build          # the extensions: extension/dist/chrome and dist/firefox
```

Then `ottie serve` runs the daemon on 127.0.0.1:24068 and `ottie open` opens the web UI, signed in.

## Coming later

- **Android app**, with the same engine built in: downloads that keep going in the background, and links shared from other apps.
- **Video sites** through yt-dlp.
- **SFTP.**
- **The extension in the browsers' stores** (Chrome Web Store, Edge Add-ons, Firefox Add-ons), so it installs and updates like any other.
- **Signed installers**, and macOS tested as much as Windows and Linux (Ottie as a system-wide service on macOS too).
- **Translations checked by native speakers.** Most languages are drafted and still need a native speaker's eye: help is welcome.

## Disclaimer

- **Experimental software.** Things will break, and what Ottie saves (its database, its settings) may change before 1.0. Keep a copy of anything you can't afford to lose, and please [report what goes wrong](https://github.com/MatthiewD/ottie/issues): your system, what you did, what happened. Ottie comes with no warranty (see the [licence](LICENSE)).
- **Ottie is a network app; mind its security.** It's a program that runs in the background, downloads from the internet and answers on a network port. Out of the box it only answers on your own computer, and every request needs a token. Treat that token, and every sign-in link Ottie gives you, like a password: whoever holds one can control your downloads and save files into your folders. To use Ottie from other devices, go through Tailscale, a VPN or a reverse proxy with HTTPS ([how](docs/remote.md)), never by opening its port to the internet, and remove a device's access when you no longer use it. Download only from sources you trust, and treat downloaded files as you would any other.
- **Use it lawfully.** You're responsible for what you download: respect copyright and the terms of the sites you download from.
- **Not affiliated with uGet.** Ottie is a separate project, inspired by uGet, and isn't made or endorsed by its authors.
- **Built with AI help.** Ottie's code is written with an AI coding assistant ([Claude Code](https://claude.com/claude-code)), directed by its maintainer, who decides what gets built and how, and tests it.

## For developers

- [Talking to the Ottie daemon](docs/api/README.md): the HTTP API every client uses (REST + a WebSocket for events), with its [OpenAPI description](docs/api/openapi.json).
- [Decisions](docs/decisions/README.md): why Ottie works the way it does, one short record per decision.
- Written in Rust (engine, daemon), Svelte and TypeScript (UI, extension), with Tauri for the desktop app.

## License

[Mozilla Public License 2.0](LICENSE).
