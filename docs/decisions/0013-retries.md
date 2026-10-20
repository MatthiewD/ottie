# 0013: Retrying failed downloads

Accepted 2026-10-12.

**What is retried:** errors that may go away. These are network errors, a connection that drops mid-file, a stalled server, 5xx answers, 408 and 429 (`Error::is_retryable`). Other 4xx answers (404, 403…), bad redirects and disk errors fail at once.

**Stalls:** a server that sends nothing for `HttpConfig::stall_timeout` (60 s) while we're waiting for it counts as a dropped connection. The timeout covers the headers and every body read, but not the time our own speed limiter makes us wait. That's why it's done with `tokio::time::timeout` around each read rather than reqwest's `read_timeout`, whose timer keeps running while we deliberately don't read. Connecting has a fixed 30 s timeout.

**The `Retrying` state** is visible, with `retries` (which retry it's waiting for) and `retry_at` in the snapshot, plus the last error. From there: Connecting (the wait is over), Queued (it gave way), Paused or Cancelled.

**Slots** (Mathieu's rule):
- A retrying job keeps its download slot for its first `retries_before_yield` retries (default 3).
- After that, if a queued job is waiting for the slot it holds (one of the total's, or its category's when that category is at its own cap), it gives way: it goes back to Queued, behind every job that never gave way, whatever their priority (otherwise it would take the slot straight back).
- With nobody waiting, it keeps its slot and goes on retrying.
- After `max_retries` retries (default 10, 0 = never) it fails with its last error.

**Waits:** the first wait is `first_wait` (default 2 s), then doubling up to a minute (or `first_wait` if that's longer). A `Retry-After` in seconds can ask for longer, up to 10 minutes; the HTTP-date form is ignored. No wait is longer than a day.

**Counting:** `retries` resets to 0 as soon as bytes arrive, so a long download on a flaky line doesn't run out. It also resets when the user resumes a failed job. The count, the wait and the place in the queue aren't saved. After a restart a `retrying` job is Queued again and tries at once, like a job that was running.

**Settings:** `RetryPolicy { max_retries, retries_before_yield, first_wait }`, part of `Settings`, saved in the settings row (`retry_wait_ms`). Per-category values can come later as columns on `categories`.

**Schema:** migration 5 rebuilds `jobs` to allow `'retrying'` in its `CHECK` (SQLite can't change a constraint in place). It creates the new table, copies the rows, drops the old one and renames, with foreign keys off. `segments` keeps pointing at `jobs` by name.

**Alternatives considered:** retrying inside the HTTP code while the job stays Downloading (invisible to the UI, and a dead server would block a slot for minutes), and always freeing the slot while waiting.
