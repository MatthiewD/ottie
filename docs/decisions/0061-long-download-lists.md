# 0061: Long download lists

Accepted 2026-10-21 (Mathieu's choice, from the audit's PERF-R6). Builds on `docs/decisions/0048` (animations).

**Why:** the Downloads page drew a row for every download, and redid work over all of them every second. Measured in Edge with only finished downloads: 1,000 appeared after 0.9 s (102,000 elements, 86 MiB, 4.6 % of a core while idle); 10,000 after 7.8 s (a million elements, 721 MiB, a third of a core while idle, 0.4 s to sort or select); 100,000 not after 15 minutes. People who never clear their finished downloads reach 10,000 in a year or two.

**Decision:**
- **Up to 200 downloads** the list draws every row, as before, with every animation.
- **Above 200** it draws only the rows in view and 600 px more above and below (`rows.ts`, `JobTable`); two empty stand-in rows keep the room of the others, so it scrolls as if they were all there. A row's height, a heading's and the space between rows are measured on the page (the phone layout's differ), so the stand-ins match.
  - A row scrolled out of the window goes at once: its fade (`leave`) would play as it scrolls away. Rows that move, come or go still slide as in 0048, among those drawn.
  - The arrow keys' cursor (0046) scrolls to its row before it's drawn (`JobTable.reveal`). Dragging in the queue works on the rows drawn, and scrolls them near the edges as before.
- **Less work every second:** the date filters (today, the past 7 days) go by the minute, and only those read the clock, so the list isn't filtered and sorted again each second; the running count in the Waiting group's heading is apart from the speed. Moving up or down in the queue isn't worked out with nothing selected.
- **Sorting** uses one collator (`byCreation`, and one per sort for names) instead of `localeCompare` with options, which made one for each comparison: sorting 10,000 downloads went from 370 ms to 26 ms.

**Measured after,** same machine and lists: 1,000 appear after 0.4 s; 10,000 after 0.5 s (under 80 MiB, 0.2 % of a core idle, 27 ms to sort); 100,000 after 1.5 s (410 MiB, 0.2 % idle, 0.3 s to sort, a 43 MiB `GET /jobs`). Scrolling through them, on a desktop's width and a phone's, the rows drawn always cover what's in view, and the arrow keys bring their row into view.

**Not changed:** the daemon still sends every download in `GET /jobs` and keeps them all in memory (100,000: started in 0.18 s, listed in 36 ms, audit PERF-R5), and screen readers see only the rows drawn.

**Alternatives:**
- **"Show more" per group** (the first 200 of each, then a button): simpler and keeps every animation, but a long group still costs as much once opened.
- **A supported size** (~2,000) and Clear finished: no code, but the page stays unusable past it.
- **Paging in the API:** less to send, but filters, sorting and the four groups work on the whole list in the page today; a big change for a case that now works.
