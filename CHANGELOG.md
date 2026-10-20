# Changelog

What changes in Ottie from one version to the next: new features, changes and fixes, for the people using it. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); a version's label (Experimental) is part of its heading, as in its tag.

## [Unreleased]

### Security

- Secure connections (HTTPS, FTPS) use an updated version of rustls, the library behind them, which fixes four security advisories. None of them could be reached through the way Ottie uses it.

## [0.1.0-experimental] - 2026-10-17

The initial release, for testing. What Ottie does is in the [README](https://github.com/Matfalco/ottie#features).

[Unreleased]: https://github.com/Matfalco/ottie/compare/v0.1.0-experimental...HEAD
[0.1.0-experimental]: https://github.com/Matfalco/ottie/releases/tag/v0.1.0-experimental
