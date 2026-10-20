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
- A download that had to start over, and whose new start then failed (the server busy for a moment), could later be reported complete with parts of the file missing (zeros in their place). Starting over now empties the unfinished file at once, so the next try downloads it all again.
- A file that appeared under a download's name just as it finished (another download, another program) could be replaced by it. The download now takes the next free name, `name (1).ext`, unless it was set to replace.
- Two Ottie using the same data folder on different ports (another `--port`, or a service and a copy started by hand) both carried on the same downloads, writing into the same files. The second one now stops at once and says Ottie is already running with that folder.
- An FTP download from a server that doesn't give files' modification times could, after an interruption, join the start of one version of a file to the end of another of the same size. Such downloads now start again from the beginning instead of resuming, on one connection.
- A pasted text holding a long run of brackets, or a sequence of very long links, could keep Ottie busy for minutes or make it run out of memory while it looked for the links. Reading a paste now takes time in proportion to its length, and links that would add up to more than 64 MiB are refused as too many.
- A video played while it downloaded could, if its player was paused for minutes and the file changed on the server meanwhile, go on with the new version's bytes after the old one's. Playback now stops there instead, as it already did when the player kept reading.
- When Ottie can't save its list of downloads (a full disk, a drive unplugged), its pages now say so in a banner, and quitting reports the error. Adding or removing downloads, and changing settings, categories, saved views or time slots, then says it couldn't be saved, and nothing changes. Before, nothing said so, and such changes were undone at the next start.
- If Ottie stopped abruptly (a crash, a power cut) just as a download over several connections started again from the beginning, the next start could finish it with parts of the file missing (zeros in their place). Its new plan is now saved before the file is prepared.
- A download that finished just before Ottie stopped abruptly is now found finished at the next start. It was downloaded again, as a second copy.
- A download whose file was being moved to its category's folder when Ottie stopped abruptly now ends up in that folder and category at the next start, or stays where it was. Before, Ottie could lose track of the file, or start the download over.
- The desktop app's own settings and its list of other computers' Ottie can no longer be left half written by a crash or a full disk. A file it can't read is now kept aside (as `.bad`) rather than silently replaced by the defaults.
- If a bug in Ottie stopped a download, pausing or cancelling it, and quitting Ottie, could wait forever. That download now fails with a message saying so, and everything else answers as usual.
- A running download given a start time, or stopped by a time slot, that was then cleared or removed while it was still stopping, stayed "downloading" without downloading anything. It now goes back to the queue and starts again.
- When the disk failed just as a download was paused, or as Ottie quit (disk full, a drive unplugged), the pause still seemed to work and nothing said what had gone wrong. The download is now paused with the disk's problem shown, and only what was really written counts.
- A file moved to a category's folder on another drive is now written to that drive for sure before the original is deleted, so a power cut at that moment can't lose it.

### Security

- Secure connections (HTTPS, FTPS) use an updated version of rustls, the library behind them, which fixes four security advisories. None of them could be reached through the way Ottie uses it.
- A device limited to some folders can no longer have files written outside them through a link inside an allowed folder followed by `..` (`link/../elsewhere`). A device's folder containing `..` is now refused.
- Another device, even a read-only one, can no longer find out whether a file exists in a folder it isn't allowed to use, by asking what adding a link there would do.
- The desktop app, the browser extension's sign-in and the `ottie` command now make sure what answers on Ottie's port is really your Ottie before sending it this computer's token. Before, a program from another account that took the port first would have received the token, and the app would have shown its page with access to your files. An Ottie older than this version now has to be restarted on it.
- On Linux and macOS, Ottie's database, which keeps the cookies and FTP passwords given with downloads, can now be read only by the account running Ottie. With Ottie installed as a system service, joining the `ottie` group to read downloads also let you read it. An existing database is fixed at the next start.
- A new token for this computer (`ottie token --new`) always takes effect at once. If Ottie read the token file while it was being written, it kept taking the old token until it restarted. Live connections still open with the old token now close within two seconds too, even when nothing else reaches Ottie meanwhile.

## [0.1.0-experimental] - 2026-10-17

The initial release, for testing. What Ottie does is in the [README](https://github.com/MatthiewD/ottie#features).

[Unreleased]: https://github.com/MatthiewD/ottie/compare/v0.1.0-experimental...HEAD
[0.1.0-experimental]: https://github.com/MatthiewD/ottie/releases/tag/v0.1.0-experimental
