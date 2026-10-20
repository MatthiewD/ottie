# Changelog

What changes in Ottie from one version to the next: new features, changes and fixes, for the people using it. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); a version's label (Experimental) is part of its heading, as in its tag.

## [Unreleased]

### Fixed

- Speed limits above 1 TiB/s and start or stop times after the year 9999 are refused. Before, such a limit was accepted but couldn't be saved, so it was lost at the next start along with the settings changed after it, and such a time made the request fail on Windows.
- A link with a sequence spanning every possible number, like `[0-18446744073709551615]`, is refused as too many links instead of giving no download at all.
- When a time slot for all downloads lets running ones finish and another, for one category, stops them at once, that category's running downloads now stop.
- A download that keeps failing no longer gives its place to one whose file is being moved to another folder: it took the place straight back and tried again without waiting.
- `ottie service install` with a relative data folder (`--data-dir state`) now writes the service's log in that folder. On Windows and macOS it went to the folder the service started in, or the service couldn't start.

### Security

- Secure connections (HTTPS, FTPS) use an updated version of rustls, the library behind them, which fixes four security advisories. None of them could be reached through the way Ottie uses it.
- A new token for this computer (`ottie token --new`) always takes effect at once. If Ottie read the token file while it was being written, it kept taking the old token until it restarted.

## [0.1.0-experimental] - 2026-10-17

The initial release, for testing. What Ottie does is in the [README](https://github.com/Matfalco/ottie#features).

[Unreleased]: https://github.com/Matfalco/ottie/compare/v0.1.0-experimental...HEAD
[0.1.0-experimental]: https://github.com/Matfalco/ottie/releases/tag/v0.1.0-experimental
