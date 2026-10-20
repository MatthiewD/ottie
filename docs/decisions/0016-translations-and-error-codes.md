# 0016: Translations (English, French) and error codes from the daemon

Accepted 2026-10-12.

**Languages:** English and French from the start. Two real languages prove the setup: plurals, units, longer French text in the layout. More languages are one JSON file each.

**Library: Paraglide JS** (Svelte's official i18n add-on).
- Messages live in `ui/messages/{locale}.json` and are compiled into typed functions (`m.status_left({ time })`). TypeScript checks the names and placeholders, and only the messages used are shipped.
- A unit test checks that every language has every English message with the same `{placeholders}`.
- The add-on's defaults are for server-rendered sites with the language in the URL. For our single-page app they're replaced:
  - no server hooks and no localized URLs;
  - language strategy `localStorage` (the page's own selector) → `preferredLanguage` (the browser's) → English (replaced in 0041 by Ottie's own choice, then the system's closest language, then English);
  - a language change reloads the page;
  - `<html lang>` is set from the client.
- The compiler's message-format plugin is loaded from the pinned npm package (`./node_modules/@inlang/plugin-message-format`), not from a CDN, so builds work offline and never run code fetched at build time.
- `pnpm check` compiles the messages first (`paraglide-js compile`). The output, `src/lib/paraglide/`, isn't in git.

**Numbers and units** come from the browser's `Intl`, in the page's language: "1.5 kB", "1,5 ko", "2,5 Mo/s", "2 h 5 min". Whole bytes use the long form ("1,023 bytes", "1 023 octets"); larger units the short one. Sizes still count 1 KB = 1024 bytes. Formatting functions take the language as a parameter (the page's by default), so tests check both languages.

**Daemon text: codes plus English.** Clients translate a stable code and fall back to the English text for codes they don't know (a newer daemon).
- **Why a job failed:** the engine's `Problem` replaces the job's error string:
  - `Network {detail}`, `Redirect {detail}`, `HttpStatus {status}`, `Stalled {seconds}`, `Incomplete {received, expected}`, `RemoteChanging`, `Disk {detail}`, `Other {detail}`;
  - `Display` gives the English wording, unchanged;
  - migration 6 stores it as `problem_kind` / `problem_number` / `problem_total` / `problem_detail`, next to the English `error`. Older failures become `other` with their text, and a problem that doesn't add up is read back as `other` rather than dropped;
  - the API sends `problem` (`{code: "httpStatus", status: 404}`) next to `error`. Stalls round up to whole seconds;
  - network and disk details stay the system's English text;
  - a download's task that crashes (a panic: a bug in Ottie) fails the job with `Other`, its text "Ottie hit a bug and stopped this download: …" in English, rather than a kind of its own: it should never happen, and a kind means a migration and a text in every language (added 2026-10-20).
- **Why a request was refused:** every error answer is `{"error": "English", "code": "...", ...details}`. Codes: `unauthorized`, `noSuchEndpoint`, `invalidBody`, `invalidUrl`, `relativeFolder`, `unsupportedScheme {scheme}`, `nonUtf8Path`, `noFolder {category}`, `invalidJobId {id}`, `jobNotFound {id}`, `invalidTransition {from, to}`, `categoryNotFound {id}`, `invalidCategoryName`, `invalidExtension {extension}`, `categoryNameTaken {name}`, `extensionTaken {extension, category}`, `cannotRemoveBuiltIn {category}`, `stopping`, `internal`. The UI translates those its screens can trigger.
- Some common statuses get their own sentence (403, 404, 410, 429, 5xx).

**Not translated:** the `ottie` command line and the daemon's logs stay in English.
