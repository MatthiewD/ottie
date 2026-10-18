# 0057: The changelog

Accepted 2026-10-17. Builds on `docs/decisions/0056` (alpha releases).

**Why:** from 0.1.0 Experimental on, testers and users need to know what each version brings, changes or fixes, without reading the commits.

**Decision:**
- **`CHANGELOG.md`** at the root, in the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format: an `## [Unreleased]` section at the top, then one `## [<version>[-<label>]] - <date>` per release, newest first, its entries under `### Added`, `### Changed`, `### Removed`, `### Fixed` or `### Security`. The heading carries the tag's label (`[0.1.0-experimental]`), so it matches the tag.
- **Written for the people using Ottie**: what they'll notice, in plain words, not the commits. 0.1.0 Experimental, the initial release, only points to the README's features.
- **Kept as we go:** a commit that changes what people get (a feature, a change in behaviour, a fix of a released bug) adds its line under Unreleased in the same commit. Changes nobody using Ottie sees (refactors, tests, CI, developer docs) and fixes of something not released yet get none.
- **A release** renames Unreleased to its version and date, starts a new empty Unreleased and updates the links at the bottom, before the tag is pushed.
- **The release's text** is `.github/release-notes.md` followed by that version's section under "What's new" (`release.yml`); a tag with no section in the changelog builds nothing.

**Alternatives:**
- Generating it from Conventional Commits (git-cliff, release-please): commits are written for developers, one per slice; a list of them doesn't tell a tester what changed for them.
- Only the GitHub release's text: lost from the repository, and nothing makes anyone write it.
