# 0008: Persistence in SQLite with real columns; running jobs continue after a restart

Accepted 2026-10-12.

**Decisions**
1. **Everything lives in SQLite** (`rusqlite`, SQLite bundled): jobs now, per-segment progress from 2c on. No sidecar files next to downloads, and nothing appended to the partial file.
2. **Real, checked columns.** Tables are `STRICT`, with `CHECK` constraints on enums. States are stored as fixed lowercase names (`queued`, `downloading`…) that don't depend on Rust variant names. Schema changes are numbered migrations in `store.rs`, tracked with `PRAGMA user_version`; a released migration is never edited. A database from a newer Ottie is refused instead of being misread.
3. **Running downloads continue automatically after a restart,** whether Ottie quit or crashed. Jobs the user paused stay paused. Shutdown therefore does *not* mark running jobs Paused: it stops them, saves their progress, and leaves their state as it was. On load, any `connecting`/`downloading` job becomes `Queued`.

**Rules that follow**
- The partial file on disk is the truth for how much we have. On load, `downloaded` is reset from its length, because the saved number may lag behind.
- **From 2c on, file data must be flushed to disk before the progress that covers it is saved.** A crash can then only make us re-download a little, never trust bytes that aren't there.
- When to save: on add, on every state change (inside the single transition function), as soon as a run learns its file name and validator (`Event::Started`), and progress at most every 2 s.
- The connection lives on a dedicated thread (`ottie-store`), because SQLite calls block. The manager queues writes without waiting, and waits for a flush before replying to shutdown.
- `journal_mode=WAL` and `synchronous=NORMAL`: crash-safe, and a power cut can lose only the last few saves.
- A row that can't be read is skipped and logged, so it doesn't stop every other job from loading.
- **A file that fails as it stops** (added 2026-10-20): if writing or syncing fails while a pause or a shutdown stops a download, it still ends Paused, but with the disk's problem, and the pause answers with the error rather than success; one connection's progress is reread from the file's length, so only what's on disk counts.
- **One daemon per data folder** (added 2026-10-20): `ottie serve` takes an exclusive lock on `<data>/ottie.lock` (`File::try_lock`) before opening the database and holds it until it stops. A second one on another port stops at once with a reason, instead of resuming the same downloads into the same files. The system releases the lock when the process ends, crash included, so the file left behind never blocks; a folder that can't be locked (some network shares) runs unguarded, with a warning.

**Alternatives considered:** a sidecar control file per download (aria2-style), a trailer inside the partial file, a JSON blob column, and object/graph databases such as OrientDB. OrientDB is a Java server and can't be embedded in an Android app; embedded Rust object stores give up SQL queries and inspection with standard SQLite tools.

**Not decided here:** where the database file lives (step 3, with the config folders), and removing jobs from history (comes with the API).
