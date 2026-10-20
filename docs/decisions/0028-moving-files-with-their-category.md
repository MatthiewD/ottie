# 0028: A download's file moves with its category

Accepted 2026-10-13. Changes what `docs/decisions/0011` said about moving a job to another category.

**Decision** (Mathieu's choice): moving a download to another category also moves its file into that category's folder, whatever state it's in.
- **Finished file:** it moves under its own name, or the first free one (` (1)`, ` (2)`…) if that name is taken there.
- **Unfinished download:** its `.part.ottie` moves the same way, and resuming continues in the new folder.
- **Nothing on disk yet:** it will simply be saved in the new folder (as before).
- **Running download:** it stops, its file moves, and it carries on at once in the same slot. A waiting download can't take that slot meanwhile. It resumes with `If-Range` as after any pause, so it never mixes two versions of the file.
  - A download that can't resume (no range support, or no validator) would start over, so moving it is refused while it runs (`cannotMoveWhileRunning`). It can move once it has finished.
- **A category without a folder** (Other, or a category set up without one): the file stays where it is, and only the category changes.
- **Deleting a category does not move files.** Its downloads go to General and stay where they are on disk, as before. If the category is deleted while one of its downloads is moving in, that download goes to General too, its file staying where it went.

**How it moves:**
- The new name is reserved as a `.part.ottie` in the new folder (atomic, like a new download), and the file is renamed onto it. A finished file then gets its real name back.
- **Between drives,** where a rename can't work, the file is copied, and the original is deleted only once the copy is complete and synced to the disk (its folder too on Unix; added 2026-10-20). A failure, a crash or a power cut halfway leaves the original intact.
- **Where it's going is saved before it goes** (added 2026-10-20, `docs/decisions/0059`): the name is reserved in the new folder first, then the category, folder and name are saved (`moving_to`, awaited), and only then does the file move. At the next start, a move a crash cut short is settled: if the file is still where it was, the reservation (or a copy begun) goes and the job stays; else the job takes its new category and place, and a finished file still under its reserved `.part.ottie` name gets its own. If a failed move can't even put the file back, it's logged and `moving_to` stays saved, so the next start settles it rather than lose track of the file.
- The move runs in a task of its own, so a long copy doesn't hold up the daemon. Meanwhile the download is `moving` (in the API and in its `changed` events), and the UI says "Moving to its new folder…". Pause, resume, cancel, remove and another move are refused with `jobMoving`.
- `POST /jobs/{id}/category` answers once the file has moved. If it couldn't (disk full, no permission), the download stays in its old category and the request answers `moveFailed` with the reason.
- **Shutdown** waits for moves under way. A running download that was stopping to move when Ottie quit keeps its old place and continues there next time.
- A file deleted by hand has nothing to move: the download just takes the new folder.

**Not tested automatically:** a real move between two drives (the tests have one disk). The copy-then-delete step is tested on its own.

**Alternatives:**
- Move a running download only when it next stops (finishes, pauses or fails): no interruption, and it works for downloads that can't resume. But the file sits in the old folder meanwhile, and the pending move has to be saved in case Ottie restarts.
- Keep files where they are, as before: simplest, but the category's folder then no longer tells where its files are.
