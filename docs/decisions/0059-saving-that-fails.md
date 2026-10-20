# 0059: Saving that fails

Accepted 2026-10-20 (Mathieu's choices). Builds on `docs/decisions/0008` (persistence) and `0022` (settings).

**Why:** the database is written on its own thread, and a write that failed (disk full, a drive unplugged, a broken file) was only logged. Nobody was told: a download added, removed or changed looked done, and was gone or back at the next start; a shutdown after such failures answered "all saved".

**Decision:**
- **The store remembers its failures.** Every write that fails is logged as before, and its error's text is kept until a later write works. `Store::flush` returns the first failure since the last flush of a write nobody waited for, and `Manager::shutdown` answers with it, so `ottie serve` ends with that error.
- **What the person asked for waits for its commit** (`Store::write`, beside the queued `Store::queue`): adding downloads (one or several), removing one, the settings, categories, views, time slots and trusted certificates. If the write fails, the request is refused with `500 storageFailed` and nothing changes: the change is saved first and only then made in memory, or, for adding (whose ID and place come first), the downloads are taken out again. Removing a category saves its jobs (moved to General), views and slots first, as the database requires, then waits for its row's deletion: if that fails, the category comes back and its jobs stay in General. Reordering slots saves them one by one: those before a failure have moved. Progress and state changes stay in the background, as before: a download must never wait on the database.
- **Clients are told through the settings:** `storageFailing` in `GET /settings` and in every `settingsChanged`, the last error's English text while writing fails, `null` once it works again. Not a setting: no change sets it, and it's never saved. The pages show a banner across the top while it's there ("Ottie can't save its list of downloads…", with the error under it).

**Alternatives:**
- **An endpoint of its own** (`GET /health` and a `healthChanged` event): cleaner if more such signals come (a full downloads disk, no network), but one more thing every client must fetch and follow. The settings are already followed everywhere.
- **The log only:** nobody reads it in time.
