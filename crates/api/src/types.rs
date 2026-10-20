//! What goes over the wire. These are the API's own types, converted from
//! the engine's, so the engine can change without breaking clients. `ts-rs`
//! writes a TypeScript version of each into `shared/api-types/` when the
//! tests run (`cargo test -p ottie-api`); clients use only those.
//!
//! Byte counts and times are `u64` in Rust but plain `number`s in
//! TypeScript (JSON has no bigger integers); they stay far below 2^53.
//!
//! `utoipa` describes them in the OpenAPI spec (`docs/api/openapi.json`).
//! It ignores serde's `rename_all_fields`, so fields inside the tagged
//! enums (`Problem`, `Reason`, `Event`) stay one word each; a test checks.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ottie_core::JobSnapshot;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

use crate::error::Failure;
use crate::settings::{SPEED_LIMIT, TIME, in_range, out_of_range};

/// One download, as listed by `GET /jobs` and sent in `changed` events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Job {
    /// Like `2026.10.12-14.03.07-1`; sorting by it sorts by creation.
    pub id: String,
    /// The link, without a password it holds (FTP: `ftp://user@host/…`).
    pub url: String,
    pub dest_dir: String,
    /// Known once the server has answered.
    pub file_name: Option<String>,
    pub state: JobState,
    #[ts(type = "number")]
    pub downloaded: u64,
    /// Size in bytes, if the server said.
    #[ts(type = "number | null")]
    pub total: Option<u64>,
    /// Whether pausing keeps what's downloaded.
    pub resumable: bool,
    /// Why it failed, or the last failure while `retrying`, in English.
    pub error: Option<String>,
    /// The same, as a code to translate (see `Problem`).
    pub problem: Option<Problem>,
    pub priority: Priority,
    /// Its place in the queue: among the waiting downloads of its priority,
    /// lower places start first. Only the order means something.
    #[ts(type = "number")]
    pub place: u64,
    /// Bytes per second; 0 = no limit of its own.
    #[ts(type = "number")]
    pub speed_limit: u64,
    /// Streaming mode: over several connections, the file comes in order,
    /// its last 1 MiB first, so it can play while downloading.
    pub streaming: bool,
    pub category: u32,
    /// Failed attempts since bytes last arrived.
    pub retries: u32,
    /// While `retrying`: when, in milliseconds since 1970 (UTC).
    #[ts(type = "number | null")]
    pub retry_at: Option<u64>,
    /// Downloads over several connections: each connection's range, as last
    /// saved (`progress` events bring them live). Empty for one connection.
    pub ranges: Vec<ByteRange>,
    /// Its file is moving to another category's folder. Until it's done, it
    /// can't be paused, resumed, cancelled, removed or moved again.
    pub moving: bool,
    /// Downloading, but the speed limits give it no bandwidth for now: other
    /// downloads take it all (urgent ones first).
    pub waiting_for_bandwidth: bool,
    /// While `scheduled`: when it joins the queue, in milliseconds since
    /// 1970 (UTC).
    #[ts(type = "number | null")]
    pub start_at: Option<u64>,
    /// When it pauses if it hasn't finished by then, the same way.
    #[ts(type = "number | null")]
    pub stop_at: Option<u64>,
    /// While `scheduled`: the time slot that keeps it from running, if one does.
    pub held_by: Option<u32>,
}

/// A part of the file that one connection fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ByteRange {
    #[ts(type = "number")]
    pub start: u64,
    /// Exclusive.
    #[ts(type = "number")]
    pub end: u64,
    /// Bytes from `start` that have arrived.
    #[ts(type = "number")]
    pub done: u64,
}

impl From<ottie_core::Segment> for ByteRange {
    fn from(segment: ottie_core::Segment) -> Self {
        ByteRange {
            start: segment.start,
            end: segment.end,
            done: segment.done,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum JobState {
    Queued,
    Connecting,
    Downloading,
    Paused,
    Completed,
    Failed,
    Cancelled,
    Retrying,
    /// Waiting for its start time (`startAt`), then it joins the queue.
    Scheduled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum Priority {
    Low,
    Normal,
    High,
    Urgent,
}

/// Body of `POST /jobs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddJob {
    pub url: String,
    /// An absolute folder. Default: the category's folder.
    #[serde(default)]
    #[ts(optional)]
    pub dest_dir: Option<String>,
    /// Default: matched by the file's extension.
    #[serde(default)]
    #[ts(optional)]
    pub category: Option<u32>,
    /// Streaming mode. Default: on for videos and music if the settings'
    /// `streamMedia` is, by the link's extension, then by the file's real
    /// name once the server answers.
    #[serde(default)]
    #[ts(optional)]
    pub streaming: Option<bool>,
    /// Wait until then (`scheduled`), in milliseconds since 1970 (UTC).
    /// Default, or a time already past: queued at once.
    #[serde(default)]
    #[ts(optional, type = "number")]
    pub start_at: Option<u64>,
    /// Pause then if it hasn't finished, the same way. After `startAt`.
    #[serde(default)]
    #[ts(optional, type = "number")]
    pub stop_at: Option<u64>,
    /// What to do if a file with its name is already in its folder, when the
    /// server answers. Default: the settings' `existing`.
    #[serde(default)]
    #[ts(optional)]
    pub existing: Option<Existing>,
    /// For a download a browser hands over (the extension): its `Cookie`
    /// header for this link, sent again with every request of this download
    /// and no other. Kept by the daemon, never given back.
    #[serde(default)]
    #[ts(optional)]
    pub cookie: Option<String>,
    /// The page it came from (`Referer`), the same way.
    #[serde(default)]
    #[ts(optional)]
    pub referrer: Option<String>,
    /// The browser's user agent, sent instead of Ottie's, the same way.
    #[serde(default)]
    #[ts(optional)]
    pub user_agent: Option<String>,
    /// Makes sending it again safe (docs/decisions/0060): a client that got
    /// no answer (a timeout, a lost connection) sends the same body with the
    /// same `requestId`, and gets the first one's answer instead of a second
    /// download, for 10 minutes (until Ottie restarts). Per token; 1 to 64
    /// letters, digits, `-` or `_`, random (a UUID). Ignored in
    /// `POST /jobs/batch`.
    #[serde(default)]
    #[ts(optional)]
    pub request_id: Option<String>,
}

/// What a download does when a file with its name is already in its folder
/// (docs/decisions/0049).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum Existing {
    /// Saved under the first free name: `name (1).ext`, `name (2).ext`…
    Rename,
    /// Downloaded beside it, then takes its place once complete.
    Replace,
    /// Stops before downloading anything: it fails with the problem
    /// `fileExists` until resumed with another choice (`PATCH /jobs/{id}`).
    Ask,
}

impl From<Existing> for ottie_core::Existing {
    fn from(existing: Existing) -> Self {
        match existing {
            Existing::Rename => ottie_core::Existing::Rename,
            Existing::Replace => ottie_core::Existing::Replace,
            Existing::Ask => ottie_core::Existing::Ask,
        }
    }
}

impl From<ottie_core::Existing> for Existing {
    fn from(existing: ottie_core::Existing) -> Self {
        match existing {
            ottie_core::Existing::Rename => Existing::Rename,
            ottie_core::Existing::Replace => Existing::Replace,
            ottie_core::Existing::Ask => Existing::Ask,
        }
    }
}

/// Answer to `POST /jobs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Added {
    pub id: String,
}

/// Body of `POST /jobs/batch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddJobs {
    /// Added in this order, each as `POST /jobs` would.
    pub jobs: Vec<AddJob>,
}

/// Answer to `POST /jobs/batch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddedJobs {
    /// One per job sent, in the same order: its new ID, or null if it was
    /// refused.
    pub ids: Vec<Option<String>>,
    /// Why each refused one was, by its place in `jobs`.
    pub refused: Vec<RefusedJob>,
}

/// A job of `POST /jobs/batch` that wasn't added, and why: the error
/// `POST /jobs` would have answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RefusedJob {
    /// Its place in `jobs`, from 0.
    pub index: u32,
    #[serde(flatten)]
    pub error: ApiError,
}

/// Body of `POST /jobs/preview`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PreviewLinks {
    /// Any text: one link per line, an email, a web page. A sequence in a
    /// link (`[001-120]`, `[a-z]`, `{small,large}`) gives each link it holds.
    pub text: String,
    /// The folder they would be saved in, if not their categories': then
    /// the folder decides their category.
    #[serde(default)]
    #[ts(optional)]
    pub dest_dir: Option<String>,
}

/// Answer to `POST /jobs/preview`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Preview {
    /// In the order found, each once.
    pub links: Vec<FoundLink>,
}

/// Something in the text that looks like a link (`scheme://…`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export)]
pub enum FoundLink {
    /// A link that can be added.
    Link {
        /// As it would be added (normalized: `HTTPS://Example.com` is
        /// `https://example.com/`).
        url: String,
        /// The file name the link suggests; the server may give another.
        name: String,
        /// The category it would get without one chosen.
        matched: Matched,
        /// A download of this same link is already on the list.
        known: bool,
        /// A file named `name` is already in the folder it would be saved in.
        exists: bool,
    },
    /// One that can't: `refused` says why (`invalidUrl` or
    /// `unsupportedScheme`).
    Invalid { text: String, refused: ApiError },
}

/// Answer to `POST /jobs/{id}/stream`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct StreamLink {
    /// A path on the daemon (`/api/v1/stream/<ticket>/<name>`) that serves
    /// the file to anyone who has it, without the token, with `Range`
    /// support. Valid for 12 hours after its last use.
    pub url: String,
}

/// A category: where its downloads go and which files it takes. As listed
/// by `GET /categories` and sent in `categoryChanged` events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Category {
    /// General is 1.
    pub id: u32,
    pub name: String,
    /// Where its downloads are saved; null: each download must name a folder.
    pub folder: Option<String>,
    /// Lowercase, without the dot. Each belongs to one category at most.
    pub extensions: Vec<String>,
    /// Bytes per second for all its downloads together; 0 = none.
    #[ts(type = "number")]
    pub speed_limit: u64,
    /// How many of its downloads may run at once; 0 = no cap of its own.
    pub max_concurrent: u32,
    /// Null for the user's own categories.
    pub built_in: Option<BuiltIn>,
}

/// The categories Ottie itself needs; they can't be removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum BuiltIn {
    /// Downloads that match no extension; its folder is the default one.
    General,
    /// Downloads saved in a folder of no category. It never has a folder or
    /// extensions (they are ignored when it's changed).
    Other,
}

/// Body of `POST /categories` and `PUT /categories/{id}`: every setting (a
/// PUT replaces them all).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CategoryBody {
    pub name: String,
    /// An absolute folder, or null.
    pub folder: Option<String>,
    /// With or without the dot, any case: `.MKV` and `mkv` are the same.
    pub extensions: Vec<String>,
    #[ts(type = "number")]
    pub speed_limit: u64,
    pub max_concurrent: u32,
}

/// Answer to `POST /categories`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddedCategory {
    pub id: u32,
}

/// Body of `POST /categories/match`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct MatchLink {
    pub url: String,
    /// The folder it would be saved in, if not its category's: then the
    /// folder decides the category.
    #[serde(default)]
    #[ts(optional)]
    pub dest_dir: Option<String>,
}

/// Answer to `POST /categories/match`: the category a download of that link
/// would get. The server's real file name may still move it, unless the user
/// picks the category or a folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Matched {
    pub category: u32,
    /// The extension the link suggests, if any.
    pub extension: Option<String>,
    /// The folder decided: it's this category's folder, or (Other) the
    /// folder of none.
    pub by_folder: bool,
}

/// A saved view: a name, which downloads it shows and how they're laid out.
/// Listed by `GET /views` and sent in `viewChanged` events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct View {
    pub id: u32,
    pub name: String,
    pub filters: ViewFilters,
    pub arrangement: Arrangement,
}

/// Which downloads a view shows: those that pass every filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ViewFilters {
    /// Null: any state. An empty list shows nothing. `connecting` counts as
    /// `downloading` (the daemon saves it so).
    pub states: Option<Vec<JobState>>,
    /// Category IDs; null: any category.
    pub categories: Option<Vec<u32>>,
    /// Part of the file name, in any case; empty: any name.
    pub name_contains: String,
    pub added: AddedWithin,
}

/// How recently a download was added; "today" starts at local midnight
/// where the list is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum AddedWithin {
    Any,
    Today,
    /// The past 7 days.
    Week,
}

/// How a list of downloads is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Arrangement {
    pub sort: SortKey,
    pub direction: SortDirection,
    /// Grouped by status, each group sorted.
    pub grouped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum SortKey {
    /// By ID, which is by creation time.
    Added,
    Name,
    Size,
    Progress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[ts(export)]
pub enum SortDirection {
    #[serde(rename = "asc")]
    Ascending,
    #[serde(rename = "desc")]
    Descending,
}

/// Body of `POST /views` and `PUT /views/{id}`: everything about the view
/// (a PUT replaces it all).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ViewBody {
    pub name: String,
    pub filters: ViewFilters,
    pub arrangement: Arrangement,
}

/// An FTPS server's certificate trusted by hand (docs/decisions/0054), as
/// `GET /certificates` lists them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TrustedCertificate {
    pub id: u32,
    /// The server's name as in links, lowercase.
    pub host: String,
    /// SHA-256 of the certificate, 64 lowercase hex digits.
    pub fingerprint: String,
}

/// Body of `POST /certificates`: what a `certificate` problem says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TrustCertificate {
    pub host: String,
    /// Hex, any case, colons allowed.
    pub fingerprint: String,
}

/// Answer to `POST /views`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddedView {
    pub id: u32,
}

/// A time slot (docs/decisions/0040), as `GET /slots` lists it and
/// `slotChanged` events send it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Slot {
    pub id: u32,
    /// Its place in the list: lower comes first, and wins where two apply
    /// to the same thing. Only the order means something.
    #[ts(type = "number")]
    pub place: u64,
    /// May be empty.
    pub name: String,
    /// Off: kept, but it never applies.
    pub enabled: bool,
    /// The category it applies to; null: the whole queue.
    pub category: Option<u32>,
    /// The days it happens on, every week (Monday first); empty when it
    /// happens once.
    pub days: Vec<Day>,
    /// `YYYY-MM-DD`: it happens once, on that day, and is removed once over.
    pub date: Option<String>,
    /// `HH:MM`, in the daemon's time zone.
    pub from: String,
    /// `HH:MM`; on the next day if it isn't after `from` (the same: a whole day).
    pub to: String,
    /// What it does to downloads; null: nothing.
    pub downloads: Option<SlotDownloads>,
    /// With `stop`: running downloads finish; only new ones wait.
    pub let_finish: bool,
    /// With `stop`: when it ends, the downloads it held are paused (to be
    /// resumed by hand) instead of carrying on.
    pub stay_paused: bool,
    /// Bytes per second for the queue (or the category) while it lasts;
    /// 0 = no limit; null: the usual limit.
    #[ts(type = "number | null")]
    pub speed_limit: Option<u64>,
    /// How many downloads may run at once while it lasts; null: the usual.
    pub max_concurrent: Option<u32>,
    /// It applies right now.
    pub active: bool,
}

/// Body of `POST /slots` and `PUT /slots/{id}`: a slot as set by the user.
/// Give `days` (every week) or a `date` (once). It must allow or stop
/// downloads, or set a limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SlotBody {
    #[serde(default)]
    #[ts(optional)]
    pub name: Option<String>,
    /// Default: on.
    #[serde(default)]
    #[ts(optional)]
    pub enabled: Option<bool>,
    /// Default: the whole queue.
    #[serde(default)]
    #[ts(optional)]
    pub category: Option<u32>,
    #[serde(default)]
    #[ts(optional)]
    pub days: Option<Vec<Day>>,
    /// `YYYY-MM-DD`; given, the slot happens once and `days` is ignored.
    #[serde(default)]
    #[ts(optional)]
    pub date: Option<String>,
    /// `HH:MM`.
    pub from: String,
    /// `HH:MM`.
    pub to: String,
    #[serde(default)]
    #[ts(optional)]
    pub downloads: Option<SlotDownloads>,
    #[serde(default)]
    #[ts(optional)]
    pub let_finish: Option<bool>,
    #[serde(default)]
    #[ts(optional)]
    pub stay_paused: Option<bool>,
    #[serde(default)]
    #[ts(optional, type = "number")]
    pub speed_limit: Option<u64>,
    #[serde(default)]
    #[ts(optional)]
    pub max_concurrent: Option<u32>,
}

/// Answer to `GET /slots`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SlotList {
    /// The time zone slots follow (the daemon's computer's), like
    /// `Europe/Paris`; null if the system doesn't name it.
    pub time_zone: Option<String>,
    /// In the order of the list: where two apply to the same thing, the
    /// higher one wins.
    pub slots: Vec<Slot>,
}

/// Answer to `POST /slots`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddedSlot {
    pub id: u32,
}

/// Body of `POST /slots/reorder`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ReorderSlots {
    /// Every slot, once, in the new order.
    pub ids: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum Day {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

/// What a slot does to downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum SlotDownloads {
    /// They may run (over a lower slot that stops them).
    Allow,
    /// None start; running ones stop (unless `letFinish`) and carry on
    /// when it ends (unless `stayPaused`).
    Stop,
}

impl From<Day> for ottie_core::Weekday {
    fn from(day: Day) -> Self {
        use ottie_core::Weekday as W;
        match day {
            Day::Monday => W::Monday,
            Day::Tuesday => W::Tuesday,
            Day::Wednesday => W::Wednesday,
            Day::Thursday => W::Thursday,
            Day::Friday => W::Friday,
            Day::Saturday => W::Saturday,
            Day::Sunday => W::Sunday,
        }
    }
}

impl From<ottie_core::Weekday> for Day {
    fn from(day: ottie_core::Weekday) -> Self {
        use ottie_core::Weekday as W;
        match day {
            W::Monday => Day::Monday,
            W::Tuesday => Day::Tuesday,
            W::Wednesday => Day::Wednesday,
            W::Thursday => Day::Thursday,
            W::Friday => Day::Friday,
            W::Saturday => Day::Saturday,
            W::Sunday => Day::Sunday,
        }
    }
}

impl From<ottie_core::Slot> for Slot {
    fn from(slot: ottie_core::Slot) -> Self {
        use ottie_core::{Downloads, Repeat};
        let settings = slot.settings;
        let (days, date) = match settings.repeat {
            Repeat::Weekly(days) => (days.into_iter().map(Day::from).collect(), None),
            Repeat::Once(date) => (Vec::new(), Some(date.to_string())),
        };
        let (downloads, let_finish, stay_paused) = match settings.downloads {
            None => (None, false, false),
            Some(Downloads::Allow) => (Some(SlotDownloads::Allow), false, false),
            Some(Downloads::Stop {
                let_finish,
                stay_paused,
            }) => (Some(SlotDownloads::Stop), let_finish, stay_paused),
        };
        let hhmm = |time: ottie_core::Time| format!("{:02}:{:02}", time.hour(), time.minute());
        Slot {
            id: slot.id.0,
            place: slot.place,
            name: settings.name,
            enabled: settings.enabled,
            category: settings.category.map(|c| c.0),
            days,
            date,
            from: hhmm(settings.from),
            to: hhmm(settings.to),
            downloads,
            let_finish,
            stay_paused,
            speed_limit: settings.speed_limit,
            max_concurrent: settings
                .max_concurrent
                .map(|max| u32::try_from(max).unwrap_or(u32::MAX)),
            active: slot.active,
        }
    }
}

impl SlotBody {
    /// The engine's settings, or which time or date isn't one.
    pub(crate) fn settings(self) -> Result<ottie_core::SlotSettings, String> {
        use ottie_core::{Date, Downloads, Repeat, Time};
        let time = |text: String| text.trim().parse::<Time>().map_err(|_| text);
        let repeat = match self.date {
            Some(date) => Repeat::Once(date.trim().parse::<Date>().map_err(|_| date)?),
            None => Repeat::Weekly(
                self.days
                    .unwrap_or_default()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            ),
        };
        let downloads = self.downloads.map(|downloads| match downloads {
            SlotDownloads::Allow => Downloads::Allow,
            SlotDownloads::Stop => Downloads::Stop {
                let_finish: self.let_finish.unwrap_or(false),
                stay_paused: self.stay_paused.unwrap_or(false),
            },
        });
        Ok(ottie_core::SlotSettings {
            name: self.name.unwrap_or_default(),
            enabled: self.enabled.unwrap_or(true),
            category: self.category.map(ottie_core::CategoryId),
            repeat,
            from: time(self.from)?,
            to: time(self.to)?,
            downloads,
            speed_limit: self.speed_limit,
            max_concurrent: self
                .max_concurrent
                .map(|max| usize::try_from(max).unwrap_or(usize::MAX)),
        })
    }
}

/// Body of `PATCH /jobs/{id}`: the job's own settings to change; a field
/// left out stays as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct JobChange {
    /// Bytes per second; 0 = no limit of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub speed_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub priority: Option<Priority>,
    /// Streaming mode. Once set, the file type no longer decides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub streaming: Option<bool>,
    /// Wait until then, in milliseconds since 1970 (UTC): a queued, paused,
    /// failed or retrying download becomes `scheduled`; a running one stops
    /// first. `null`: no start time (a scheduled download joins the queue
    /// now, as it does for a time already past).
    #[serde(default, with = "present", skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number | null")]
    #[schema(value_type = Option<u64>)]
    pub start_at: Option<Option<u64>>,
    /// Pause then, wherever it is; `null`: no stop time. After the start time.
    #[serde(default, with = "present", skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number | null")]
    #[schema(value_type = Option<u64>)]
    pub stop_at: Option<Option<u64>>,
    /// What to do if a file with its name is already in its folder: for one
    /// that stopped to ask (`fileExists`), choose, then resume it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub existing: Option<Existing>,
}

/// A field that's there, even as `null` (`Some(None)`), as opposed to one
/// left out (`None`, through `#[serde(default)]`).
mod present {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Some)
    }

    /// `Some(None)` as `null`; `None` is skipped (`skip_serializing_if`).
    pub(super) fn serialize<S, T>(
        value: &Option<Option<T>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        value.serialize(serializer)
    }
}

impl JobChange {
    /// The engine's change; `outOfRange` for a limit or a time past what
    /// Ottie takes.
    pub(crate) fn checked(self) -> Result<ottie_core::JobChange, Failure> {
        if let Some(limit) = self.speed_limit {
            in_range("speedLimit", limit, &SPEED_LIMIT)?;
        }
        let time = |field, at: Option<Option<u64>>| {
            at.map(|at| at.map(|at| from_millis(field, at)).transpose())
                .transpose()
        };
        Ok(ottie_core::JobChange {
            speed_limit: self.speed_limit,
            priority: self.priority.map(Into::into),
            streaming: self.streaming,
            start_at: time("startAt", self.start_at)?,
            stop_at: time("stopAt", self.stop_at)?,
            existing: self.existing.map(Into::into),
        })
    }
}

/// A time on the wire: milliseconds since 1970 (UTC).
pub(crate) fn millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

/// A time from the wire; `outOfRange` (naming its `field`) past the end of
/// year 9999. A larger one could make the system's clock panic.
pub(crate) fn from_millis(field: &str, millis: u64) -> Result<SystemTime, Failure> {
    in_range(field, millis, &TIME)?;
    // Every system holds the times within TIME; checked all the same.
    UNIX_EPOCH
        .checked_add(Duration::from_millis(millis))
        .ok_or_else(|| out_of_range(field, &TIME))
}

/// Body of `POST /jobs/reorder`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ReorderJobs {
    /// The downloads to move. Those that aren't queued, scheduled or paused stay as they are.
    pub ids: Vec<String>,
    pub to: Reorder,
}

/// Where to move downloads in the queue, among the waiting downloads of
/// their priority: `"up"`, `"down"`, `"top"`, `"bottom"`, or next to a
/// download, `{ "before": id }` or `{ "after": id }` (where it was dropped).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum Reorder {
    /// One place earlier.
    Up,
    /// One place later.
    Down,
    /// Before all the others.
    Top,
    /// After all the others.
    Bottom,
    /// Just before this download. Only those of its priority move.
    Before(String),
    /// Just after this download.
    After(String),
}

impl TryFrom<Reorder> for ottie_core::Reorder {
    type Error = ottie_core::Error;

    fn try_from(to: Reorder) -> Result<Self, Self::Error> {
        Ok(match to {
            Reorder::Up => ottie_core::Reorder::Up,
            Reorder::Down => ottie_core::Reorder::Down,
            Reorder::Top => ottie_core::Reorder::Top,
            Reorder::Bottom => ottie_core::Reorder::Bottom,
            Reorder::Before(id) => ottie_core::Reorder::Before(id.parse()?),
            Reorder::After(id) => ottie_core::Reorder::After(id.parse()?),
        })
    }
}

/// Body of `POST /jobs/{id}/category`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct MoveJob {
    pub category: u32,
}

/// What applies to the whole daemon, as `GET /settings` answers it and
/// `settingsChanged` events send it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Settings {
    /// The first-start setup (starter categories) was done or skipped;
    /// until then, front ends offer it.
    pub setup_done: bool,
    /// The last step of that setup someone went past (done or skipped), so
    /// it carries on after it, in whichever front end; null: from the start.
    pub setup_step: Option<SetupStep>,
    /// How many downloads run at once; the others wait. At least 1.
    pub max_concurrent: u32,
    /// Bytes per second for all downloads together; 0 = no limit.
    #[ts(type = "number")]
    pub global_limit: u64,
    /// A download that keeps failing with errors that may go away (a dropped
    /// connection, a busy server) fails after this many retries in a row.
    /// 0 = never retry.
    pub max_retries: u32,
    /// Retries a download makes before letting a waiting one go first.
    pub retries_before_yield: u32,
    /// Wait before the first retry, in milliseconds; each next one waits
    /// twice as long, up to a minute.
    #[ts(type = "number")]
    pub first_retry_wait_ms: u64,
    /// The most links added at once (`POST /jobs/batch`) or looked at
    /// (`POST /jobs/preview`).
    pub max_batch: u32,
    /// New video and music downloads start in streaming mode, unless the
    /// one adding them says otherwise.
    pub stream_media: bool,
    /// The look shared by the devices that choose to follow it; null until
    /// one shares its own.
    pub appearance: Option<Appearance>,
    /// The language those devices show, as a tag like `en` or `pt-BR`; null:
    /// each shows its own system's. A device without it keeps its own.
    pub language: Option<String>,
    /// What a download does when a file with its name is already in its
    /// folder, unless the one adding it said (`existing` in `POST /jobs`).
    pub existing: Existing,
    /// While Ottie can't save to its database (disk full, a drive gone), the
    /// last error, in English; null when saving works. Not a setting: no
    /// change sets it, and `settingsChanged` tells when it changes.
    pub storage_failing: Option<String>,
}

/// Body of `PATCH /settings`: only the fields given change, all at once.
/// A value out of its range (see `crates/api/src/settings.rs`) refuses the
/// whole change with `outOfRange`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SettingsChange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub setup_done: Option<bool>,
    /// `null`: the setup starts from its first step.
    #[serde(default, with = "present", skip_serializing_if = "Option::is_none")]
    #[ts(optional, as = "Option<Option<SetupStep>>")]
    #[schema(value_type = Option<SetupStep>)]
    pub setup_step: Option<Option<SetupStep>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max_concurrent: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub global_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub retries_before_yield: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub first_retry_wait_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max_batch: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stream_media: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub appearance: Option<Appearance>,
    /// `null`: each device shows its own system's language.
    #[serde(default, with = "present", skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "string | null")]
    #[schema(value_type = Option<String>)]
    pub language: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub existing: Option<Existing>,
}

/// A step of the first-start setup (docs/decisions/0050), in the order they
/// come. A front end shows those it has: the desktop app's own come last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum SetupStep {
    /// The language, light or dark, the colours.
    Look,
    /// The downloads folder, how many at once, a file already there.
    Downloads,
    /// The starter categories.
    Categories,
    /// The desktop app: what starts at sign-in, closing the window.
    Startup,
    /// The desktop app: watching the clipboard.
    Clipboard,
    /// The desktop app: notifications and their sounds.
    Notifications,
}

impl SetupStep {
    const ALL: [SetupStep; 6] = [
        SetupStep::Look,
        SetupStep::Downloads,
        SetupStep::Categories,
        SetupStep::Startup,
        SetupStep::Clipboard,
        SetupStep::Notifications,
    ];

    /// As the engine keeps it (the same as in JSON).
    fn as_str(self) -> &'static str {
        match self {
            SetupStep::Look => "look",
            SetupStep::Downloads => "downloads",
            SetupStep::Categories => "categories",
            SetupStep::Startup => "startup",
            SetupStep::Clipboard => "clipboard",
            SetupStep::Notifications => "notifications",
        }
    }

    /// A step the engine kept; one this daemon doesn't know (saved by a
    /// newer one) counts as none, so the setup starts over.
    fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|step| step.as_str() == text)
    }
}

/// How the front ends look: light, dark or as the system says, and which
/// pair of themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Appearance {
    pub mode: AppearanceMode,
    pub palette: Palette,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum AppearanceMode {
    System,
    Light,
    Dark,
}

/// A light theme and a dark one that go together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum Palette {
    /// Seafoam and Lagoon.
    Sea,
    /// Pearl and Slate.
    Neutral,
}

/// The total download speed over the last hour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SpeedHistory {
    /// All downloads' speed together in bytes per second, one sample per
    /// second, oldest first; the last is the latest. At most 3600.
    #[ts(type = "number[]")]
    pub samples: Vec<u64>,
}

/// Why a job failed or is retrying. Clients translate it by `code` and
/// fall back to the job's English `error` for codes they don't know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(
    tag = "code",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(export)]
pub enum Problem {
    /// Connecting or reading failed; `detail` is English.
    Network {
        detail: String,
    },
    /// Too many redirects, or one to something other than http(s).
    Redirect {
        detail: String,
    },
    /// The server answered with a status we can't use (404, 503…).
    HttpStatus {
        status: u16,
    },
    /// No data for this many seconds.
    Stalled {
        #[ts(type = "number")]
        seconds: u64,
    },
    /// The connection closed before the end of the file.
    Incomplete {
        #[ts(type = "number")]
        received: u64,
        #[ts(type = "number")]
        expected: u64,
    },
    /// The file kept changing on the server.
    RemoteChanging,
    /// Writing the file failed; `detail` is English.
    Disk {
        detail: String,
    },
    /// A file named `name` is already in the folder, and the download asks
    /// what to do: resume it after choosing `existing` (`PATCH /jobs/{id}`).
    FileExists {
        name: String,
    },
    /// The FTP server refused a command with reply `status` (530: the user
    /// name or password, 550: no such file or no access…); `detail` is what
    /// it said.
    FtpReply {
        status: u16,
        detail: String,
    },
    /// Neither the system nor the person trusts the FTPS server's
    /// certificate: posting `host` and `fingerprint` (SHA-256, lowercase
    /// hex) to `POST /certificates` trusts it; then resume the download.
    Certificate {
        host: String,
        fingerprint: String,
    },
    /// An `ftpes://` link to a server that doesn't offer TLS.
    NoTls,
    Other {
        detail: String,
    },
}

/// Who a token belongs to, and the folders devices may use: `GET /access`
/// (docs/decisions/0052).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AccessInfo {
    /// The device this token was given to; null for the computer running
    /// Ottie (its token file's token), which alone manages devices and folders.
    pub device: Option<Device>,
    /// It may look but not change anything.
    pub read_only: bool,
    pub folders: AllowedFolders,
}

/// Where devices may have downloads written, besides every category's
/// folder (and what's inside them). The computer running Ottie isn't limited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AllowedFolders {
    /// `folders` in config.toml: changed only there.
    pub config: Vec<String>,
    /// Chosen in Settings (`PUT /access/folders`).
    pub chosen: Vec<String>,
}

/// Body of `PUT /access/folders`: absolute folders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct FolderList {
    pub folders: Vec<String>,
}

/// A device with a token of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Device {
    pub id: u32,
    pub name: String,
    /// It may look but not change anything.
    pub read_only: bool,
    /// When it was added, in milliseconds since 1970.
    #[ts(type = "number")]
    pub created: u64,
    /// When its token was last used (to the minute), in milliseconds since 1970.
    #[ts(type = "number | null")]
    pub last_used: Option<u64>,
}

/// Body of `POST /identity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct IdentityChallenge {
    /// 32 to 128 hex digits, random, new for each check.
    pub nonce: String,
}

/// Answer of `POST /identity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct IdentityProof {
    /// HMAC-SHA256 of `ottie identity v1`, a line break, then the nonce,
    /// keyed with this computer's token, as hex.
    pub proof: String,
}

/// Body of `POST /access/devices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddDevice {
    /// 1 to 60 characters: "Phone", "Laptop"…
    pub name: String,
    /// Default: false.
    #[serde(default)]
    #[ts(optional)]
    pub read_only: Option<bool>,
}

/// Answer to `POST /access/devices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddedDevice {
    pub device: Device,
    /// Its token, shown this once: Ottie keeps only a hash of it.
    pub token: String,
}

/// Body of every error answer: English text, plus a code (and details) for
/// clients to translate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ApiError {
    /// English: for logs, and for codes a client doesn't know.
    pub error: String,
    #[serde(flatten)]
    pub reason: Reason,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(
    tag = "code",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(export)]
pub enum Reason {
    /// The token is missing or wrong (401).
    Unauthorized,
    /// This device's token is read-only (403).
    ReadOnly,
    /// Only the computer running Ottie, with its token file's token, may do
    /// this: manage devices and the folders they may use (403).
    OwnerOnly,
    /// This device may not have downloads written there: only in a
    /// category's folder or one listed in Settings or config.toml (403).
    FolderNotAllowed {
        folder: String,
    },
    DeviceNotFound {
        id: u32,
    },
    /// A device's name has 1 to 60 characters.
    InvalidDeviceName,
    NoSuchEndpoint,
    /// A part of the path isn't what it should be (a number ID that isn't
    /// a number); `error` says what's wrong.
    InvalidPath,
    /// The body isn't the JSON expected; `error` says what's wrong.
    InvalidBody,
    InvalidUrl,
    /// `destDir` must be an absolute path.
    RelativeFolder,
    UnsupportedScheme {
        scheme: String,
    },
    NonUtf8Path,
    /// No folder given, and the category has none.
    NoFolder {
        category: String,
    },
    InvalidJobId {
        id: String,
    },
    JobNotFound {
        id: String,
    },
    /// The job's state doesn't allow it (resuming a completed job…).
    InvalidTransition {
        from: JobState,
        to: JobState,
    },
    /// A download's stop time must come after its start time.
    StopBeforeStart,
    SlotNotFound {
        id: u32,
    },
    /// A slot without a date needs at least one day.
    SlotWithoutDays,
    /// A slot must allow or stop downloads, or set a limit.
    SlotWithoutEffect,
    /// Not `HH:MM` (a time) or `YYYY-MM-DD` (a date).
    InvalidTime {
        text: String,
    },
    /// The new order must list every slot once.
    InvalidSlotOrder,
    CategoryNotFound {
        id: u32,
    },
    InvalidCategoryName,
    InvalidExtension {
        extension: String,
    },
    CategoryNameTaken {
        name: String,
    },
    ExtensionTaken {
        extension: String,
        category: String,
    },
    /// General and Other can't be removed.
    CannotRemoveBuiltIn {
        category: String,
    },
    /// A value must be from `min` to `max`; `setting` is its field name
    /// (`maxConcurrent`, `globalLimit`, `speedLimit`, `startAt`…).
    OutOfRange {
        setting: String,
        #[ts(type = "number")]
        min: u64,
        #[ts(type = "number")]
        max: u64,
    },
    /// Its file is moving; try again once it's done.
    JobMoving {
        id: String,
    },
    /// The download can't resume, so moving it now would start it over.
    CannotMoveWhileRunning,
    /// The file couldn't move (disk full, no permission…); `detail` says why.
    MoveFailed {
        detail: String,
    },
    ViewNotFound {
        id: u32,
    },
    InvalidViewName,
    ViewNameTaken {
        name: String,
    },
    /// Not a server's name and a certificate's SHA-256 (64 hex digits).
    InvalidCertificate,
    CertificateNotFound {
        id: u32,
    },
    /// Not a language tag like `en` or `pt-BR`.
    InvalidLanguage {
        language: String,
    },
    /// A play link that doesn't exist or has expired.
    NoSuchLink,
    /// Played past what arrived, but the download failed or was cancelled.
    DownloadStopped {
        id: String,
    },
    /// The download started over while being played (its file changed).
    FileChanged {
        id: String,
    },
    /// A browser's `cookie`, `referrer` or `userAgent` (the field's name)
    /// that can't be sent as a header: a line break, or over 32 KiB.
    InvalidHeader {
        name: String,
    },
    /// A `requestId` that isn't 1 to 64 letters, digits, `-` or `_`.
    InvalidRequestId,
    /// More links at once than the setting `maxBatch` allows.
    TooManyLinks {
        #[ts(type = "number")]
        count: u64,
        #[ts(type = "number")]
        max: u64,
    },
    /// Saving the change to the database failed (a full disk, a drive gone):
    /// nothing changed. `error` has the database's text (docs/decisions/0059).
    StorageFailed,
    /// The daemon is shutting down.
    Stopping,
    /// A bug or an unexpected failure; `error` has the details.
    Internal,
}

/// A message on the `GET /events` WebSocket. Connect first, then fetch
/// `GET /jobs`, then apply events in order (insert-or-replace by `id`, or
/// drop on `removed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, ToSchema)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(export)]
pub enum Event {
    /// A job was added or changed state, name or settings.
    Changed { job: Job },
    /// Bytes arrived; sent a few times per second per running job.
    Progress {
        id: String,
        #[ts(type = "number")]
        downloaded: u64,
        #[ts(type = "number | null")]
        total: Option<u64>,
        /// Several connections: each one's range, with what has arrived so
        /// far. Empty for one connection.
        ranges: Vec<ByteRange>,
    },
    /// A job was removed (`DELETE /jobs/{id}`): drop it.
    Removed { id: String },
    /// A category was added or changed: insert-or-replace by `id`.
    CategoryChanged { category: Category },
    /// A category was removed; its jobs were moved to General first (and a
    /// `changed` event was sent for each).
    CategoryRemoved { id: u32 },
    /// The settings changed (from any client): all of them, as now.
    SettingsChanged { settings: Settings },
    /// A saved view was added or changed: insert-or-replace by `id`.
    ViewChanged { view: View },
    /// A saved view was removed.
    ViewRemoved { id: u32 },
    /// A time slot was added or changed, or started or stopped applying:
    /// insert-or-replace by `id`.
    SlotChanged { slot: Slot },
    /// A time slot was removed (by hand, with its category, or once over).
    SlotRemoved { id: u32 },
    /// Events were missed (the client was too slow): fetch `GET /jobs` again.
    Resync,
}

impl From<JobSnapshot> for Job {
    fn from(job: JobSnapshot) -> Self {
        Job {
            id: job.id.to_string(),
            url: without_password(job.url),
            dest_dir: job.dest_dir.to_string_lossy().into_owned(),
            file_name: job.file_name,
            state: job.state.into(),
            downloaded: job.downloaded,
            total: job.total,
            resumable: job.resumable,
            error: job.problem.as_ref().map(ToString::to_string),
            problem: job.problem.map(Problem::from),
            priority: job.priority.into(),
            place: job.place,
            speed_limit: job.speed_limit,
            streaming: job.streaming,
            category: job.category.0,
            retries: job.retries,
            retry_at: job.retry_at.map(millis),
            ranges: job.segments.into_iter().map(ByteRange::from).collect(),
            moving: job.moving,
            waiting_for_bandwidth: job.waiting_for_bandwidth,
            start_at: job.start_at.map(millis),
            stop_at: job.stop_at.map(millis),
            held_by: job.held_by.map(|slot| slot.0),
        }
    }
}

impl From<ottie_core::Matched> for Matched {
    fn from(matched: ottie_core::Matched) -> Self {
        Matched {
            category: matched.category.0,
            extension: matched.extension,
            by_folder: matched.by_folder,
        }
    }
}

impl From<ottie_core::LinkPreview> for FoundLink {
    fn from(preview: ottie_core::LinkPreview) -> Self {
        match preview {
            ottie_core::LinkPreview::Link {
                url,
                name,
                matched,
                known,
                exists,
                ..
            } => FoundLink::Link {
                url: url.to_string(),
                name,
                matched: matched.into(),
                known,
                exists,
            },
            ottie_core::LinkPreview::Invalid { text, error } => FoundLink::Invalid {
                text,
                refused: Failure::from(error).into(),
            },
        }
    }
}

impl From<ottie_core::Settings> for Settings {
    fn from(settings: ottie_core::Settings) -> Self {
        let retry = settings.retry;
        Settings {
            setup_done: settings.setup_done,
            setup_step: settings.setup_step.as_deref().and_then(SetupStep::parse),
            max_concurrent: u32::try_from(settings.max_concurrent).unwrap_or(u32::MAX),
            global_limit: settings.global_limit,
            max_retries: retry.max_retries,
            retries_before_yield: retry.retries_before_yield,
            first_retry_wait_ms: u64::try_from(retry.first_wait.as_millis()).unwrap_or(u64::MAX),
            max_batch: u32::try_from(settings.max_batch).unwrap_or(u32::MAX),
            stream_media: settings.stream_media,
            appearance: settings.appearance.map(Appearance::from),
            language: settings.language,
            existing: settings.existing.into(),
            storage_failing: settings.storage_failing,
        }
    }
}

impl From<SettingsChange> for ottie_core::SettingsChange {
    fn from(change: SettingsChange) -> Self {
        ottie_core::SettingsChange {
            global_limit: change.global_limit,
            max_concurrent: change
                .max_concurrent
                .map(|max| usize::try_from(max).unwrap_or(usize::MAX)),
            max_retries: change.max_retries,
            retries_before_yield: change.retries_before_yield,
            first_retry_wait: change.first_retry_wait_ms.map(Duration::from_millis),
            max_batch: change
                .max_batch
                .map(|max| usize::try_from(max).unwrap_or(usize::MAX)),
            stream_media: change.stream_media,
            setup_done: change.setup_done,
            setup_step: change
                .setup_step
                .map(|step| step.map(|step| step.as_str().to_owned())),
            appearance: change.appearance.map(ottie_core::Appearance::from),
            language: change.language,
            existing: change.existing.map(Into::into),
        }
    }
}

impl From<ottie_core::Appearance> for Appearance {
    fn from(appearance: ottie_core::Appearance) -> Self {
        use ottie_core::{AppearanceMode as M, Palette as P};
        Appearance {
            mode: match appearance.mode {
                M::System => AppearanceMode::System,
                M::Light => AppearanceMode::Light,
                M::Dark => AppearanceMode::Dark,
            },
            palette: match appearance.palette {
                P::Sea => Palette::Sea,
                P::Neutral => Palette::Neutral,
            },
        }
    }
}

impl From<Appearance> for ottie_core::Appearance {
    fn from(appearance: Appearance) -> Self {
        use ottie_core::{AppearanceMode as M, Palette as P};
        ottie_core::Appearance {
            mode: match appearance.mode {
                AppearanceMode::System => M::System,
                AppearanceMode::Light => M::Light,
                AppearanceMode::Dark => M::Dark,
            },
            palette: match appearance.palette {
                Palette::Sea => P::Sea,
                Palette::Neutral => P::Neutral,
            },
        }
    }
}

impl From<ottie_core::Category> for Category {
    fn from(category: ottie_core::Category) -> Self {
        let settings = category.settings;
        Category {
            id: category.id.0,
            name: settings.name,
            folder: settings.folder.as_deref().map(path_text),
            extensions: settings.extensions,
            speed_limit: settings.speed_limit,
            max_concurrent: u32::try_from(settings.max_concurrent).unwrap_or(u32::MAX),
            built_in: category.built_in.map(|built_in| match built_in {
                ottie_core::BuiltIn::General => BuiltIn::General,
                ottie_core::BuiltIn::Other => BuiltIn::Other,
            }),
        }
    }
}

/// Category folders are checked to be UTF-8 when set, so nothing is lost.
fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Spelled out, so engine changes can't silently change the API.
/// A link as shown: the password it holds stays with the daemon
/// (docs/decisions/0054).
fn without_password(mut url: ottie_core::Url) -> String {
    if url.password().is_some() {
        // Only fails for links that can't hold one.
        let _ = url.set_password(None);
    }
    url.into()
}

impl From<ottie_core::TrustedCertificate> for TrustedCertificate {
    fn from(certificate: ottie_core::TrustedCertificate) -> Self {
        TrustedCertificate {
            id: certificate.id.0,
            host: certificate.host,
            fingerprint: certificate.fingerprint,
        }
    }
}

impl From<ottie_core::Problem> for Problem {
    fn from(problem: ottie_core::Problem) -> Self {
        use ottie_core::Problem as P;
        match problem {
            P::Network { detail } => Problem::Network { detail },
            P::Redirect { detail } => Problem::Redirect { detail },
            P::HttpStatus { status } => Problem::HttpStatus { status },
            P::Stalled { seconds } => Problem::Stalled { seconds },
            P::Incomplete { received, expected } => Problem::Incomplete { received, expected },
            P::RemoteChanging => Problem::RemoteChanging,
            P::Disk { detail } => Problem::Disk { detail },
            P::FileExists { name } => Problem::FileExists { name },
            P::FtpReply { code, detail } => Problem::FtpReply {
                status: code,
                detail,
            },
            P::Certificate { host, fingerprint } => Problem::Certificate { host, fingerprint },
            P::NoTls => Problem::NoTls,
            P::Other { detail } => Problem::Other { detail },
        }
    }
}

impl From<ottie_core::View> for View {
    fn from(view: ottie_core::View) -> Self {
        use ottie_core::{AddedWithin as A, SortDirection as D, SortKey as K};
        let settings = view.settings;
        View {
            id: view.id.0,
            name: settings.name,
            filters: ViewFilters {
                states: settings
                    .states
                    .map(|states| states.into_iter().map(JobState::from).collect()),
                categories: settings
                    .categories
                    .map(|ids| ids.into_iter().map(|id| id.0).collect()),
                name_contains: settings.name_contains,
                added: match settings.added {
                    A::Any => AddedWithin::Any,
                    A::Today => AddedWithin::Today,
                    A::Week => AddedWithin::Week,
                },
            },
            arrangement: Arrangement {
                sort: match settings.sort {
                    K::Added => SortKey::Added,
                    K::Name => SortKey::Name,
                    K::Size => SortKey::Size,
                    K::Progress => SortKey::Progress,
                },
                direction: match settings.direction {
                    D::Ascending => SortDirection::Ascending,
                    D::Descending => SortDirection::Descending,
                },
                grouped: settings.grouped,
            },
        }
    }
}

impl From<ViewBody> for ottie_core::ViewSettings {
    fn from(body: ViewBody) -> Self {
        use ottie_core::{AddedWithin as A, CategoryId, SortDirection as D, SortKey as K};
        let ViewBody {
            name,
            filters,
            arrangement,
        } = body;
        ottie_core::ViewSettings {
            name,
            states: filters
                .states
                .map(|states| states.into_iter().map(Into::into).collect()),
            categories: filters
                .categories
                .map(|ids| ids.into_iter().map(CategoryId).collect()),
            name_contains: filters.name_contains,
            added: match filters.added {
                AddedWithin::Any => A::Any,
                AddedWithin::Today => A::Today,
                AddedWithin::Week => A::Week,
            },
            sort: match arrangement.sort {
                SortKey::Added => K::Added,
                SortKey::Name => K::Name,
                SortKey::Size => K::Size,
                SortKey::Progress => K::Progress,
            },
            direction: match arrangement.direction {
                SortDirection::Ascending => D::Ascending,
                SortDirection::Descending => D::Descending,
            },
            grouped: arrangement.grouped,
        }
    }
}

impl From<JobState> for ottie_core::JobState {
    fn from(state: JobState) -> Self {
        use ottie_core::JobState as S;
        match state {
            JobState::Queued => S::Queued,
            JobState::Connecting => S::Connecting,
            JobState::Downloading => S::Downloading,
            JobState::Paused => S::Paused,
            JobState::Completed => S::Completed,
            JobState::Failed => S::Failed,
            JobState::Cancelled => S::Cancelled,
            JobState::Retrying => S::Retrying,
            JobState::Scheduled => S::Scheduled,
        }
    }
}

/// Spelled out, so renaming an engine state can't silently change the API.
impl From<ottie_core::JobState> for JobState {
    fn from(state: ottie_core::JobState) -> Self {
        use ottie_core::JobState as S;
        match state {
            S::Queued => JobState::Queued,
            S::Connecting => JobState::Connecting,
            S::Downloading => JobState::Downloading,
            S::Paused => JobState::Paused,
            S::Completed => JobState::Completed,
            S::Failed => JobState::Failed,
            S::Cancelled => JobState::Cancelled,
            S::Retrying => JobState::Retrying,
            S::Scheduled => JobState::Scheduled,
        }
    }
}

impl From<Priority> for ottie_core::Priority {
    fn from(priority: Priority) -> Self {
        use ottie_core::Priority as P;
        match priority {
            Priority::Low => P::Low,
            Priority::Normal => P::Normal,
            Priority::High => P::High,
            Priority::Urgent => P::Urgent,
        }
    }
}

impl From<ottie_core::Priority> for Priority {
    fn from(priority: ottie_core::Priority) -> Self {
        use ottie_core::Priority as P;
        match priority {
            P::Low => Priority::Low,
            P::Normal => Priority::Normal,
            P::High => Priority::High,
            P::Urgent => Priority::Urgent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ByteRange, Event, JobChange, SetupStep};

    #[test]
    fn setup_steps_are_kept_by_their_json_names() {
        for step in SetupStep::ALL {
            let json = serde_json::to_value(step).unwrap();
            assert_eq!(json, serde_json::json!(step.as_str()));
            assert_eq!(SetupStep::parse(step.as_str()), Some(step));
        }
        assert_eq!(SetupStep::parse("later"), None);
    }

    #[test]
    fn a_time_left_out_is_not_a_time_taken_away() {
        let change = |json| serde_json::from_value::<JobChange>(json).unwrap();
        let left_out = change(serde_json::json!({ "priority": "high" }));
        assert_eq!((left_out.start_at, left_out.stop_at), (None, None));
        let taken_away = change(serde_json::json!({ "startAt": null, "stopAt": 5 }));
        assert_eq!(
            (taken_away.start_at, taken_away.stop_at),
            (Some(None), Some(Some(5)))
        );
        // And back: only what was given.
        let json = serde_json::to_value(&taken_away).unwrap();
        assert_eq!(json, serde_json::json!({ "startAt": null, "stopAt": 5 }));
    }

    #[test]
    fn progress_carries_each_connections_range() {
        let range = ByteRange::from(ottie_core::Segment {
            start: 100,
            end: 300,
            done: 50,
        });
        let event = Event::Progress {
            id: "2026.10.13-09.00.00-1".into(),
            downloaded: 50,
            total: Some(300),
            ranges: vec![range],
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "progress",
                "id": "2026.10.13-09.00.00-1",
                "downloaded": 50,
                "total": 300,
                "ranges": [{ "start": 100, "end": 300, "done": 50 }],
            })
        );
    }
}
