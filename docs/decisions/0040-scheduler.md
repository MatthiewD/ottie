# 0040: Scheduler

Accepted 2026-10-14.

**Why:** people download at night (off-peak hours, a data cap), slow downloads down while they work or play, start one big file later, and leave the computer to turn itself off once everything is done. uGet has a weekly grid and IDM queues that start and stop at set times; Ottie replaces both.

**Decision** (Mathieu's choices):
- **Time slots**, in their own **Schedule** tab, in a list:
  - A slot applies to the **whole queue or one category**.
  - It **repeats** on chosen weekdays, or happens **once** on a date (and removes itself after it ends). It runs from one time to another; an end before the start means the next day.
  - It does any of: **allow or stop** downloads, a **speed limit** (the total one for the queue, the category's for a category), a **max concurrent**.
  - When a slot stops downloads, the ones running **pause**, or the slot can **let them finish** (only new ones don't start). When it ends, those it held **carry on**, or the slot can keep them **paused**, to be resumed by hand (added 2026-10-15). Switching the slot off or removing it always lets them carry on.
  - The editor asks what a slot does to downloads in three choices, each with a line saying what it means: **only limit** (speed or how many; never starts or stops one), **run** (even where a lower slot stops them; limits optional) or **stop** (then its two choices above; no limits).
  - **The slot higher in the list wins** where two apply to the same thing at the same time, for each of the three effects; the tab warns about such conflicts, and about any two slots that overlap for the same downloads, in the list and in the editor before saving (added 2026-10-15). A queue slot and a category slot both apply (a stopped queue stops every category). When both stop downloads, running ones finish only if both let them (added 2026-10-20).
  - **Outside every slot, the usual settings apply.** A slot never changes them; it only overrides them while it lasts. As everywhere, a lower max concurrent never stops running downloads.
  - A slot has an optional **name** ("Night"), an **on/off switch** (kept but never applies when off), and the same time twice means a whole day.
  - **The tab** shows the week (a column per day, each slot a block, side by side where they share a day, overridden ones dashed) above the list, edited in place as categories are. In the week, a slot can be dragged to other hours or days, or by its top or bottom edge to start or end at another time (by quarters of an hour; its start stays on its day); a click opens it in the editor (added 2026-10-15). The title bar's speed chip says when a slot stops or limits all downloads right now, and a held download says which slot holds it.
  - **API:** one resource per slot, like categories and views: `GET /slots` (the list in order, with the time zone), `POST /slots`, `PUT`/`DELETE /slots/{id}`, `POST /slots/reorder` (every ID, once, in the new order); `slotChanged`/`slotRemoved` events, also when a slot starts or stops applying. Removing a category removes its slots.
- **A download's own start and stop time:** it waits until its start time, then joins the queue at its place; at its stop time it pauses (if it hasn't started by then, too). Each time is used once, then cleared.
- **A new state, `Scheduled`:** a download waiting for its start time, or held by a slot.
  - Queued ↔ Scheduled; Connecting | Downloading | Retrying → Scheduled (a slot stops it); Scheduled → Paused | Cancelled; Paused | Failed → Scheduled (resumed while held).
  - It's saved like the other states; at start, the daemon checks again whether each one is still held.
  - In the Downloads page it's one of the Waiting group's statuses, shown with its start time or the slot holding it.
  - Starting a scheduled download by hand clears its start time; if a slot holds it, it runs anyway, and slots leave it alone until it pauses, finishes or fails (a saved flag on the job). Which slot holds a download is saved on it, so that a slot keeping downloads paused does so even when it ended while Ottie wasn't running.
  - Pausing it clears its start time too, so the tray's "Pause all" leaves scheduled downloads alone ("Resume all" couldn't give their times back).
- **When everything has finished** (desktop app): quit Ottie, sleep, hibernate or shut down.
  - Chosen **once** (from the tray menu and the Downloads page; cleared after it runs) or **permanently** in Settings. Both are this computer's settings, like notifications (`docs/decisions/0035`).
  - Before sleep, hibernate or shut down, a **countdown** with Cancel; its length is a setting (60 s at first).
  - **"Finished"** means nothing is queued, connecting, downloading, retrying or scheduled. A setting leaves scheduled downloads out ("nothing left to run"). Paused and failed downloads never count.
  - Added 2026-10-15 (slice 6c, Mathieu's choices):
    - It happens when the **last download finishes or fails**: pausing, cancelling or removing the last one isn't finishing. A download that starts again during the countdown stops it, and a choice for once is kept for then; Cancel uses it up.
    - The countdown shows in **a small window** on top of everything (the daemon's `/finish` page: seconds left, Cancel with the focus, "now"), or, as a setting, **as a notification**; then Cancel is in the tray menu and the Downloads page. Its length: 10 s to 10 min.
    - The Downloads page has a **toolbar button** ("Shut down when finished", lit while one is chosen; during the countdown it counts down and its menu starts with Cancel). The tray menu lists the choices under one item, and Cancel during a countdown; so does the system's menu (Linux).
    - What this computer **can't do is shown greyed out**, with why (no sleep state, hibernation off, no right to shut down); one chosen that can't be done when the time comes is told by a notification.
    - **The system's own calls**: on Windows `SetSuspendState` (sleep, hibernate; `GetPwrCapabilities` says which it has) and `ExitWindowsEx` (shut down, programs may still save; after switching on the shutdown privilege), through `windows-sys`, already built for Tauri; on Linux logind (`systemctl suspend|hibernate|poweroff`, `busctl` asks what it allows). Not `rundll32 powrprof.dll,SetSuspendState`, which hibernates instead when hibernation is on, nor a crate that can't say what's available. macOS later.
    - Quitting waits 3 s, for the last notification's sound. `OTTIE_POWER_DRY_RUN` makes the countdown end with a line on stderr instead, for trying it out.
- **Times are those of the computer running the daemon** (the tab says which), and follow its wall clock across daylight-saving changes: a time the clocks skip counts as the same time after the change (02:30 is 03:30 when they go forward at 02:00); one they repeat counts the first time. A download's own times are exact instants (saved in UTC).
- **Through `jiff`** (0.2) for the time zone and its daylight-saving rules: Rust's standard library has neither. It reads the system's time zone on Windows, Linux, macOS and Android, and handles skipped and repeated times by design. The daemon checks the clock at the next start or end due, and at least every 30 s, so sleep or a clock change can't make it miss one.

**Alternatives:**
- uGet's weekly grid of hours: simple, but too rigid (whole hours, one scope, one effect per hour).
- Lower in the list wins, the most restrictive wins, or overlaps refused: Mathieu preferred list order with a warning.
- A held download as Queued with a reason (only the times saved, the hold worked out from the clock), or as Paused with who paused it: fewer changes, but a state of its own is explicit and checked by the state machine (`JobState::can_become`).
- A start time only per download: Mathieu also wanted a stop time.
- "Finished" ignoring scheduled downloads by default: chosen as the setting, not the default.
- The slots as one list replaced whole (`GET`/`PUT /schedule`): simpler, but Mathieu preferred a resource per slot, like categories and views.
- `chrono` with `iana-time-zone` and `chrono-tz`: the long-standing choice, but three crates, and skipped or repeated times handled by hand.
- The list alone, without the week view: less to build, but overlaps and gaps are easier to see on the week.
