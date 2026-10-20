# 0060: A download sent again

Accepted 2026-10-18 (Mathieu's choice, from the audit's SYN-003), built 2026-10-21. Builds on `docs/decisions/0055` (the extension).

**Why:** the extension hands a browser's download to Ottie with `POST /jobs`, waiting 10 s at most. When no answer comes (a timeout, a connection lost, Ottie busy), it starts Ottie through its native host and sends it again. But "no answer" doesn't mean "not added": Ottie may have added it and the answer got lost. Then the same download was added twice, downloaded twice, and the extension only knew the second one's ID, so it couldn't take back the first if the browser finished first.

**Decision:**
- **`requestId` on `POST /jobs`**, optional: a random ID (the extension sends a UUID), 1 to 64 letters, digits, `-` or `_` (else `400 invalidRequestId`). The same body sent again with the same `requestId` gets the first one's answer (`201`, the same ID), whatever happened to that download since, instead of adding another.
- **Per token:** this computer's and each device's requests are kept apart.
- **Sent again while the first is still being added:** the second waits for it, then answers the same.
- **Only what was added is remembered.** A refused request (a bad link, a folder the device may not use) can be sent again with the same ID, and is checked again.
- **Kept in the daemon's memory for 10 minutes**, at most 10,000 at once (the oldest goes first). A client sends it again within seconds. A restart forgets them: a download added just before a crash and sent again after it is added twice, as before.
- **`POST /jobs/batch` ignores it.** People add several links from a page and see the result; nothing sends a batch again by itself.
- **The extension** gives each download it hands over a `requestId` once (`withRequestId`, `extension/src/lib/daemon.ts`) and keeps it for its second try.

**Alternatives:**
- **No second try in the extension:** the browser keeps the download whenever Ottie doesn't answer in time, even when Ottie only had to be started (the usual case at the browser's first download after signing in). The handover would fail when it's most needed.
- **Finding a duplicate by its link:** the same link may rightly be downloaded twice (a file that changed, another folder), and two requests at once would still both add one.
- **Remembered in the database:** survives a restart, but a crash between adding and answering is rare, and every add would write one more row.
