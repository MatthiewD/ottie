# 0037: Sequences in one link

Accepted 2026-10-14. Follows 0036, which chose the syntax.

**Why:** a numbered series (`page001.jpg` to `page120.jpg`, `part1.rar` to `part9.rar`) is the most common batch, and typing 120 links isn't an option. uGet has a URL sequence form; Ottie writes the series in the link itself, as curl does.

**Decision:**
- **Curl's syntax**, in any link the daemon reads (`POST /jobs/preview`, so in the several-links dialog and wherever a later client sends text):
  - `[1-120]`: numbers. A leading zero pads every number to the first one's width (`[001-120]` → `001` … `120`, `[08-10]` → `08`, `09`, `10`).
  - `[a-z]`, `[A-Z]`: letters, both in the same case.
  - `:step` after either: `[0-100:10]`, `[a-z:2]`.
  - A range counts down if its last value is below the first (`[10-1]`); curl refuses those, but counting down costs nothing and reads as written.
  - `{small,large}`: a list of two items or more; items can be empty (`img{,_hd}.png`). `{id}` alone stays as written: a template, not a list.
  - Several in one link give every combination, the last one changing fastest (`s[1-2]/{a,b}` → `s1/a`, `s1/b`, `s2/a`, `s2/b`).
  - They work anywhere after the scheme, the host included (`img{1,2}.example.com`).
- **Only well-formed ones count**: anything else stays as written (`http://[::1]/`, `a[]=1`, `b[0]=2`, `[1-3:0]`, `{a,b`). No nesting: a list holds no brackets or braces. `\[`, `\]`, `\{`, `\}` (and `\,` in a list) are the character itself, in any link.
- **Counted before expanded**: each link as written counts once, a sequence as all the links it gives. Over `maxBatch` refuses them all at once (`tooManyLinks`, with that count), before any link is made, so `[1-999999999999]` costs nothing. So do links that would add up to more than 64 MiB of text (long links in a sequence), with `max` then saying how many of them would fit (added 2026-10-20). Reading a paste takes time in proportion to its length: a range is at most 64 characters as written (two 20-digit numbers and a step), so `[` is never read again from each one, and it's read off the daemon's async threads.
- **Then each link is one found link**, as if written out: each once in the whole text, with its own name, category and `known`. A sequence whose scheme can't be downloaded (`ftp://…/[1-100]`) is reported once, as written.
- **The UI**: a link with a sequence, pasted into the link field or typed there and sent, opens the several-links dialog, which lists every link it gives. The dialog's hint shows the syntax.
- **A missing number** (404) is an ordinary failed download.

**Alternatives:**
- The UI expanding sequences: every other client would have to do it again (0036).
- Refusing malformed sequences as invalid links: `[` and `{` are legal in links, so a link that only looks like one would be refused for no reason.
- Expanding in the manager's actor: a 100,000-link sequence would hold up every other command meanwhile; it's done in the caller's task, after reading the cap.
