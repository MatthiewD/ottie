# Changelog

What changes in Ottie from one version to the next: new features, changes and fixes, for the people using it. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); a version's label (Experimental) is part of its heading, as in its tag.

## [Unreleased]

### Fixed

- Speed limits above 1 TiB/s and start or stop times after the year 9999 are refused. Before, such a limit was accepted but couldn't be saved, so it was lost at the next start along with the settings changed after it, and such a time made the request fail on Windows.
- A link with a sequence spanning every possible number, like `[0-18446744073709551615]`, is refused as too many links instead of giving no download at all.
- When a time slot for all downloads lets running ones finish and another, for one category, stops them at once, that category's running downloads now stop.
- A download that keeps failing no longer gives its place to one whose file is being moved to another folder: it took the place straight back and tried again without waiting.
- `ottie service install` with a relative data folder (`--data-dir state`) now writes the service's log in that folder. On Windows and macOS it went to the folder the service started in, or the service couldn't start.
- With "Quit Ottie" chosen for when everything has finished, a download added in the few seconds before it quits now keeps Ottie open.
- Opening Ottie while it's still starting in the tray at sign-in now shows its window once it's ready. Before, nothing happened.
- In the desktop app, another computer's Ottie removed while the window was still switching to it is no longer shown anyway, and after several switches in a row, the last one chosen is the one shown.
- On Linux, the system's tray menu greys out Pause all, Resume all and the speed limit for another computer's Ottie added with a read-only link, as Ottie's own menu does. They did nothing.
- In Firefox, when the browser finished a download before Ottie took it over, Ottie's copy is now removed. Ottie downloaded the file a second time.
- In a browser that doesn't let pages keep data, another computer's Ottie added to the list now opens when chosen. Before, choosing it did nothing.

### Security

- Secure connections (HTTPS, FTPS) use an updated version of rustls, the library behind them, which fixes four security advisories. None of them could be reached through the way Ottie uses it.
- A new token for this computer (`ottie token --new`) always takes effect at once. If Ottie read the token file while it was being written, it kept taking the old token until it restarted.

## [0.1.0-experimental] - 2026-10-17

The initial release, for testing. What Ottie does is in the [README](https://github.com/Matfalco/ottie#features).

[Unreleased]: https://github.com/Matfalco/ottie/compare/v0.1.0-experimental...HEAD
[0.1.0-experimental]: https://github.com/Matfalco/ottie/releases/tag/v0.1.0-experimental
