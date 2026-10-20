//! SQLite persistence. SQLite calls block, so the connection lives on its own
//! thread and the manager talks to it through a channel. See docs/decisions/0008.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jiff::civil::{Date, Time};
use reqwest::Url;
use rusqlite::{Connection, Row, params};
use tokio::sync::{oneshot, watch};

use crate::category::{BuiltIn, Category, CategoryId, CategorySettings};
use crate::certificate::{CertificateId, TrustedCertificate};
use crate::download::{Download, RequestHeaders, Segment, Validator};
use crate::error::Error;
use crate::filename::Existing;
use crate::id::JobId;
use crate::job::{JobState, Priority};
use crate::problem::Problem;
use crate::settings::{Appearance, AppearanceMode, Palette, RetryPolicy, Settings};
use crate::slot::{ALL_DAYS, Downloads, Repeat, SlotId, SlotSettings};
use crate::view::{AddedWithin, SortDirection, SortKey, View, ViewId, ViewSettings};

/// Each entry upgrades the schema by one version (`PRAGMA user_version`).
/// Never edit an entry once released; append a new one.
const MIGRATIONS: &[&str] = &[
    // 1: jobs
    "CREATE TABLE jobs (
        id_secs         INTEGER NOT NULL,
        id_seq          INTEGER NOT NULL,
        url             TEXT    NOT NULL,
        dest_dir        TEXT    NOT NULL,
        file_name       TEXT,
        state           TEXT    NOT NULL CHECK (state IN ('queued', 'connecting',
                            'downloading', 'paused', 'completed', 'failed', 'cancelled')),
        total           INTEGER,
        downloaded      INTEGER NOT NULL DEFAULT 0,
        validator_kind  TEXT    CHECK (validator_kind IN ('etag', 'last-modified')),
        validator_value TEXT,
        resumable       INTEGER NOT NULL DEFAULT 0 CHECK (resumable IN (0, 1)),
        error           TEXT,
        PRIMARY KEY (id_secs, id_seq)
    ) STRICT;",
    // 2: ranges of multi-connection downloads
    "CREATE TABLE segments (
        job_secs    INTEGER NOT NULL,
        job_seq     INTEGER NOT NULL,
        range_start INTEGER NOT NULL,
        range_end   INTEGER NOT NULL,
        done        INTEGER NOT NULL,
        PRIMARY KEY (job_secs, job_seq, range_start),
        FOREIGN KEY (job_secs, job_seq) REFERENCES jobs (id_secs, id_seq) ON DELETE CASCADE,
        CHECK (range_start < range_end AND done >= 0 AND done <= range_end - range_start)
    ) STRICT;",
    // 3: priorities, speed limits and settings
    "ALTER TABLE jobs ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal'
        CHECK (priority IN ('low', 'normal', 'high', 'urgent'));
     ALTER TABLE jobs ADD COLUMN speed_limit INTEGER NOT NULL DEFAULT 0
        CHECK (speed_limit >= 0);
     CREATE TABLE settings (
        id             INTEGER PRIMARY KEY CHECK (id = 1),
        global_limit   INTEGER NOT NULL DEFAULT 0 CHECK (global_limit >= 0),
        max_concurrent INTEGER NOT NULL DEFAULT 3 CHECK (max_concurrent >= 1)
     ) STRICT;
     INSERT INTO settings (id) VALUES (1);",
    // 4: categories. Runs with foreign keys off: SQLite only adds a
    // REFERENCES column with a non-NULL default while they are.
    "CREATE TABLE categories (
        id             INTEGER PRIMARY KEY CHECK (id >= 1),
        name           TEXT    NOT NULL COLLATE NOCASE UNIQUE CHECK (name <> ''),
        folder         TEXT,
        speed_limit    INTEGER NOT NULL DEFAULT 0 CHECK (speed_limit >= 0),
        max_concurrent INTEGER NOT NULL DEFAULT 0 CHECK (max_concurrent >= 0)
     ) STRICT;
     CREATE TABLE category_extensions (
        extension   TEXT    PRIMARY KEY CHECK (extension <> ''),
        category_id INTEGER NOT NULL REFERENCES categories (id) ON DELETE CASCADE
     ) STRICT;
     INSERT INTO categories (id, name) VALUES (1, 'General');
     ALTER TABLE jobs ADD COLUMN category_id INTEGER NOT NULL DEFAULT 1
        REFERENCES categories (id);
     ALTER TABLE jobs ADD COLUMN category_chosen INTEGER NOT NULL DEFAULT 0
        CHECK (category_chosen IN (0, 1));",
    // 5: the `retrying` state and the retry policy. SQLite can't change a
    // CHECK constraint, so `jobs` is rebuilt (with foreign keys off, see `prepare`).
    "CREATE TABLE jobs_new (
        id_secs         INTEGER NOT NULL,
        id_seq          INTEGER NOT NULL,
        url             TEXT    NOT NULL,
        dest_dir        TEXT    NOT NULL,
        file_name       TEXT,
        state           TEXT    NOT NULL CHECK (state IN ('queued', 'connecting',
                            'downloading', 'paused', 'completed', 'failed', 'cancelled',
                            'retrying')),
        total           INTEGER,
        downloaded      INTEGER NOT NULL DEFAULT 0,
        validator_kind  TEXT    CHECK (validator_kind IN ('etag', 'last-modified')),
        validator_value TEXT,
        resumable       INTEGER NOT NULL DEFAULT 0 CHECK (resumable IN (0, 1)),
        error           TEXT,
        priority        TEXT    NOT NULL DEFAULT 'normal'
                            CHECK (priority IN ('low', 'normal', 'high', 'urgent')),
        speed_limit     INTEGER NOT NULL DEFAULT 0 CHECK (speed_limit >= 0),
        category_id     INTEGER NOT NULL DEFAULT 1 REFERENCES categories (id),
        category_chosen INTEGER NOT NULL DEFAULT 0 CHECK (category_chosen IN (0, 1)),
        PRIMARY KEY (id_secs, id_seq)
     ) STRICT;
     INSERT INTO jobs_new (id_secs, id_seq, url, dest_dir, file_name, state, total,
                           downloaded, validator_kind, validator_value, resumable, error,
                           priority, speed_limit, category_id, category_chosen)
        SELECT id_secs, id_seq, url, dest_dir, file_name, state, total,
               downloaded, validator_kind, validator_value, resumable, error,
               priority, speed_limit, category_id, category_chosen
        FROM jobs;
     DROP TABLE jobs;
     ALTER TABLE jobs_new RENAME TO jobs;
     ALTER TABLE settings ADD COLUMN max_retries INTEGER NOT NULL DEFAULT 10
        CHECK (max_retries >= 0);
     ALTER TABLE settings ADD COLUMN retries_before_yield INTEGER NOT NULL DEFAULT 3
        CHECK (retries_before_yield >= 0);
     ALTER TABLE settings ADD COLUMN retry_wait_ms INTEGER NOT NULL DEFAULT 2000
        CHECK (retry_wait_ms >= 0);",
    // 6: why a job failed, as a kind plus details, so clients can translate
    // it. `error` keeps the English text; older failures become 'other'.
    "ALTER TABLE jobs ADD COLUMN problem_kind TEXT CHECK (problem_kind IN ('network',
        'redirect', 'http-status', 'stalled', 'incomplete', 'remote-changing', 'disk', 'other'));
     ALTER TABLE jobs ADD COLUMN problem_number INTEGER;
     ALTER TABLE jobs ADD COLUMN problem_total INTEGER;
     ALTER TABLE jobs ADD COLUMN problem_detail TEXT;
     UPDATE jobs SET problem_kind = 'other', problem_detail = error WHERE error IS NOT NULL;",
    // 7: the highest job ID handed out, so removing the newest job can't
    // make its ID come back. 0 / 0 = none recorded (the jobs table tells).
    "ALTER TABLE settings ADD COLUMN last_id_secs INTEGER NOT NULL DEFAULT 0
        CHECK (last_id_secs >= 0);
     ALTER TABLE settings ADD COLUMN last_id_seq INTEGER NOT NULL DEFAULT 0
        CHECK (last_id_seq >= 0);",
    // 8: whether the first-start setup (starter categories) was done or
    // skipped; a database that already has categories of its own counts as
    // set up. And the built-in Other category, for downloads saved in a folder
    // of no category: ID 2 on a new database, the next free one otherwise
    // (renamed if the user already has an "Other").
    "ALTER TABLE settings ADD COLUMN setup_done INTEGER NOT NULL DEFAULT 0
        CHECK (setup_done IN (0, 1));
     UPDATE settings SET setup_done = 1 WHERE (SELECT COUNT(*) FROM categories) > 1;
     ALTER TABLE categories ADD COLUMN built_in TEXT CHECK (built_in IN ('general', 'other'));
     CREATE UNIQUE INDEX categories_built_in ON categories (built_in);
     UPDATE categories SET built_in = 'general' WHERE id = 1;
     INSERT INTO categories (id, name, built_in)
        SELECT MAX(id) + 1,
               CASE WHEN EXISTS (SELECT 1 FROM categories WHERE name = 'Other')
                    THEN 'Other folders' ELSE 'Other' END,
               'other'
        FROM categories;",
    // 9: the appearance and language shared by the devices that follow them;
    // NULL until one shares its own. Languages are tags like `en` or `pt-BR`.
    "ALTER TABLE settings ADD COLUMN appearance_mode TEXT
        CHECK (appearance_mode IN ('system', 'light', 'dark'));
     ALTER TABLE settings ADD COLUMN appearance_palette TEXT
        CHECK (appearance_palette IN ('sea', 'neutral'));
     ALTER TABLE settings ADD COLUMN language TEXT
        CHECK (length(language) BETWEEN 2 AND 35 AND language NOT GLOB '*[^A-Za-z0-9-]*');",
    // 10: saved views. A list that is "any" (`any_state`, `any_category`)
    // has no rows; an empty one shows nothing.
    "CREATE TABLE views (
        id            INTEGER PRIMARY KEY CHECK (id >= 1),
        name          TEXT    NOT NULL COLLATE NOCASE UNIQUE CHECK (name <> ''),
        any_state     INTEGER NOT NULL CHECK (any_state IN (0, 1)),
        any_category  INTEGER NOT NULL CHECK (any_category IN (0, 1)),
        name_contains TEXT    NOT NULL DEFAULT '',
        added         TEXT    NOT NULL CHECK (added IN ('any', 'today', 'week')),
        sort          TEXT    NOT NULL CHECK (sort IN ('added', 'name', 'size', 'progress')),
        direction     TEXT    NOT NULL CHECK (direction IN ('asc', 'desc')),
        grouped       INTEGER NOT NULL CHECK (grouped IN (0, 1))
     ) STRICT;
     CREATE TABLE view_states (
        view_id INTEGER NOT NULL REFERENCES views (id) ON DELETE CASCADE,
        state   TEXT    NOT NULL CHECK (state IN ('queued', 'downloading', 'paused',
                                                  'completed', 'failed', 'cancelled', 'retrying')),
        PRIMARY KEY (view_id, state)
     ) STRICT;
     CREATE TABLE view_categories (
        view_id     INTEGER NOT NULL REFERENCES views (id) ON DELETE CASCADE,
        category_id INTEGER NOT NULL REFERENCES categories (id) ON DELETE CASCADE,
        PRIMARY KEY (view_id, category_id)
     ) STRICT;",
    // 11: streaming mode per download, and whether the user chose it (else
    // the file type decides); the setting that turns it on for videos and music.
    "ALTER TABLE jobs ADD COLUMN streaming INTEGER NOT NULL DEFAULT 0
        CHECK (streaming IN (0, 1));
     ALTER TABLE jobs ADD COLUMN streaming_chosen INTEGER NOT NULL DEFAULT 0
        CHECK (streaming_chosen IN (0, 1));
     ALTER TABLE settings ADD COLUMN stream_media INTEGER NOT NULL DEFAULT 1
        CHECK (stream_media IN (0, 1));",
    // 12: each job's place in the queue, at first in the order they were added.
    "ALTER TABLE jobs ADD COLUMN queue_place INTEGER NOT NULL DEFAULT 0
        CHECK (queue_place >= 0);
     UPDATE jobs SET queue_place = (SELECT COUNT(*) FROM jobs AS older
        WHERE older.id_secs < jobs.id_secs
           OR (older.id_secs = jobs.id_secs AND older.id_seq <= jobs.id_seq));",
    // 13: the most links added at once.
    "ALTER TABLE settings ADD COLUMN max_batch INTEGER NOT NULL DEFAULT 10000
        CHECK (max_batch >= 1);",
    // 14: the `scheduled` state, and each job's own start and stop time, in
    // milliseconds since 1970 (UTC). `jobs` and `view_states` are rebuilt for
    // their CHECKs, as in 5.
    "CREATE TABLE jobs_new (
        id_secs          INTEGER NOT NULL,
        id_seq           INTEGER NOT NULL,
        url              TEXT    NOT NULL,
        dest_dir         TEXT    NOT NULL,
        file_name        TEXT,
        state            TEXT    NOT NULL CHECK (state IN ('queued', 'connecting',
                             'downloading', 'paused', 'completed', 'failed', 'cancelled',
                             'retrying', 'scheduled')),
        total            INTEGER,
        downloaded       INTEGER NOT NULL DEFAULT 0,
        validator_kind   TEXT    CHECK (validator_kind IN ('etag', 'last-modified')),
        validator_value  TEXT,
        resumable        INTEGER NOT NULL DEFAULT 0 CHECK (resumable IN (0, 1)),
        error            TEXT,
        priority         TEXT    NOT NULL DEFAULT 'normal'
                             CHECK (priority IN ('low', 'normal', 'high', 'urgent')),
        speed_limit      INTEGER NOT NULL DEFAULT 0 CHECK (speed_limit >= 0),
        category_id      INTEGER NOT NULL DEFAULT 1 REFERENCES categories (id),
        category_chosen  INTEGER NOT NULL DEFAULT 0 CHECK (category_chosen IN (0, 1)),
        problem_kind     TEXT    CHECK (problem_kind IN ('network', 'redirect', 'http-status',
                             'stalled', 'incomplete', 'remote-changing', 'disk', 'other')),
        problem_number   INTEGER,
        problem_total    INTEGER,
        problem_detail   TEXT,
        streaming        INTEGER NOT NULL DEFAULT 0 CHECK (streaming IN (0, 1)),
        streaming_chosen INTEGER NOT NULL DEFAULT 0 CHECK (streaming_chosen IN (0, 1)),
        queue_place      INTEGER NOT NULL DEFAULT 0 CHECK (queue_place >= 0),
        start_at         INTEGER CHECK (start_at >= 0),
        stop_at          INTEGER CHECK (stop_at >= 0),
        PRIMARY KEY (id_secs, id_seq)
     ) STRICT;
     INSERT INTO jobs_new (id_secs, id_seq, url, dest_dir, file_name, state, total,
                           downloaded, validator_kind, validator_value, resumable, error,
                           priority, speed_limit, category_id, category_chosen,
                           problem_kind, problem_number, problem_total, problem_detail,
                           streaming, streaming_chosen, queue_place)
        SELECT id_secs, id_seq, url, dest_dir, file_name, state, total,
               downloaded, validator_kind, validator_value, resumable, error,
               priority, speed_limit, category_id, category_chosen,
               problem_kind, problem_number, problem_total, problem_detail,
               streaming, streaming_chosen, queue_place
        FROM jobs;
     DROP TABLE jobs;
     ALTER TABLE jobs_new RENAME TO jobs;
     CREATE TABLE view_states_new (
        view_id INTEGER NOT NULL REFERENCES views (id) ON DELETE CASCADE,
        state   TEXT    NOT NULL CHECK (state IN ('queued', 'downloading', 'paused',
                                                  'completed', 'failed', 'cancelled', 'retrying',
                                                  'scheduled')),
        PRIMARY KEY (view_id, state)
     ) STRICT;
     INSERT INTO view_states_new SELECT view_id, state FROM view_states;
     DROP TABLE view_states;
     ALTER TABLE view_states_new RENAME TO view_states;",
    // 15: time slots, in the order of the list (`place`). `days` holds a bit
    // per weekday, Monday first; a slot that happens once has a `date`
    // (YYYY-MM-DD) instead. Times are minutes from midnight. A slot of a
    // category goes with it. `stay_paused`: with a stop, those it held are
    // paused when it ends. And on jobs: started by hand while a slot held
    // them; the slot holding a scheduled one (no foreign key: a job may
    // still name a slot just removed, until it's settled).
    "CREATE TABLE slots (
        id             INTEGER PRIMARY KEY CHECK (id >= 1),
        place          INTEGER NOT NULL CHECK (place >= 0),
        name           TEXT    NOT NULL DEFAULT '',
        enabled        INTEGER NOT NULL CHECK (enabled IN (0, 1)),
        category_id    INTEGER REFERENCES categories (id) ON DELETE CASCADE,
        days           INTEGER CHECK (days BETWEEN 1 AND 127),
        date           TEXT    CHECK (date GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
        start_minute   INTEGER NOT NULL CHECK (start_minute BETWEEN 0 AND 1439),
        end_minute     INTEGER NOT NULL CHECK (end_minute BETWEEN 0 AND 1439),
        downloads      TEXT    CHECK (downloads IN ('allow', 'stop', 'stop-let-finish')),
        speed_limit    INTEGER CHECK (speed_limit >= 0),
        max_concurrent INTEGER CHECK (max_concurrent >= 1),
        stay_paused    INTEGER NOT NULL DEFAULT 0 CHECK (stay_paused IN (0, 1)),
        CHECK ((days IS NULL) <> (date IS NULL))
     ) STRICT;
     ALTER TABLE jobs ADD COLUMN ignores_slots INTEGER NOT NULL DEFAULT 0
        CHECK (ignores_slots IN (0, 1));
     ALTER TABLE jobs ADD COLUMN held_by INTEGER CHECK (held_by >= 1);",
    // 16: the legacy palettes (docs/decisions/0044). `settings` is rebuilt
    // for the CHECK, as `jobs` was in 5.
    "CREATE TABLE settings_new (
        id                   INTEGER PRIMARY KEY CHECK (id = 1),
        global_limit         INTEGER NOT NULL DEFAULT 0 CHECK (global_limit >= 0),
        max_concurrent       INTEGER NOT NULL DEFAULT 3 CHECK (max_concurrent >= 1),
        max_retries          INTEGER NOT NULL DEFAULT 10 CHECK (max_retries >= 0),
        retries_before_yield INTEGER NOT NULL DEFAULT 3 CHECK (retries_before_yield >= 0),
        retry_wait_ms        INTEGER NOT NULL DEFAULT 2000 CHECK (retry_wait_ms >= 0),
        last_id_secs         INTEGER NOT NULL DEFAULT 0 CHECK (last_id_secs >= 0),
        last_id_seq          INTEGER NOT NULL DEFAULT 0 CHECK (last_id_seq >= 0),
        setup_done           INTEGER NOT NULL DEFAULT 0 CHECK (setup_done IN (0, 1)),
        appearance_mode      TEXT CHECK (appearance_mode IN ('system', 'light', 'dark')),
        appearance_palette   TEXT CHECK (appearance_palette IN ('sea', 'neutral',
                                 'sea-legacy', 'neutral-legacy')),
        language             TEXT CHECK (length(language) BETWEEN 2 AND 35
                                 AND language NOT GLOB '*[^A-Za-z0-9-]*'),
        stream_media         INTEGER NOT NULL DEFAULT 1 CHECK (stream_media IN (0, 1)),
        max_batch            INTEGER NOT NULL DEFAULT 10000 CHECK (max_batch >= 1)
     ) STRICT;
     INSERT INTO settings_new (id, global_limit, max_concurrent, max_retries,
            retries_before_yield, retry_wait_ms, last_id_secs, last_id_seq, setup_done,
            appearance_mode, appearance_palette, language, stream_media, max_batch)
        SELECT id, global_limit, max_concurrent, max_retries,
            retries_before_yield, retry_wait_ms, last_id_secs, last_id_seq, setup_done,
            appearance_mode, appearance_palette, language, stream_media, max_batch
        FROM settings;
     DROP TABLE settings;
     ALTER TABLE settings_new RENAME TO settings;",
    // 17: no legacy palettes anymore (docs/decisions/0047): each saved one
    // becomes its rounded pair, and `settings` is rebuilt for the CHECK.
    "UPDATE settings SET appearance_palette = CASE appearance_palette
        WHEN 'sea-legacy' THEN 'sea'
        WHEN 'neutral-legacy' THEN 'neutral'
        ELSE appearance_palette END;
     CREATE TABLE settings_new (
        id                   INTEGER PRIMARY KEY CHECK (id = 1),
        global_limit         INTEGER NOT NULL DEFAULT 0 CHECK (global_limit >= 0),
        max_concurrent       INTEGER NOT NULL DEFAULT 3 CHECK (max_concurrent >= 1),
        max_retries          INTEGER NOT NULL DEFAULT 10 CHECK (max_retries >= 0),
        retries_before_yield INTEGER NOT NULL DEFAULT 3 CHECK (retries_before_yield >= 0),
        retry_wait_ms        INTEGER NOT NULL DEFAULT 2000 CHECK (retry_wait_ms >= 0),
        last_id_secs         INTEGER NOT NULL DEFAULT 0 CHECK (last_id_secs >= 0),
        last_id_seq          INTEGER NOT NULL DEFAULT 0 CHECK (last_id_seq >= 0),
        setup_done           INTEGER NOT NULL DEFAULT 0 CHECK (setup_done IN (0, 1)),
        appearance_mode      TEXT CHECK (appearance_mode IN ('system', 'light', 'dark')),
        appearance_palette   TEXT CHECK (appearance_palette IN ('sea', 'neutral')),
        language             TEXT CHECK (length(language) BETWEEN 2 AND 35
                                 AND language NOT GLOB '*[^A-Za-z0-9-]*'),
        stream_media         INTEGER NOT NULL DEFAULT 1 CHECK (stream_media IN (0, 1)),
        max_batch            INTEGER NOT NULL DEFAULT 10000 CHECK (max_batch >= 1)
     ) STRICT;
     INSERT INTO settings_new (id, global_limit, max_concurrent, max_retries,
            retries_before_yield, retry_wait_ms, last_id_secs, last_id_seq, setup_done,
            appearance_mode, appearance_palette, language, stream_media, max_batch)
        SELECT id, global_limit, max_concurrent, max_retries,
            retries_before_yield, retry_wait_ms, last_id_secs, last_id_seq, setup_done,
            appearance_mode, appearance_palette, language, stream_media, max_batch
        FROM settings;
     DROP TABLE settings;
     ALTER TABLE settings_new RENAME TO settings;",
    // 18: what to do when a file with a download's name is already in its
    // folder (docs/decisions/0049): the setting, each job's own choice
    // (NULL: the setting's), and the problem of one that stopped to ask.
    // `jobs` is rebuilt for the CHECK, as in 14.
    "CREATE TABLE jobs_new (
        id_secs          INTEGER NOT NULL,
        id_seq           INTEGER NOT NULL,
        url              TEXT    NOT NULL,
        dest_dir         TEXT    NOT NULL,
        file_name        TEXT,
        state            TEXT    NOT NULL CHECK (state IN ('queued', 'connecting',
                             'downloading', 'paused', 'completed', 'failed', 'cancelled',
                             'retrying', 'scheduled')),
        total            INTEGER,
        downloaded       INTEGER NOT NULL DEFAULT 0,
        validator_kind   TEXT    CHECK (validator_kind IN ('etag', 'last-modified')),
        validator_value  TEXT,
        resumable        INTEGER NOT NULL DEFAULT 0 CHECK (resumable IN (0, 1)),
        error            TEXT,
        priority         TEXT    NOT NULL DEFAULT 'normal'
                             CHECK (priority IN ('low', 'normal', 'high', 'urgent')),
        speed_limit      INTEGER NOT NULL DEFAULT 0 CHECK (speed_limit >= 0),
        category_id      INTEGER NOT NULL DEFAULT 1 REFERENCES categories (id),
        category_chosen  INTEGER NOT NULL DEFAULT 0 CHECK (category_chosen IN (0, 1)),
        problem_kind     TEXT    CHECK (problem_kind IN ('network', 'redirect', 'http-status',
                             'stalled', 'incomplete', 'remote-changing', 'disk', 'file-exists',
                             'other')),
        problem_number   INTEGER,
        problem_total    INTEGER,
        problem_detail   TEXT,
        streaming        INTEGER NOT NULL DEFAULT 0 CHECK (streaming IN (0, 1)),
        streaming_chosen INTEGER NOT NULL DEFAULT 0 CHECK (streaming_chosen IN (0, 1)),
        queue_place      INTEGER NOT NULL DEFAULT 0 CHECK (queue_place >= 0),
        start_at         INTEGER CHECK (start_at >= 0),
        stop_at          INTEGER CHECK (stop_at >= 0),
        ignores_slots    INTEGER NOT NULL DEFAULT 0 CHECK (ignores_slots IN (0, 1)),
        held_by          INTEGER CHECK (held_by >= 1),
        existing         TEXT    CHECK (existing IN ('rename', 'replace', 'ask')),
        PRIMARY KEY (id_secs, id_seq)
     ) STRICT;
     INSERT INTO jobs_new (id_secs, id_seq, url, dest_dir, file_name, state, total,
                           downloaded, validator_kind, validator_value, resumable, error,
                           priority, speed_limit, category_id, category_chosen,
                           problem_kind, problem_number, problem_total, problem_detail,
                           streaming, streaming_chosen, queue_place, start_at, stop_at,
                           ignores_slots, held_by)
        SELECT id_secs, id_seq, url, dest_dir, file_name, state, total,
               downloaded, validator_kind, validator_value, resumable, error,
               priority, speed_limit, category_id, category_chosen,
               problem_kind, problem_number, problem_total, problem_detail,
               streaming, streaming_chosen, queue_place, start_at, stop_at,
               ignores_slots, held_by
        FROM jobs;
     DROP TABLE jobs;
     ALTER TABLE jobs_new RENAME TO jobs;
     ALTER TABLE settings ADD COLUMN existing TEXT NOT NULL DEFAULT 'rename'
        CHECK (existing IN ('rename', 'replace', 'ask'));",
    // 19: the last step of the first-start setup someone went past, so it
    // carries on after it (docs/decisions/0050). The steps are the front
    // ends' names.
    "ALTER TABLE settings ADD COLUMN setup_step TEXT
        CHECK (length(setup_step) BETWEEN 1 AND 40);",
    // 20: FTP (docs/decisions/0054): its validator (size and modification
    // time) and problems (a reply refusing, an untrusted certificate, no
    // TLS for an ftpes:// link), so `jobs` is rebuilt for the CHECKs, as in
    // 18. And the certificates trusted by hand: a server's name and the
    // SHA-256 of its certificate, lowercase hex.
    "CREATE TABLE jobs_new (
        id_secs          INTEGER NOT NULL,
        id_seq           INTEGER NOT NULL,
        url              TEXT    NOT NULL,
        dest_dir         TEXT    NOT NULL,
        file_name        TEXT,
        state            TEXT    NOT NULL CHECK (state IN ('queued', 'connecting',
                             'downloading', 'paused', 'completed', 'failed', 'cancelled',
                             'retrying', 'scheduled')),
        total            INTEGER,
        downloaded       INTEGER NOT NULL DEFAULT 0,
        validator_kind   TEXT    CHECK (validator_kind IN ('etag', 'last-modified', 'ftp')),
        validator_value  TEXT,
        resumable        INTEGER NOT NULL DEFAULT 0 CHECK (resumable IN (0, 1)),
        error            TEXT,
        priority         TEXT    NOT NULL DEFAULT 'normal'
                             CHECK (priority IN ('low', 'normal', 'high', 'urgent')),
        speed_limit      INTEGER NOT NULL DEFAULT 0 CHECK (speed_limit >= 0),
        category_id      INTEGER NOT NULL DEFAULT 1 REFERENCES categories (id),
        category_chosen  INTEGER NOT NULL DEFAULT 0 CHECK (category_chosen IN (0, 1)),
        problem_kind     TEXT    CHECK (problem_kind IN ('network', 'redirect', 'http-status',
                             'stalled', 'incomplete', 'remote-changing', 'disk', 'file-exists',
                             'ftp-reply', 'certificate', 'no-tls', 'other')),
        problem_number   INTEGER,
        problem_total    INTEGER,
        problem_detail   TEXT,
        streaming        INTEGER NOT NULL DEFAULT 0 CHECK (streaming IN (0, 1)),
        streaming_chosen INTEGER NOT NULL DEFAULT 0 CHECK (streaming_chosen IN (0, 1)),
        queue_place      INTEGER NOT NULL DEFAULT 0 CHECK (queue_place >= 0),
        start_at         INTEGER CHECK (start_at >= 0),
        stop_at          INTEGER CHECK (stop_at >= 0),
        ignores_slots    INTEGER NOT NULL DEFAULT 0 CHECK (ignores_slots IN (0, 1)),
        held_by          INTEGER CHECK (held_by >= 1),
        existing         TEXT    CHECK (existing IN ('rename', 'replace', 'ask')),
        PRIMARY KEY (id_secs, id_seq)
     ) STRICT;
     INSERT INTO jobs_new SELECT * FROM jobs;
     DROP TABLE jobs;
     ALTER TABLE jobs_new RENAME TO jobs;
     CREATE TABLE certificates (
        id          INTEGER PRIMARY KEY CHECK (id >= 1),
        host        TEXT    NOT NULL CHECK (host <> ''),
        fingerprint TEXT    NOT NULL CHECK (length(fingerprint) = 64
                                AND fingerprint NOT GLOB '*[^0-9a-f]*'),
        UNIQUE (host, fingerprint)
     ) STRICT;",
    // 21: what a browser sent with a download it handed over (0055).
    "ALTER TABLE jobs ADD COLUMN cookie TEXT;
     ALTER TABLE jobs ADD COLUMN referrer TEXT;
     ALTER TABLE jobs ADD COLUMN user_agent TEXT;",
];

/// Everything saved about one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobRecord {
    pub id: JobId,
    pub state: JobState,
    pub download: Download,
    pub problem: Option<Problem>,
    pub priority: Priority,
    /// Its place in the queue, among the jobs of its priority.
    pub place: u64,
    pub speed_limit: u64,
    pub category: CategoryId,
    /// The user picked the category (or a folder, which decided it), rather
    /// than it being matched by extension; it is never re-matched.
    pub category_chosen: bool,
    pub streaming: bool,
    /// The user turned streaming mode on or off, rather than the file type
    /// deciding; it is never decided again.
    pub streaming_chosen: bool,
    pub start_at: Option<SystemTime>,
    pub stop_at: Option<SystemTime>,
    /// Started by hand while a time slot held it: slots leave it alone.
    pub ignores_slots: bool,
    /// While `Scheduled`: the slot holding it, so the one that ends can keep
    /// it paused, even across a restart.
    pub held_by: Option<SlotId>,
    /// What to do when its file's name is taken; `None`: the setting's.
    pub existing: Option<Existing>,
}

/// A time slot with its place in the list (lower comes first, and wins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotRecord {
    pub id: SlotId,
    pub place: u64,
    pub settings: SlotSettings,
}

/// Everything the database holds, as read at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Loaded {
    /// Oldest first.
    pub jobs: Vec<JobRecord>,
    pub settings: Settings,
    /// By ID; General is always there.
    pub categories: Vec<Category>,
    /// The highest job ID handed out, whether or not that job still exists.
    pub last_id: Option<JobId>,
    /// By ID.
    pub views: Vec<View>,
    /// By place.
    pub slots: Vec<SlotRecord>,
    /// By ID.
    pub certificates: Vec<TrustedCertificate>,
}

/// Handle to the database thread. The thread stops when this is dropped.
pub struct Store {
    /// The last write's error while writes fail: shown to clients.
    failing: watch::Receiver<Option<String>>,
    ops: std_mpsc::Sender<Op>,
}

enum Op {
    Load(oneshot::Sender<Result<Loaded, Error>>),
    Save(Box<JobRecord>),
    SaveJobs(Vec<JobRecord>),
    SaveSettings(Settings),
    SaveCategory(Category),
    RemoveCategory(CategoryId),
    SaveView(View),
    RemoveView(ViewId),
    RemoveJob { id: JobId, last_id: JobId },
    SaveSlot(SlotRecord),
    RemoveSlot(SlotId),
    SaveCertificate(TrustedCertificate),
    RemoveCertificate(CertificateId),
    Flush(oneshot::Sender<Result<(), Error>>),
}

impl Store {
    /// Opens (or creates) the database file and brings its schema up to date.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Store, Error> {
        let path = path.into();
        Self::spawn(move || {
            keep_private(&path)?;
            Ok(Connection::open(path)?)
        })
        .await
    }

    /// A database that only lives in memory, for tests and experiments.
    pub async fn open_in_memory() -> Result<Store, Error> {
        Self::spawn(|| Ok(Connection::open_in_memory()?)).await
    }

    async fn spawn(
        open: impl FnOnce() -> Result<Connection, Error> + Send + 'static,
    ) -> Result<Store, Error> {
        let (ops, requests) = std_mpsc::channel();
        let (ready, opened) = oneshot::channel();
        let (failing, failures) = watch::channel(None);
        thread::Builder::new()
            .name("ottie-store".to_owned())
            .spawn(move || {
                let conn = open().and_then(|mut conn| {
                    prepare(&mut conn)?;
                    Ok(conn)
                });
                match conn {
                    Ok(conn) => {
                        let _ = ready.send(Ok(()));
                        serve(conn, requests, failing);
                    }
                    Err(err) => {
                        let _ = ready.send(Err(err));
                    }
                }
            })?;
        opened.await.map_err(|_| Error::StoreStopped)??;
        Ok(Store {
            ops,
            failing: failures,
        })
    }

    pub(crate) async fn load(&self) -> Result<Loaded, Error> {
        let (reply, answer) = oneshot::channel();
        self.ops
            .send(Op::Load(reply))
            .map_err(|_| Error::StoreStopped)?;
        answer.await.map_err(|_| Error::StoreStopped)?
    }

    /// Queues an insert-or-update. Writes happen in order. Failures are logged
    /// rather than returned: a failed save must not stop a download.
    pub(crate) fn save(&self, record: JobRecord) {
        if self.ops.send(Op::Save(Box::new(record))).is_err() {
            tracing::error!("database thread has stopped; job not saved");
        }
    }

    /// Queued like `save`, and written in one transaction: all or none.
    pub(crate) fn save_jobs(&self, records: Vec<JobRecord>) {
        if self.ops.send(Op::SaveJobs(records)).is_err() {
            tracing::error!("database thread has stopped; jobs not saved");
        }
    }

    /// Queued like `save`.
    pub(crate) fn save_settings(&self, settings: Settings) {
        if self.ops.send(Op::SaveSettings(settings)).is_err() {
            tracing::error!("database thread has stopped; settings not saved");
        }
    }

    /// Queued like `save`. Replaces the category's extensions too.
    pub(crate) fn save_category(&self, category: Category) {
        if self.ops.send(Op::SaveCategory(category)).is_err() {
            tracing::error!("database thread has stopped; category not saved");
        }
    }

    /// Queued like `save`. Its jobs must have been saved in another category first.
    pub(crate) fn remove_category(&self, id: CategoryId) {
        if self.ops.send(Op::RemoveCategory(id)).is_err() {
            tracing::error!("database thread has stopped; category not removed");
        }
    }

    /// Queued like `save`. Replaces the view's lists too.
    pub(crate) fn save_view(&self, view: View) {
        if self.ops.send(Op::SaveView(view)).is_err() {
            tracing::error!("database thread has stopped; view not saved");
        }
    }

    /// Queued like `save`.
    pub(crate) fn remove_view(&self, id: ViewId) {
        if self.ops.send(Op::RemoveView(id)).is_err() {
            tracing::error!("database thread has stopped; view not removed");
        }
    }

    /// Queued like `save`.
    pub(crate) fn save_certificate(&self, certificate: TrustedCertificate) {
        if self.ops.send(Op::SaveCertificate(certificate)).is_err() {
            tracing::error!("database thread has stopped; certificate not saved");
        }
    }

    /// Queued like `save`.
    pub(crate) fn remove_certificate(&self, id: CertificateId) {
        if self.ops.send(Op::RemoveCertificate(id)).is_err() {
            tracing::error!("database thread has stopped; certificate not removed");
        }
    }

    /// Queued like `save`. Deletes the job and its segments, and records
    /// `last_id`, the highest ID handed out, so it's never handed out again.
    pub(crate) fn remove_job(&self, id: JobId, last_id: JobId) {
        if self.ops.send(Op::RemoveJob { id, last_id }).is_err() {
            tracing::error!("database thread has stopped; job not removed");
        }
    }

    /// Queued like `save`.
    pub(crate) fn save_slot(&self, slot: SlotRecord) {
        if self.ops.send(Op::SaveSlot(slot)).is_err() {
            tracing::error!("database thread has stopped; time slot not saved");
        }
    }

    /// Queued like `save`.
    pub(crate) fn remove_slot(&self, id: SlotId) {
        if self.ops.send(Op::RemoveSlot(id)).is_err() {
            tracing::error!("database thread has stopped; time slot not removed");
        }
    }

    /// Waits until every save queued before this call has been written;
    /// the first one that failed since the last flush, if one did.
    pub(crate) async fn flush(&self) -> Result<(), Error> {
        let (reply, done) = oneshot::channel();
        self.ops
            .send(Op::Flush(reply))
            .map_err(|_| Error::StoreStopped)?;
        done.await.map_err(|_| Error::StoreStopped)?
    }

    /// While writes fail, the last one's error: follow it to tell clients.
    pub(crate) fn failing(&self) -> watch::Receiver<Option<String>> {
        self.failing.clone()
    }
}

/// Runs on the database thread until the `Store` is dropped. A write that
/// fails is logged, kept for the next flush, and shown on `failing` until
/// a later write works.
fn serve(
    mut conn: Connection,
    requests: std_mpsc::Receiver<Op>,
    failing: watch::Sender<Option<String>>,
) {
    // The first write that failed since the last flush.
    let mut first: Option<String> = None;
    while let Ok(op) = requests.recv() {
        let written = match op {
            Op::Load(reply) => {
                let _ = reply.send(load_all(&conn));
                continue;
            }
            Op::Flush(reply) => {
                let _ = reply.send(
                    first
                        .take()
                        .map_or(Ok(()), |err| Err(Error::StorageFailed(err))),
                );
                continue;
            }
            Op::Save(record) => save(&mut conn, std::slice::from_ref(&*record))
                .map_err(|err| failed(err, &format!("could not save job {}", record.id))),
            Op::SaveJobs(records) => save(&mut conn, &records)
                .map_err(|err| failed(err, &format!("could not save {} jobs", records.len()))),
            Op::SaveSettings(settings) => save_settings(&conn, &settings)
                .map_err(|err| failed(err, "could not save settings")),
            Op::SaveCategory(category) => save_category(&mut conn, &category)
                .map_err(|err| failed(err, &format!("could not save category {}", category.id))),
            Op::RemoveCategory(id) => conn
                .execute("DELETE FROM categories WHERE id = ?1", [id.0])
                .map(drop)
                .map_err(|err| failed(err, &format!("could not remove category {id}"))),
            Op::SaveView(view) => save_view(&mut conn, &view)
                .map_err(|err| failed(err, &format!("could not save view {}", view.id))),
            Op::RemoveView(id) => conn
                .execute("DELETE FROM views WHERE id = ?1", [id.0])
                .map(drop)
                .map_err(|err| failed(err, &format!("could not remove view {id}"))),
            Op::RemoveJob { id, last_id } => remove_job(&mut conn, id, last_id)
                .map_err(|err| failed(err, &format!("could not remove job {id}"))),
            Op::SaveSlot(slot) => save_slot(&conn, &slot)
                .map_err(|err| failed(err, &format!("could not save time slot {}", slot.id))),
            Op::RemoveSlot(id) => conn
                .execute("DELETE FROM slots WHERE id = ?1", [id.0])
                .map(drop)
                .map_err(|err| failed(err, &format!("could not remove time slot {id}"))),
            Op::SaveCertificate(certificate) => conn
                .execute(
                    "INSERT INTO certificates (id, host, fingerprint) VALUES (?1, ?2, ?3)",
                    params![certificate.id.0, certificate.host, certificate.fingerprint],
                )
                .map(drop)
                .map_err(|err| {
                    failed(
                        err,
                        &format!("could not save certificate {}", certificate.id),
                    )
                }),
            Op::RemoveCertificate(id) => conn
                .execute("DELETE FROM certificates WHERE id = ?1", [id.0])
                .map(drop)
                .map_err(|err| failed(err, &format!("could not remove certificate {id}"))),
        };
        match written {
            Ok(()) => {
                failing.send_if_modified(|now| now.take().is_some());
            }
            Err(text) => {
                first.get_or_insert_with(|| text.clone());
                failing.send_if_modified(|now| {
                    let new = now.as_deref() != Some(text.as_str());
                    *now = Some(text);
                    new
                });
            }
        }
    }
}

/// Logs a failed write; its error's text, for those it's shown to.
fn failed(err: impl std::fmt::Display, what: &str) -> String {
    tracing::error!(error = %err, "{what}");
    err.to_string()
}

fn prepare(conn: &mut Connection) -> Result<(), Error> {
    // WAL: a crash can't corrupt the file, and writes don't block reads.
    // NORMAL: a power cut may lose the last few saves, which only means
    // re-downloading a little; it never makes us trust bytes we don't have.
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // Some schema changes need foreign keys off, so they're only turned on
    // after migrating; `migrate` checks them itself.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    migrate(conn)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<(), Error> {
    let found: usize = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > MIGRATIONS.len() {
        return Err(Error::DatabaseTooNew {
            found,
            supported: MIGRATIONS.len(),
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(found) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        if tx.prepare("PRAGMA foreign_key_check")?.exists([])? {
            // Returning without committing undoes this migration.
            return Err(Error::MigrationBroken(index + 1));
        }
        tx.pragma_update(None, "user_version", index + 1)?;
        tx.commit()?;
    }
    Ok(())
}

/// Upserts the jobs and replaces their segments, in one transaction so a
/// crash can't leave a job and its segments out of step, nor save only some
/// of the jobs.
fn save(conn: &mut Connection, records: &[JobRecord]) -> Result<(), Error> {
    let tx = conn.transaction()?;
    for record in records {
        write_job(&tx, record)?;
    }
    tx.commit()?;
    Ok(())
}

fn write_job(tx: &Connection, record: &JobRecord) -> Result<(), Error> {
    let dl = &record.download;
    let dest_dir = dl
        .dest_dir
        .to_str()
        .ok_or_else(|| Error::NonUtf8Path(dl.dest_dir.clone()))?;
    let (validator_kind, validator_value) = match &dl.validator {
        Some(Validator::ETag(v)) => (Some("etag"), Some(v.as_str())),
        Some(Validator::LastModified(v)) => (Some("last-modified"), Some(v.as_str())),
        Some(Validator::Ftp(v)) => (Some("ftp"), Some(v.as_str())),
        None => (None, None),
    };
    let (secs, seq) = (record.id.secs(), record.id.seq());
    let (kind, number, total, detail) = match &record.problem {
        Some(problem) => {
            let (kind, number, total, detail) = problem_columns(problem);
            (Some(kind), number, total, detail)
        }
        None => (None, None, None, None),
    };
    tx.prepare_cached(
        "INSERT INTO jobs (id_secs, id_seq, url, dest_dir, file_name, state, total,
                           downloaded, validator_kind, validator_value, resumable, error,
                           priority, speed_limit, category_id, category_chosen,
                           problem_kind, problem_number, problem_total, problem_detail,
                           streaming, streaming_chosen, queue_place, start_at, stop_at,
                           ignores_slots, held_by, existing, cookie, referrer, user_agent)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31)
         ON CONFLICT (id_secs, id_seq) DO UPDATE SET
             url = excluded.url, dest_dir = excluded.dest_dir,
             file_name = excluded.file_name, state = excluded.state,
             total = excluded.total, downloaded = excluded.downloaded,
             validator_kind = excluded.validator_kind,
             validator_value = excluded.validator_value,
             resumable = excluded.resumable, error = excluded.error,
             priority = excluded.priority, speed_limit = excluded.speed_limit,
             category_id = excluded.category_id, category_chosen = excluded.category_chosen,
             problem_kind = excluded.problem_kind, problem_number = excluded.problem_number,
             problem_total = excluded.problem_total, problem_detail = excluded.problem_detail,
             streaming = excluded.streaming, streaming_chosen = excluded.streaming_chosen,
             queue_place = excluded.queue_place, start_at = excluded.start_at,
             stop_at = excluded.stop_at, ignores_slots = excluded.ignores_slots,
             held_by = excluded.held_by, existing = excluded.existing,
             cookie = excluded.cookie, referrer = excluded.referrer,
             user_agent = excluded.user_agent",
    )?
    .execute(params![
        secs,
        seq,
        dl.url.as_str(),
        dest_dir,
        dl.file_name,
        state_name(record.state),
        dl.total,
        dl.downloaded,
        validator_kind,
        validator_value,
        dl.resumable,
        record.problem.as_ref().map(Problem::to_string),
        priority_name(record.priority),
        record.speed_limit,
        record.category.0,
        record.category_chosen,
        kind,
        number,
        total,
        detail,
        record.streaming,
        record.streaming_chosen,
        record.place,
        record.start_at.map(unix_millis),
        record.stop_at.map(unix_millis),
        record.ignores_slots,
        record.held_by.map(|slot| slot.0),
        record.existing.map(Existing::as_str),
        dl.headers.cookie,
        dl.headers.referrer,
        dl.headers.user_agent,
    ])?;
    tx.prepare_cached("DELETE FROM segments WHERE job_secs = ?1 AND job_seq = ?2")?
        .execute(params![secs, seq])?;
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO segments (job_secs, job_seq, range_start, range_end, done)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for s in &dl.segments {
            insert.execute(params![secs, seq, s.start, s.end, s.done])?;
        }
    }
    Ok(())
}

/// Segments go with the job (`ON DELETE CASCADE`).
fn remove_job(conn: &mut Connection, id: JobId, last_id: JobId) -> Result<(), Error> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM jobs WHERE id_secs = ?1 AND id_seq = ?2",
        params![id.secs(), id.seq()],
    )?;
    tx.execute(
        "UPDATE settings SET last_id_secs = ?1, last_id_seq = ?2 WHERE id = 1",
        params![last_id.secs(), last_id.seq()],
    )?;
    tx.commit()?;
    Ok(())
}

fn load_all(conn: &Connection) -> Result<Loaded, Error> {
    Ok(Loaded {
        jobs: load_jobs(conn)?,
        settings: load_settings(conn)?,
        categories: load_categories(conn)?,
        last_id: load_last_id(conn)?,
        views: load_views(conn)?,
        slots: load_slots(conn)?,
        certificates: load_certificates(conn)?,
    })
}

fn load_certificates(conn: &Connection) -> Result<Vec<TrustedCertificate>, Error> {
    let mut stmt = conn.prepare("SELECT id, host, fingerprint FROM certificates ORDER BY id")?;
    let certificates = stmt
        .query_map([], |row| {
            Ok(TrustedCertificate {
                id: CertificateId(row.get(0)?),
                host: row.get(1)?,
                fingerprint: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(certificates)
}

fn load_last_id(conn: &Connection) -> Result<Option<JobId>, Error> {
    let (secs, seq): (u64, u32) = conn.query_row(
        "SELECT last_id_secs, last_id_seq FROM settings WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((seq > 0).then(|| JobId::from_parts(secs, seq)))
}

fn load_jobs(conn: &Connection) -> Result<Vec<JobRecord>, Error> {
    let mut stmt = conn.prepare(
        "SELECT id_secs, id_seq, url, dest_dir, file_name, state, total, downloaded,
                validator_kind, validator_value, resumable, error, priority, speed_limit,
                category_id, category_chosen,
                problem_kind, problem_number, problem_total, problem_detail,
                streaming, streaming_chosen, queue_place, start_at, stop_at, ignores_slots,
                held_by, existing, cookie, referrer, user_agent
         FROM jobs ORDER BY id_secs, id_seq",
    )?;
    let mut rows = stmt.query([])?;
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        // A row we can't understand is skipped (and logged) rather than
        // stopping every other download from loading.
        match record_from_row(row)? {
            Ok(record) => records.push(record),
            Err(problem) => tracing::error!(%problem, "skipping unreadable job in database"),
        }
    }

    let mut by_job: BTreeMap<JobId, Vec<Segment>> = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT job_secs, job_seq, range_start, range_end, done
         FROM segments ORDER BY job_secs, job_seq, range_start",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id = JobId::from_parts(row.get(0)?, row.get(1)?);
        by_job.entry(id).or_default().push(Segment {
            start: row.get(2)?,
            end: row.get(3)?,
            done: row.get(4)?,
        });
    }
    for record in &mut records {
        record.download.segments = by_job.remove(&record.id).unwrap_or_default();
    }
    Ok(records)
}

/// The outer `Result` is a database error, the inner one a row with bad data.
fn record_from_row(row: &Row) -> rusqlite::Result<Result<JobRecord, String>> {
    let id = JobId::from_parts(row.get(0)?, row.get(1)?);
    let url: String = row.get(2)?;
    let state: String = row.get(5)?;
    let validator_kind: Option<String> = row.get(8)?;
    let validator_value: Option<String> = row.get(9)?;

    let Ok(url) = Url::parse(&url) else {
        return Ok(Err(format!("job {id}: invalid URL")));
    };
    let Some(state) = parse_state(&state) else {
        return Ok(Err(format!("job {id}: unknown state `{state}`")));
    };
    let priority: String = row.get(12)?;
    let Some(priority) = parse_priority(&priority) else {
        return Ok(Err(format!("job {id}: unknown priority `{priority}`")));
    };
    let validator = match (validator_kind.as_deref(), validator_value) {
        (Some("etag"), Some(v)) => Some(Validator::ETag(v)),
        (Some("last-modified"), Some(v)) => Some(Validator::LastModified(v)),
        (Some("ftp"), Some(v)) => Some(Validator::Ftp(v)),
        _ => None,
    };
    let download = Download {
        url,
        dest_dir: PathBuf::from(row.get::<_, String>(3)?),
        file_name: row.get(4)?,
        total: row.get(6)?,
        downloaded: row.get(7)?,
        validator,
        resumable: row.get(10)?,
        segments: Vec::new(), // filled in by `load`
        placement: None,
        existing: Existing::default(),
        trusted: Vec::new(),
        headers: RequestHeaders {
            cookie: row.get(28)?,
            referrer: row.get(29)?,
            user_agent: row.get(30)?,
        },
    };
    Ok(Ok(JobRecord {
        id,
        state,
        download,
        problem: problem_from_row(row)?,
        priority,
        speed_limit: row.get(13)?,
        category: CategoryId(row.get(14)?),
        category_chosen: row.get(15)?,
        streaming: row.get(20)?,
        streaming_chosen: row.get(21)?,
        place: row.get(22)?,
        start_at: row.get::<_, Option<u64>>(23)?.map(from_unix_millis),
        stop_at: row.get::<_, Option<u64>>(24)?.map(from_unix_millis),
        ignores_slots: row.get(25)?,
        held_by: row.get::<_, Option<u32>>(26)?.map(SlotId),
        // The CHECK allows only known names.
        existing: row
            .get::<_, Option<String>>(27)?
            .as_deref()
            .and_then(Existing::parse),
    }))
}

/// How times are stored: milliseconds since 1970 (UTC); earlier ones as 0.
fn unix_millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

fn from_unix_millis(millis: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(millis)
}

/// How a problem is stored: its kind, up to two numbers and a text.
fn problem_columns(
    problem: &Problem,
) -> (&'static str, Option<u64>, Option<u64>, Option<Cow<'_, str>>) {
    match problem {
        Problem::Network { detail } => {
            ("network", None, None, Some(Cow::Borrowed(detail.as_str())))
        }
        Problem::Redirect { detail } => {
            ("redirect", None, None, Some(Cow::Borrowed(detail.as_str())))
        }
        Problem::HttpStatus { status } => ("http-status", Some(u64::from(*status)), None, None),
        Problem::Stalled { seconds } => ("stalled", Some(*seconds), None, None),
        Problem::Incomplete { received, expected } => {
            ("incomplete", Some(*received), Some(*expected), None)
        }
        Problem::RemoteChanging => ("remote-changing", None, None, None),
        Problem::Disk { detail } => ("disk", None, None, Some(Cow::Borrowed(detail.as_str()))),
        Problem::FileExists { name } => (
            "file-exists",
            None,
            None,
            Some(Cow::Borrowed(name.as_str())),
        ),
        Problem::FtpReply { code, detail } => (
            "ftp-reply",
            Some(u64::from(*code)),
            None,
            Some(Cow::Borrowed(detail.as_str())),
        ),
        // The host has no spaces: the fingerprint first, then it.
        Problem::Certificate { host, fingerprint } => (
            "certificate",
            None,
            None,
            Some(Cow::Owned(format!("{fingerprint} {host}"))),
        ),
        Problem::NoTls => ("no-tls", None, None, None),
        Problem::Other { detail } => ("other", None, None, Some(Cow::Borrowed(detail.as_str()))),
    }
}

/// Columns 11 (`error`) and 16 to 19. A problem that doesn't add up is kept
/// as `Other` with its English text rather than dropped.
fn problem_from_row(row: &Row) -> rusqlite::Result<Option<Problem>> {
    let english: Option<String> = row.get(11)?;
    let Some(kind) = row.get::<_, Option<String>>(16)? else {
        return Ok(None);
    };
    let number: Option<u64> = row.get(17)?;
    let total: Option<u64> = row.get(18)?;
    let detail: Option<String> = row.get(19)?;
    let problem = match (kind.as_str(), number, total, detail) {
        ("network", _, _, Some(detail)) => Some(Problem::Network { detail }),
        ("redirect", _, _, Some(detail)) => Some(Problem::Redirect { detail }),
        ("http-status", Some(status), _, _) => u16::try_from(status)
            .ok()
            .map(|status| Problem::HttpStatus { status }),
        ("stalled", Some(seconds), _, _) => Some(Problem::Stalled { seconds }),
        ("incomplete", Some(received), Some(expected), _) => {
            Some(Problem::Incomplete { received, expected })
        }
        ("remote-changing", ..) => Some(Problem::RemoteChanging),
        ("disk", _, _, Some(detail)) => Some(Problem::Disk { detail }),
        ("file-exists", _, _, Some(name)) => Some(Problem::FileExists { name }),
        ("ftp-reply", Some(code), _, Some(detail)) => u16::try_from(code)
            .ok()
            .map(|code| Problem::FtpReply { code, detail }),
        ("certificate", _, _, Some(detail)) => {
            detail
                .split_once(' ')
                .map(|(fingerprint, host)| Problem::Certificate {
                    host: host.to_owned(),
                    fingerprint: fingerprint.to_owned(),
                })
        }
        ("no-tls", ..) => Some(Problem::NoTls),
        ("other", _, _, Some(detail)) => Some(Problem::Other { detail }),
        _ => None,
    };
    Ok(problem.or_else(|| {
        Some(Problem::Other {
            detail: english.unwrap_or_else(|| format!("unknown problem `{kind}`")),
        })
    }))
}

fn load_settings(conn: &Connection) -> Result<Settings, Error> {
    Ok(conn.query_row(
        "SELECT global_limit, max_concurrent, max_retries, retries_before_yield, retry_wait_ms,
                setup_done, appearance_mode, appearance_palette, language, stream_media,
                max_batch, existing, setup_step
         FROM settings WHERE id = 1",
        [],
        |row| {
            let mode: Option<String> = row.get(6)?;
            let palette: Option<String> = row.get(7)?;
            // The CHECKs allow only known names; a half-set pair counts as unset.
            let appearance = match (mode.as_deref(), palette.as_deref()) {
                (Some(mode), Some(palette)) => AppearanceMode::parse(mode)
                    .zip(Palette::parse(palette))
                    .map(|(mode, palette)| Appearance { mode, palette }),
                _ => None,
            };
            Ok(Settings {
                global_limit: row.get(0)?,
                max_concurrent: row.get(1)?,
                retry: RetryPolicy {
                    max_retries: row.get(2)?,
                    retries_before_yield: row.get(3)?,
                    first_wait: Duration::from_millis(row.get(4)?),
                },
                max_batch: row.get(10)?,
                stream_media: row.get(9)?,
                setup_done: row.get(5)?,
                setup_step: row.get(12)?,
                appearance,
                language: row.get(8)?,
                // The CHECK allows only known names.
                existing: Existing::parse(&row.get::<_, String>(11)?).unwrap_or_default(),
                // Not saved: whether writing works now.
                storage_failing: None,
            })
        },
    )?)
}

fn save_settings(conn: &Connection, settings: &Settings) -> Result<(), Error> {
    let retry = &settings.retry;
    let wait_ms = u64::try_from(retry.first_wait.as_millis()).unwrap_or(u64::MAX);
    conn.execute(
        "UPDATE settings SET global_limit = ?1, max_concurrent = ?2, max_retries = ?3,
                retries_before_yield = ?4, retry_wait_ms = ?5, setup_done = ?6,
                appearance_mode = ?7, appearance_palette = ?8, language = ?9,
                stream_media = ?10, max_batch = ?11, existing = ?12, setup_step = ?13
         WHERE id = 1",
        params![
            settings.global_limit,
            settings.max_concurrent,
            retry.max_retries,
            retry.retries_before_yield,
            wait_ms,
            settings.setup_done,
            settings.appearance.map(|a| a.mode.as_str()),
            settings.appearance.map(|a| a.palette.as_str()),
            settings.language,
            settings.stream_media,
            settings.max_batch,
            settings.existing.as_str(),
            settings.setup_step,
        ],
    )?;
    Ok(())
}

fn load_categories(conn: &Connection) -> Result<Vec<Category>, Error> {
    let mut stmt = conn.prepare(
        "SELECT id, name, folder, speed_limit, max_concurrent, built_in
         FROM categories ORDER BY id",
    )?;
    let mut categories: BTreeMap<CategoryId, Category> = stmt
        .query_map([], |row| {
            let id = CategoryId(row.get(0)?);
            let settings = CategorySettings {
                name: row.get(1)?,
                folder: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
                extensions: Vec::new(), // filled in below
                speed_limit: row.get(3)?,
                max_concurrent: row.get(4)?,
            };
            let built_in = match row.get::<_, Option<String>>(5)?.as_deref() {
                Some("general") => Some(BuiltIn::General),
                Some("other") => Some(BuiltIn::Other),
                _ => None,
            };
            Ok((
                id,
                Category {
                    id,
                    settings,
                    built_in,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut stmt =
        conn.prepare("SELECT category_id, extension FROM category_extensions ORDER BY extension")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if let Some(category) = categories.get_mut(&CategoryId(row.get(0)?)) {
            category.settings.extensions.push(row.get(1)?);
        }
    }
    Ok(categories.into_values().collect())
}

/// Upserts the category and replaces its extensions, in one transaction.
fn save_category(conn: &mut Connection, category: &Category) -> Result<(), Error> {
    let settings = &category.settings;
    let folder = match &settings.folder {
        Some(folder) => Some(
            folder
                .to_str()
                .ok_or_else(|| Error::NonUtf8Path(folder.clone()))?,
        ),
        None => None,
    };
    let id = category.id.0;
    let tx = conn.transaction()?;
    tx.prepare_cached(
        "INSERT INTO categories (id, name, folder, speed_limit, max_concurrent)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (id) DO UPDATE SET
             name = excluded.name, folder = excluded.folder,
             speed_limit = excluded.speed_limit, max_concurrent = excluded.max_concurrent",
    )?
    .execute(params![
        id,
        settings.name,
        folder,
        settings.speed_limit,
        settings.max_concurrent,
    ])?;
    tx.prepare_cached("DELETE FROM category_extensions WHERE category_id = ?1")?
        .execute([id])?;
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO category_extensions (extension, category_id) VALUES (?1, ?2)",
        )?;
        for ext in &settings.extensions {
            insert.execute(params![ext, id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn load_views(conn: &Connection) -> Result<Vec<View>, Error> {
    let mut stmt = conn.prepare(
        "SELECT id, name, any_state, any_category, name_contains, added, sort, direction, grouped
         FROM views ORDER BY id",
    )?;
    let mut views: BTreeMap<ViewId, ViewSettings> = stmt
        .query_map([], |row| {
            let any_state: bool = row.get(2)?;
            let any_category: bool = row.get(3)?;
            // The CHECKs allow only known names; the defaults stand in otherwise.
            let added: String = row.get(5)?;
            let sort: String = row.get(6)?;
            let direction: String = row.get(7)?;
            let settings = ViewSettings {
                name: row.get(1)?,
                states: (!any_state).then(Vec::new), // filled in below
                categories: (!any_category).then(Vec::new),
                name_contains: row.get(4)?,
                added: AddedWithin::parse(&added).unwrap_or_default(),
                sort: SortKey::parse(&sort).unwrap_or_default(),
                direction: SortDirection::parse(&direction).unwrap_or_default(),
                grouped: row.get(8)?,
            };
            Ok((ViewId(row.get(0)?), settings))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut stmt = conn.prepare("SELECT view_id, state FROM view_states")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let state: String = row.get(1)?;
        if let Some(states) = views
            .get_mut(&ViewId(row.get(0)?))
            .and_then(|view| view.states.as_mut())
            && let Some(state) = parse_state(&state)
        {
            states.push(state);
        }
    }
    let mut stmt = conn.prepare("SELECT view_id, category_id FROM view_categories")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if let Some(categories) = views
            .get_mut(&ViewId(row.get(0)?))
            .and_then(|view| view.categories.as_mut())
        {
            categories.push(CategoryId(row.get(1)?));
        }
    }
    views
        .into_iter()
        .map(|(id, settings)| {
            // Rows come in no particular order; this puts the lists in theirs.
            let settings = settings.normalized()?;
            Ok(View { id, settings })
        })
        .collect()
}

/// Upserts the view and replaces its lists, in one transaction.
fn save_view(conn: &mut Connection, view: &View) -> Result<(), Error> {
    let settings = &view.settings;
    let id = view.id.0;
    let tx = conn.transaction()?;
    tx.prepare_cached(
        "INSERT INTO views (id, name, any_state, any_category, name_contains, added, sort,
                            direction, grouped)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (id) DO UPDATE SET
             name = excluded.name, any_state = excluded.any_state,
             any_category = excluded.any_category, name_contains = excluded.name_contains,
             added = excluded.added, sort = excluded.sort, direction = excluded.direction,
             grouped = excluded.grouped",
    )?
    .execute(params![
        id,
        settings.name,
        settings.states.is_none(),
        settings.categories.is_none(),
        settings.name_contains,
        settings.added.as_str(),
        settings.sort.as_str(),
        settings.direction.as_str(),
        settings.grouped,
    ])?;
    tx.prepare_cached("DELETE FROM view_states WHERE view_id = ?1")?
        .execute([id])?;
    tx.prepare_cached("DELETE FROM view_categories WHERE view_id = ?1")?
        .execute([id])?;
    {
        let mut insert =
            tx.prepare_cached("INSERT INTO view_states (view_id, state) VALUES (?1, ?2)")?;
        for &state in settings.states.iter().flatten() {
            insert.execute(params![id, state_name(state)])?;
        }
        let mut insert = tx
            .prepare_cached("INSERT INTO view_categories (view_id, category_id) VALUES (?1, ?2)")?;
        for category in settings.categories.iter().flatten() {
            insert.execute(params![id, category.0])?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn load_slots(conn: &Connection) -> Result<Vec<SlotRecord>, Error> {
    let mut stmt = conn.prepare(
        "SELECT id, place, name, enabled, category_id, days, date, start_minute, end_minute,
                downloads, speed_limit, max_concurrent, stay_paused
         FROM slots ORDER BY place, id",
    )?;
    let mut rows = stmt.query([])?;
    let mut slots = Vec::new();
    while let Some(row) = rows.next()? {
        let id = SlotId(row.get(0)?);
        // A row we can't understand is skipped (and logged), as with jobs.
        match slot_from_row(row)? {
            Some(settings) => slots.push(SlotRecord {
                id,
                place: row.get(1)?,
                settings,
            }),
            None => tracing::error!(%id, "skipping unreadable time slot in database"),
        }
    }
    Ok(slots)
}

fn slot_from_row(row: &Row) -> rusqlite::Result<Option<SlotSettings>> {
    let days: Option<u8> = row.get(5)?;
    let date: Option<String> = row.get(6)?;
    let repeat = match (days, date) {
        (Some(days), None) => Repeat::Weekly(
            ALL_DAYS
                .into_iter()
                .enumerate()
                .filter(|(bit, _)| days & (1 << bit) != 0)
                .map(|(_, day)| day)
                .collect(),
        ),
        (None, Some(date)) => match date.parse::<Date>() {
            Ok(date) => Repeat::Once(date),
            Err(_) => return Ok(None),
        },
        _ => return Ok(None),
    };
    let downloads: Option<String> = row.get(9)?;
    let stay_paused: bool = row.get(12)?;
    let stop = |let_finish| Downloads::Stop {
        let_finish,
        stay_paused,
    };
    let downloads = match downloads.as_deref() {
        None => None,
        Some("allow") => Some(Downloads::Allow),
        Some("stop") => Some(stop(false)),
        Some("stop-let-finish") => Some(stop(true)),
        Some(_) => return Ok(None),
    };
    let (Some(from), Some(to)) = (time_of(row.get(7)?), time_of(row.get(8)?)) else {
        return Ok(None);
    };
    Ok(Some(SlotSettings {
        name: row.get(2)?,
        enabled: row.get(3)?,
        category: row.get::<_, Option<u32>>(4)?.map(CategoryId),
        repeat,
        from,
        to,
        downloads,
        speed_limit: row.get(10)?,
        max_concurrent: row.get(11)?,
    }))
}

/// A time of day from minutes since midnight.
fn time_of(minutes: i64) -> Option<Time> {
    let hour = i8::try_from(minutes / 60).ok()?;
    let minute = i8::try_from(minutes % 60).ok()?;
    Time::new(hour, minute, 0, 0).ok()
}

fn minutes_of(time: Time) -> i64 {
    i64::from(time.hour()) * 60 + i64::from(time.minute())
}

fn save_slot(conn: &Connection, slot: &SlotRecord) -> Result<(), Error> {
    let settings = &slot.settings;
    let (days, date) = match &settings.repeat {
        Repeat::Weekly(days) => {
            let bits = ALL_DAYS
                .into_iter()
                .enumerate()
                .filter(|(_, day)| days.contains(day))
                .fold(0u8, |bits, (bit, _)| bits | 1 << bit);
            (Some(bits), None)
        }
        Repeat::Once(date) => (None, Some(date.to_string())),
    };
    let downloads = settings.downloads.map(|downloads| match downloads {
        Downloads::Allow => "allow",
        Downloads::Stop {
            let_finish: false, ..
        } => "stop",
        Downloads::Stop {
            let_finish: true, ..
        } => "stop-let-finish",
    });
    conn.prepare_cached(
        "INSERT INTO slots (id, place, name, enabled, category_id, days, date, start_minute,
                            end_minute, downloads, speed_limit, max_concurrent, stay_paused)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT (id) DO UPDATE SET
             place = excluded.place, name = excluded.name, enabled = excluded.enabled,
             category_id = excluded.category_id, days = excluded.days, date = excluded.date,
             start_minute = excluded.start_minute, end_minute = excluded.end_minute,
             downloads = excluded.downloads, speed_limit = excluded.speed_limit,
             max_concurrent = excluded.max_concurrent, stay_paused = excluded.stay_paused",
    )?
    .execute(params![
        slot.id.0,
        slot.place,
        settings.name,
        settings.enabled,
        settings.category.map(|c| c.0),
        days,
        date,
        minutes_of(settings.from),
        minutes_of(settings.to),
        downloads,
        settings.speed_limit,
        settings.max_concurrent,
        settings.keeps_paused(),
    ])?;
    Ok(())
}

fn priority_name(priority: Priority) -> &'static str {
    match priority {
        Priority::Low => "low",
        Priority::Normal => "normal",
        Priority::High => "high",
        Priority::Urgent => "urgent",
    }
}

fn parse_priority(name: &str) -> Option<Priority> {
    Some(match name {
        "low" => Priority::Low,
        "normal" => Priority::Normal,
        "high" => Priority::High,
        "urgent" => Priority::Urgent,
        _ => return None,
    })
}

/// How states are written in the database. Independent of the Rust names, so
/// renaming an enum variant can't silently change stored data.
fn state_name(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Connecting => "connecting",
        JobState::Downloading => "downloading",
        JobState::Paused => "paused",
        JobState::Completed => "completed",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
        JobState::Retrying => "retrying",
        JobState::Scheduled => "scheduled",
    }
}

fn parse_state(name: &str) -> Option<JobState> {
    Some(match name {
        "queued" => JobState::Queued,
        "connecting" => JobState::Connecting,
        "downloading" => JobState::Downloading,
        "paused" => JobState::Paused,
        "completed" => JobState::Completed,
        "failed" => JobState::Failed,
        "cancelled" => JobState::Cancelled,
        "retrying" => JobState::Retrying,
        "scheduled" => JobState::Scheduled,
        _ => return None,
    })
}

/// The database holds cookies and links with passwords: only its owner may
/// read it, not the group a system service shares its downloads with
/// (`docs/service.md`). On Unix, a new one is made `0600`, and one made
/// before (with its `-wal` and `-shm`) is set to it; SQLite gives the files
/// it adds later the database's own mode. Elsewhere its folder keeps it
/// private.
#[cfg(unix)]
fn keep_private(path: &std::path::Path) -> std::io::Result<()> {
    use std::io::ErrorKind;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path);
    match created {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        match std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)) {
            Err(err) if err.kind() != ErrorKind::NotFound => return Err(err),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn keep_private(_: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(seq: u32) -> JobRecord {
        let url = Url::parse("https://example.com/file.zip?x=1").unwrap();
        let mut download = Download::new(url, "/downloads").unwrap();
        download.file_name = Some("file.zip".to_owned());
        download.total = Some(u64::from(u32::MAX) * 4);
        download.downloaded = 1234;
        download.validator = Some(Validator::ETag("\"abc\"".to_owned()));
        download.resumable = true;
        download.headers = RequestHeaders {
            cookie: Some("session=abc; theme=dark".to_owned()),
            referrer: Some("https://example.com/files".to_owned()),
            user_agent: None,
        };
        JobRecord {
            id: JobId::from_parts(1_760_279_405, seq),
            state: JobState::Paused,
            download,
            problem: None,
            priority: Priority::High,
            speed_limit: 512 * 1024,
            category: CategoryId::GENERAL,
            category_chosen: false,
            streaming: true,
            streaming_chosen: true,
            place: u64::from(seq) * 10,
            start_at: None,
            stop_at: Some(UNIX_EPOCH + Duration::from_millis(1_760_300_000_123)),
            ignores_slots: true,
            held_by: Some(SlotId(3)),
            existing: Some(Existing::Replace),
        }
    }

    #[tokio::test]
    async fn saves_and_loads_every_field() {
        let store = Store::open_in_memory().await.unwrap();
        let mut failed = record(2);
        failed.state = JobState::Failed;
        failed.problem = Some(Problem::HttpStatus { status: 404 });
        failed.download.validator = Some(Validator::LastModified("yesterday".to_owned()));
        let mut fresh = record(3);
        fresh.download = Download::new(fresh.download.url.clone(), "/x").unwrap();

        for r in [record(1), failed.clone(), fresh.clone()] {
            store.save(r);
        }
        assert_eq!(store.load().await.unwrap().jobs, [record(1), failed, fresh]);
    }

    #[tokio::test]
    async fn scheduled_jobs_keep_their_times() {
        let store = Store::open_in_memory().await.unwrap();
        let mut job = record(1);
        job.state = JobState::Scheduled;
        job.start_at = Some(UNIX_EPOCH + Duration::from_millis(1_760_299_000_456));
        store.save(job.clone());
        assert_eq!(store.load().await.unwrap().jobs, [job]);
    }

    #[test]
    fn rebuilding_for_scheduled_keeps_jobs_and_views() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS[..13].iter().enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO jobs (id_secs, id_seq, url, dest_dir, file_name, state, downloaded,
                               problem_kind, problem_detail, streaming, queue_place)
             VALUES (1, 1, 'https://example.com/a.mkv', '/d', 'a.mkv', 'failed', 40,
                     'disk', 'disk full', 1, 7);
             INSERT INTO segments VALUES (1, 1, 0, 50, 30), (1, 1, 50, 100, 10);
             INSERT INTO views (id, name, any_state, any_category, added, sort, direction, grouped)
             VALUES (1, 'Broken', 0, 1, 'any', 'added', 'asc', 1);
             INSERT INTO view_states VALUES (1, 'failed');",
        )
        .unwrap();
        prepare(&mut conn).unwrap();

        let loaded = load_all(&conn).unwrap();
        let job = &loaded.jobs[0];
        assert_eq!(job.state, JobState::Failed);
        let disk_full = Problem::Disk {
            detail: "disk full".into(),
        };
        assert_eq!(job.problem, Some(disk_full));
        assert_eq!((job.place, job.streaming), (7, true));
        assert_eq!((job.start_at, job.stop_at), (None, None));
        assert_eq!(job.download.segments.len(), 2);
        assert_eq!(
            loaded.views[0].settings.states,
            Some(vec![JobState::Failed])
        );
        // A view can pick scheduled downloads now.
        conn.execute("INSERT INTO view_states VALUES (1, 'scheduled')", [])
            .unwrap();
        // The segments and view states still belong to the rebuilt tables.
        conn.execute_batch("DELETE FROM jobs; DELETE FROM views;")
            .unwrap();
        let left: i64 = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM segments) + (SELECT COUNT(*) FROM view_states)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn slots_are_saved_replaced_and_removed() {
        use jiff::civil::{Weekday, date, time};
        let store = Store::open_in_memory().await.unwrap();
        store.save_category(videos());
        let night = SlotRecord {
            id: SlotId(1),
            place: 2,
            settings: SlotSettings {
                name: "Night".into(),
                repeat: Repeat::Weekly(vec![Weekday::Monday, Weekday::Sunday]),
                downloads: Some(Downloads::Stop {
                    let_finish: true,
                    stay_paused: true,
                }),
                speed_limit: Some(0),
                ..SlotSettings::daily(time(23, 30, 0, 0), time(7, 0, 0, 0))
            },
        };
        let once = SlotRecord {
            id: SlotId(2),
            place: 1,
            settings: SlotSettings {
                enabled: false,
                category: Some(videos().id),
                repeat: Repeat::Once(date(2026, 10, 14)),
                downloads: Some(Downloads::Allow),
                max_concurrent: Some(2),
                ..SlotSettings::daily(time(0, 0, 0, 0), time(0, 0, 0, 0))
            },
        };
        store.save_slot(night.clone());
        store.save_slot(once.clone());
        // In the order of their places.
        assert_eq!(
            store.load().await.unwrap().slots,
            [once.clone(), night.clone()]
        );

        let mut night = night;
        night.place = 0;
        night.settings.downloads = None;
        store.save_slot(night.clone());
        store.remove_slot(SlotId(9)); // Not there: nothing happens.
        assert_eq!(store.load().await.unwrap().slots, [night.clone(), once]);
        // A category's slots go with it.
        store.remove_category(videos().id);
        assert_eq!(store.load().await.unwrap().slots, [night]);
    }

    #[tokio::test]
    async fn every_kind_of_problem_round_trips() {
        let store = Store::open_in_memory().await.unwrap();
        let problems = [
            Problem::Network {
                detail: "connection refused".into(),
            },
            Problem::Redirect {
                detail: "too many redirects".into(),
            },
            Problem::HttpStatus { status: 503 },
            Problem::Stalled { seconds: 60 },
            Problem::Incomplete {
                received: 10,
                expected: u64::from(u32::MAX) * 3,
            },
            Problem::RemoteChanging,
            Problem::Disk {
                detail: "disk full".into(),
            },
            Problem::FileExists {
                name: "film.mkv".into(),
            },
            Problem::FtpReply {
                code: 530,
                detail: "Login incorrect.".into(),
            },
            Problem::Certificate {
                host: "[fe80::1]".into(),
                fingerprint: "ab".repeat(32),
            },
            Problem::NoTls,
            Problem::Other {
                detail: "something".into(),
            },
        ];
        let records: Vec<JobRecord> = (1..)
            .zip(problems)
            .map(|(seq, problem)| JobRecord {
                problem: Some(problem),
                ..record(seq)
            })
            .collect();
        for r in &records {
            store.save(r.clone());
        }
        assert_eq!(store.load().await.unwrap().jobs, records);
    }

    #[test]
    fn rebuilding_jobs_for_existing_files_keeps_them() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS[..17].iter().enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO jobs (id_secs, id_seq, url, dest_dir, file_name, state, total,
                               downloaded, priority, speed_limit, problem_kind,
                               problem_detail, error, streaming, streaming_chosen, queue_place,
                               start_at, stop_at, ignores_slots, held_by)
             VALUES (5, 1, 'https://example.com/a.zip', '/d', 'a.zip', 'scheduled', 100, 40,
                     'high', 7, 'disk', 'disk full', 'file error: disk full', 1, 1, 3,
                     1000, 2000, 1, 4);",
        )
        .unwrap();
        prepare(&mut conn).unwrap();

        let loaded = load_all(&conn).unwrap();
        assert_eq!(loaded.settings.existing, Existing::Rename);
        let [job] = &loaded.jobs[..] else {
            panic!("expected one job, got {:?}", loaded.jobs);
        };
        assert_eq!(job.state, JobState::Scheduled);
        assert_eq!(job.download.file_name.as_deref(), Some("a.zip"));
        assert_eq!(
            (job.download.total, job.download.downloaded),
            (Some(100), 40)
        );
        assert_eq!(
            (job.priority, job.speed_limit, job.place),
            (Priority::High, 7, 3)
        );
        assert_eq!(
            job.problem,
            Some(Problem::Disk {
                detail: "disk full".into()
            })
        );
        assert!(job.streaming && job.streaming_chosen && job.ignores_slots);
        assert_eq!(job.start_at, Some(from_unix_millis(1000)));
        assert_eq!(job.stop_at, Some(from_unix_millis(2000)));
        assert_eq!(job.held_by, Some(SlotId(4)));
        assert_eq!(job.existing, None);
    }

    #[test]
    fn failures_from_before_problems_keep_their_text() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS[..5].iter().enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute(
            "INSERT INTO jobs (id_secs, id_seq, url, dest_dir, state, error)
             VALUES (1, 1, 'https://example.com/a', '/d', 'failed', 'server answered 410 Gone')",
            [],
        )
        .unwrap();
        prepare(&mut conn).unwrap();
        let jobs = load_jobs(&conn).unwrap();
        let expected = Problem::Other {
            detail: "server answered 410 Gone".into(),
        };
        assert_eq!(jobs[0].problem, Some(expected));
    }

    #[tokio::test]
    async fn saving_again_updates_the_row() {
        let store = Store::open_in_memory().await.unwrap();
        store.save(record(1));
        let mut done = record(1);
        done.state = JobState::Completed;
        done.download.downloaded = done.download.total.unwrap();
        store.save(done.clone());
        assert_eq!(store.load().await.unwrap().jobs, [done]);
    }

    #[tokio::test]
    async fn several_jobs_are_saved_at_once() {
        let store = Store::open_in_memory().await.unwrap();
        let mut first = record(1);
        first.download.segments = vec![Segment {
            start: 0,
            end: 50,
            done: 5,
        }];
        let jobs = vec![first, record(2), record(3)];
        store.save_jobs(jobs.clone());
        assert_eq!(store.load().await.unwrap().jobs, jobs);
    }

    #[tokio::test]
    async fn segments_are_saved_and_replaced() {
        let store = Store::open_in_memory().await.unwrap();
        let mut job = record(1);
        let seg = |start, end, done| Segment { start, end, done };
        job.download.segments = vec![seg(0, 50, 50), seg(50, 100, 10)];
        store.save(job.clone());
        assert_eq!(store.load().await.unwrap().jobs, [job.clone()]);

        // After a split there are more ranges; after completion none.
        job.download.segments = vec![seg(0, 50, 50), seg(50, 75, 25), seg(75, 100, 3)];
        store.save(job.clone());
        assert_eq!(store.load().await.unwrap().jobs, [job.clone()]);
        job.download.segments.clear();
        store.save(job.clone());
        assert_eq!(store.load().await.unwrap().jobs, [job]);
    }

    #[tokio::test]
    async fn removed_jobs_are_gone_but_their_ids_are_remembered() {
        let store = Store::open_in_memory().await.unwrap();
        assert_eq!(store.load().await.unwrap().last_id, None);
        let mut older = record(1);
        older.download.segments = vec![Segment {
            start: 0,
            end: 50,
            done: 5,
        }];
        let newer = record(2);
        store.save(older.clone());
        store.save(newer.clone());

        store.remove_job(newer.id, newer.id);
        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.jobs, [older.clone()]);
        assert_eq!(loaded.last_id, Some(newer.id));

        store.remove_job(older.id, newer.id);
        assert!(store.load().await.unwrap().jobs.is_empty());
    }

    #[tokio::test]
    async fn settings_start_at_their_defaults_and_can_change() {
        let store = Store::open_in_memory().await.unwrap();
        assert_eq!(store.load().await.unwrap().settings, Settings::default());
        let changed = Settings {
            global_limit: 1_000_000,
            max_concurrent: 5,
            retry: RetryPolicy {
                max_retries: 4,
                retries_before_yield: 0,
                first_wait: Duration::from_millis(250),
            },
            max_batch: 500,
            stream_media: false,
            setup_done: true,
            setup_step: Some("categories".into()),
            appearance: Some(Appearance {
                mode: AppearanceMode::Light,
                palette: Palette::Neutral,
            }),
            language: Some("pt-BR".into()),
            existing: Existing::Ask,
            storage_failing: None,
        };
        store.save_settings(changed.clone());
        assert_eq!(store.load().await.unwrap().settings, changed);
    }

    #[test]
    fn rebuilding_settings_keeps_them_and_legacy_palettes_become_their_pair() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS[..16].iter().enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "UPDATE settings SET global_limit = 7, max_concurrent = 2, max_retries = 4,
                retries_before_yield = 1, retry_wait_ms = 500, last_id_secs = 9,
                last_id_seq = 3, setup_done = 1, appearance_mode = 'dark',
                appearance_palette = 'neutral-legacy', language = 'fr', stream_media = 0,
                max_batch = 50",
        )
        .unwrap();
        prepare(&mut conn).unwrap();

        let settings = load_all(&conn).unwrap().settings;
        assert_eq!(settings.global_limit, 7);
        assert_eq!(settings.max_concurrent, 2);
        assert_eq!(settings.retry.first_wait, Duration::from_millis(500));
        assert_eq!(settings.max_batch, 50);
        assert!(!settings.stream_media && settings.setup_done);
        assert_eq!(settings.language.as_deref(), Some("fr"));
        let neutral_dark = Appearance {
            mode: AppearanceMode::Dark,
            palette: Palette::Neutral,
        };
        assert_eq!(settings.appearance, Some(neutral_dark));
        let legacy = conn.execute("UPDATE settings SET appearance_palette = 'sea-legacy'", []);
        assert!(legacy.is_err(), "the CHECK refuses legacy palettes");
    }

    fn videos() -> Category {
        Category {
            id: CategoryId(3),
            settings: CategorySettings {
                folder: Some(PathBuf::from("/downloads/videos")),
                extensions: vec!["mkv".into(), "mp4".into()],
                speed_limit: 1_000_000,
                max_concurrent: 2,
                ..CategorySettings::named("Videos")
            },
            built_in: None,
        }
    }

    #[tokio::test]
    async fn general_and_other_exist_from_the_start() {
        let store = Store::open_in_memory().await.unwrap();
        let general = Category {
            id: CategoryId::GENERAL,
            settings: CategorySettings::named("General"),
            built_in: Some(BuiltIn::General),
        };
        let other = Category {
            id: CategoryId(2),
            settings: CategorySettings::named("Other"),
            built_in: Some(BuiltIn::Other),
        };
        assert_eq!(store.load().await.unwrap().categories, [general, other]);
    }

    #[tokio::test]
    async fn categories_are_saved_replaced_and_removed() {
        let store = Store::open_in_memory().await.unwrap();
        let mut videos = videos();
        store.save_category(videos.clone());
        assert_eq!(store.load().await.unwrap().categories[2], videos);

        videos.settings.extensions = vec!["webm".into()];
        videos.settings.folder = None;
        store.save_category(videos.clone());
        assert_eq!(store.load().await.unwrap().categories[2], videos);

        store.remove_category(videos.id);
        assert_eq!(store.load().await.unwrap().categories.len(), 2);
        // Its extension went with it, so another category can take it.
        let mut films = videos;
        films.settings.name = "Films".into();
        store.save_category(films.clone());
        assert_eq!(store.load().await.unwrap().categories[2], films);
    }

    #[tokio::test]
    async fn ftp_validators_and_trusted_certificates_are_kept() {
        let store = Store::open_in_memory().await.unwrap();
        let mut ftp = record(1);
        ftp.download.url = Url::parse("ftp://me:pw@nas.local/a.iso").unwrap();
        ftp.download.validator = Some(Validator::Ftp("1024 20240102030405".into()));
        store.save(ftp.clone());
        let nas = TrustedCertificate {
            id: CertificateId(1),
            host: "nas.local".into(),
            fingerprint: "ab".repeat(32),
        };
        let router = TrustedCertificate {
            id: CertificateId(2),
            host: "192.168.1.1".into(),
            fingerprint: "cd".repeat(32),
        };
        store.save_certificate(nas.clone());
        store.save_certificate(router);
        store.remove_certificate(CertificateId(2));
        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.jobs, [ftp]);
        assert_eq!(loaded.certificates, [nas]);
    }

    #[tokio::test]
    async fn views_are_saved_replaced_and_removed() {
        let store = Store::open_in_memory().await.unwrap();
        store.save_category(videos());
        let mut view = View {
            id: ViewId(1),
            settings: ViewSettings {
                states: Some(vec![JobState::Downloading, JobState::Failed]),
                categories: Some(vec![CategoryId::GENERAL, videos().id]),
                name_contains: "iso".into(),
                added: AddedWithin::Week,
                sort: SortKey::Size,
                direction: SortDirection::Ascending,
                grouped: false,
                ..ViewSettings::named("Big ISOs")
            },
        };
        store.save_view(view.clone());
        let everything = View {
            id: ViewId(2),
            settings: ViewSettings::named("Everything"),
        };
        store.save_view(everything.clone());
        assert_eq!(
            store.load().await.unwrap().views,
            [view.clone(), everything]
        );

        // An empty list is not the same as any.
        view.settings.states = Some(Vec::new());
        view.settings.categories = None;
        store.save_view(view.clone());
        assert_eq!(store.load().await.unwrap().views[0], view);

        store.remove_view(ViewId(2));
        assert_eq!(store.load().await.unwrap().views, [view]);
    }

    #[tokio::test]
    async fn jobs_keep_their_category() {
        let store = Store::open_in_memory().await.unwrap();
        store.save_category(videos());
        let mut job = record(1);
        job.category = videos().id;
        job.category_chosen = true;
        store.save(job.clone());
        assert_eq!(store.load().await.unwrap().jobs, [job]);
    }

    #[test]
    fn jobs_from_before_categories_land_in_general() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS[..3].iter().enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute(
            "INSERT INTO jobs (id_secs, id_seq, url, dest_dir, state)
             VALUES (1, 1, 'https://example.com/a.mkv', '/d', 'paused')",
            [],
        )
        .unwrap();
        prepare(&mut conn).unwrap();
        let jobs = load_jobs(&conn).unwrap();
        assert_eq!(jobs[0].category, CategoryId::GENERAL);
        assert!(!jobs[0].category_chosen);
    }

    #[test]
    fn rebuilding_jobs_keeps_every_row_and_segment() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS[..4].iter().enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO jobs (id_secs, id_seq, url, dest_dir, file_name, state, total,
                               downloaded, priority, speed_limit)
             VALUES (1, 1, 'https://example.com/a.bin', '/d', 'a.bin', 'paused', 100,
                     40, 'high', 7);
             INSERT INTO segments VALUES (1, 1, 0, 50, 30), (1, 1, 50, 100, 10);",
        )
        .unwrap();
        prepare(&mut conn).unwrap();

        let loaded = load_all(&conn).unwrap();
        let job = &loaded.jobs[0];
        assert_eq!((job.priority, job.speed_limit), (Priority::High, 7));
        assert_eq!(job.download.segments.len(), 2);
        assert_eq!(loaded.settings.retry, RetryPolicy::default());
        // The segments still belong to the rebuilt table.
        conn.execute("DELETE FROM jobs", []).unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM segments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0, "deleting a job deletes its segments");
    }

    #[test]
    fn older_databases_get_other_and_count_as_set_up_if_they_have_categories() {
        for (extra_category, expected) in [(false, false), (true, true)] {
            let mut conn = Connection::open_in_memory().unwrap();
            for (i, sql) in MIGRATIONS[..7].iter().enumerate() {
                conn.execute_batch(sql).unwrap();
                conn.pragma_update(None, "user_version", i + 1).unwrap();
            }
            if extra_category {
                conn.execute("INSERT INTO categories (id, name) VALUES (2, 'other')", [])
                    .unwrap();
            }
            prepare(&mut conn).unwrap();
            assert_eq!(load_settings(&conn).unwrap().setup_done, expected);
            let categories = load_categories(&conn).unwrap();
            let other = categories.last().unwrap();
            assert_eq!(other.built_in, Some(BuiltIn::Other));
            // The user's own "other" keeps its name and ID.
            let (id, name) = if extra_category {
                (3, "Other folders")
            } else {
                (2, "Other")
            };
            assert_eq!(
                (other.id, other.settings.name.as_str()),
                (CategoryId(id), name)
            );
        }
    }

    #[test]
    fn older_jobs_queue_in_the_order_they_were_added() {
        let mut conn = Connection::open_in_memory().unwrap();
        for (i, sql) in MIGRATIONS[..11].iter().enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", i + 1).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO jobs (id_secs, id_seq, url, dest_dir, state)
             VALUES (2, 1, 'https://example.com/c', '/d', 'queued'),
                    (1, 2, 'https://example.com/b', '/d', 'queued'),
                    (1, 1, 'https://example.com/a', '/d', 'paused');",
        )
        .unwrap();
        prepare(&mut conn).unwrap();

        let places: Vec<u64> = load_jobs(&conn).unwrap().iter().map(|j| j.place).collect();
        assert_eq!(places, [1, 2, 3]);
    }

    #[test]
    fn priority_names_round_trip() {
        use Priority::*;
        for priority in [Low, Normal, High, Urgent] {
            assert_eq!(parse_priority(priority_name(priority)), Some(priority));
        }
    }

    #[test]
    fn state_names_round_trip() {
        use JobState::*;
        for state in [
            Queued,
            Connecting,
            Downloading,
            Paused,
            Completed,
            Failed,
            Cancelled,
            Retrying,
            Scheduled,
        ] {
            assert_eq!(parse_state(state_name(state)), Some(state));
        }
    }

    #[test]
    fn migrations_set_the_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        prepare(&mut conn).unwrap();
        let version: usize = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len());
        // Running again is a no-op.
        prepare(&mut conn).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn only_its_owner_reads_the_database() {
        use std::os::unix::fs::PermissionsExt;

        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ottie.db");
        let store = Store::open(&path).await.unwrap();
        store.flush().await.unwrap();
        assert_eq!(mode(&path), 0o600);
        let wal = dir.path().join("ottie.db-wal");
        if wal.exists() {
            assert_eq!(mode(&wal), 0o600);
        }
        drop(store);

        // One made before, readable by the group: tightened.
        let older = dir.path().join("older.db");
        std::fs::write(&older, b"").unwrap();
        std::fs::set_permissions(&older, std::fs::Permissions::from_mode(0o640)).unwrap();
        Store::open(&older).await.unwrap();
        assert_eq!(mode(&older), 0o600);
    }
}
