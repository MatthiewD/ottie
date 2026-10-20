#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::PathBuf;
use std::time::Duration;

use ottie_core::{
    Appearance, AppearanceMode, CategoryId, CategorySettings, Error, Existing, HttpBackend,
    HttpConfig, JobChange, JobId, JobSnapshot, JobState, Manager, NewJob, PART_SUFFIX, Palette,
    Priority, Reorder, RetryPolicy, Settings, SettingsChange, Store,
};
use support::{TestServer, test_data};
use tempfile::TempDir;

const SIZE: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

struct Setup {
    server: TestServer,
    _dir: TempDir,
    db: PathBuf,
    downloads: PathBuf,
    data: Vec<u8>,
}

async fn setup() -> Setup {
    let data = test_data(SIZE, 1);
    let server = TestServer::start(data.clone()).await;
    // Slow enough (about 0.3 s per file) to stop a download halfway.
    server.configure(|b| b.chunk_delay = Some(Duration::from_millis(5)));
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("downloads");
    std::fs::create_dir(&downloads).unwrap();
    Setup {
        server,
        db: dir.path().join("ottie.db"),
        _dir: dir,
        downloads,
        data,
    }
}

impl Setup {
    /// Starts a manager on the test database, as Ottie does at launch. The
    /// default HTTP settings use one connection for this 1 MiB file.
    async fn launch(&self) -> Manager {
        self.launch_with(HttpConfig::default()).await
    }

    async fn launch_with(&self, http: HttpConfig) -> Manager {
        let store = Store::open(&self.db).await.unwrap();
        let backend = HttpBackend::with_config(http).unwrap();
        Manager::start(backend, store).await.unwrap()
    }

    /// A job that waits an hour before it starts: nothing downloads.
    async fn add_later(&self, manager: &Manager) -> Result<JobId, Error> {
        let url = self.server.url("/files/data.bin");
        let later = std::time::SystemTime::now() + Duration::from_secs(3600);
        manager
            .add(NewJob::new(url).in_dir(&self.downloads).starting_at(later))
            .await
    }

    async fn add(&self, manager: &Manager) -> JobId {
        let url = self.server.url("/files/data.bin");
        manager
            .add(NewJob::new(url).in_dir(&self.downloads))
            .await
            .unwrap()
    }

    fn part(&self) -> PathBuf {
        self.downloads.join(format!("data.bin{PART_SUFFIX}"))
    }

    fn part_len(&self) -> u64 {
        std::fs::metadata(self.part()).unwrap().len()
    }

    fn assert_complete(&self) {
        let file = self.downloads.join("data.bin");
        assert!(std::fs::read(file).unwrap() == self.data, "content differs");
        assert!(!self.part().exists());
    }
}

/// Polls until the job reaches `state`. (Polling rather than events, because a
/// freshly started manager may change state before we can subscribe.)
async fn wait_until(manager: &Manager, id: JobId, state: JobState) -> JobSnapshot {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let jobs = manager.list().await.unwrap();
            if let Some(job) = jobs.into_iter().find(|j| j.id == id && j.state == state) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("job {id} never became {state:?}"))
}

#[tokio::test]
async fn running_jobs_continue_after_a_restart() {
    let s = setup().await;
    let manager = s.launch().await;
    let id = s.add(&manager).await;
    wait_until(&manager, id, JobState::Downloading).await;
    manager.shutdown().await.unwrap();

    let saved = s.part_len();
    assert!(saved > 0 && saved < SIZE as u64, "stopped at {saved}");

    let manager = s.launch().await;
    wait_until(&manager, id, JobState::Completed).await;
    s.assert_complete();
    let resumed = &s.server.requests()[1];
    assert_eq!(resumed.range, Some(format!("bytes={saved}-")));
    assert_eq!(resumed.if_range.as_deref(), Some("\"v1\""));
}

#[tokio::test]
async fn paused_jobs_stay_paused_after_a_restart() {
    let s = setup().await;
    let manager = s.launch().await;
    let id = s.add(&manager).await;
    wait_until(&manager, id, JobState::Downloading).await;
    manager.pause(id).await.unwrap();
    manager.shutdown().await.unwrap();

    let manager = s.launch().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let job = wait_until(&manager, id, JobState::Paused).await;
    assert_eq!(job.downloaded, s.part_len());
    assert_eq!(s.server.requests().len(), 1, "a paused job must not start");

    manager.resume(id).await.unwrap();
    wait_until(&manager, id, JobState::Completed).await;
    s.assert_complete();
}

#[tokio::test]
async fn crash_with_stale_progress_resumes_from_the_file() {
    let s = setup().await;
    let manager = s.launch().await;
    let id = s.add(&manager).await;
    wait_until(&manager, id, JobState::Downloading).await;
    manager.pause(id).await.unwrap();
    manager.shutdown().await.unwrap();
    let on_disk = s.part_len();

    // Make the database look like Ottie crashed mid-download, before its
    // last progress save: still "downloading", with an outdated byte count.
    let db = rusqlite::Connection::open(&s.db).unwrap();
    db.execute("UPDATE jobs SET state = 'downloading', downloaded = 0", [])
        .unwrap();
    drop(db);

    let manager = s.launch().await;
    wait_until(&manager, id, JobState::Completed).await;
    s.assert_complete();
    let resumed = &s.server.requests()[1];
    assert_eq!(resumed.range, Some(format!("bytes={on_disk}-")));
}

#[tokio::test]
async fn history_and_ids_survive_a_restart() {
    let s = setup().await;
    s.server.configure(|b| b.chunk_delay = None);
    let manager = s.launch().await;
    let first = s.add(&manager).await;
    let done = wait_until(&manager, first, JobState::Completed).await;
    manager.shutdown().await.unwrap();

    let manager = s.launch().await;
    let jobs = manager.list().await.unwrap();
    assert_eq!(jobs, [done]);

    let url = s.server.url("/files/other.bin");
    let second = manager
        .add(NewJob::new(url).in_dir(&s.downloads))
        .await
        .unwrap();
    assert!(second > first);
    wait_until(&manager, second, JobState::Completed).await;
}

#[tokio::test]
async fn a_database_from_a_newer_ottie_is_refused() {
    let s = setup().await;
    let db = rusqlite::Connection::open(&s.db).unwrap();
    db.pragma_update(None, "user_version", 99).unwrap();
    drop(db);

    let result = Store::open(&s.db).await;
    assert!(
        matches!(result, Err(Error::DatabaseTooNew { found: 99, .. })),
        "got {:?}",
        result.err()
    );
}

#[tokio::test]
async fn multi_connection_downloads_survive_restarts_and_crashes() {
    let s = setup().await;
    // Four connections are fast; slow down enough to quit halfway.
    s.server
        .configure(|b| b.chunk_delay = Some(Duration::from_millis(20)));
    let four = HttpConfig {
        connections: 4,
        min_split: 64 * 1024,
        ..HttpConfig::default()
    };
    let count_segments = || {
        let db = rusqlite::Connection::open(&s.db).unwrap();
        db.query_row("SELECT COUNT(*) FROM segments", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };

    // Quit halfway: the ranges are in the database.
    let manager = s.launch_with(four.clone()).await;
    let id = s.add(&manager).await;
    wait_until(&manager, id, JobState::Downloading).await;
    manager.shutdown().await.unwrap();
    assert!(count_segments() >= 4);
    assert_eq!(s.part_len(), SIZE as u64, "full size from the start");

    // Crash: pretend the last progress saves never happened. The ranges
    // restart from their beginning, which is safe, just slower.
    let db = rusqlite::Connection::open(&s.db).unwrap();
    db.execute("UPDATE segments SET done = 0", []).unwrap();
    drop(db);

    let manager = s.launch_with(four).await;
    wait_until(&manager, id, JobState::Completed).await;
    s.assert_complete();
    assert_eq!(count_segments(), 0, "ranges are removed once complete");
}

#[tokio::test]
async fn settings_priorities_and_limits_survive_a_restart() {
    let s = setup().await;
    let manager = s.launch().await;
    manager.set_global_limit(5_000_000).await.unwrap();
    manager.set_max_concurrent(2).await.unwrap();
    let retry = RetryPolicy {
        max_retries: 4,
        retries_before_yield: 1,
        first_wait: Duration::from_millis(1500),
    };
    manager.set_retry_policy(retry).await.unwrap();
    let appearance = Appearance {
        mode: AppearanceMode::Dark,
        palette: Palette::Neutral,
    };
    let change = SettingsChange {
        max_batch: Some(500),
        stream_media: Some(false),
        setup_done: Some(false),
        setup_step: Some(Some("downloads".into())),
        appearance: Some(appearance),
        language: Some(Some("fr".into())),
        existing: Some(Existing::Replace),
        ..SettingsChange::default()
    };
    manager.change_settings(change).await.unwrap();
    let id = s.add(&manager).await;
    manager.pause(id).await.unwrap();
    manager.set_priority(id, Priority::High).await.unwrap();
    manager.set_speed_limit(id, 123_456).await.unwrap();
    let streaming = JobChange {
        streaming: Some(true),
        ..JobChange::default()
    };
    manager.change_job(id, streaming).await.unwrap();
    manager.shutdown().await.unwrap();

    let manager = s.launch().await;
    let expected = Settings {
        global_limit: 5_000_000,
        max_concurrent: 2,
        retry,
        max_batch: 500,
        stream_media: false,
        setup_done: false,
        setup_step: Some("downloads".into()),
        appearance: Some(appearance),
        language: Some("fr".into()),
        existing: Existing::Replace,
        storage_failing: None,
    };
    assert_eq!(manager.settings().await.unwrap(), expected);
    let job = &manager.list().await.unwrap()[0];
    assert_eq!((job.priority, job.speed_limit), (Priority::High, 123_456));
    assert!(job.streaming);
}

#[tokio::test]
async fn places_in_the_queue_survive_a_restart() {
    let s = setup().await;
    let manager = s.launch().await;
    let first = s.add(&manager).await;
    let second = s.add(&manager).await;
    for id in [first, second] {
        manager.pause(id).await.unwrap();
    }
    manager.reorder(vec![second], Reorder::Top).await.unwrap();
    manager.shutdown().await.unwrap();

    let manager = s.launch().await;
    let third = s.add(&manager).await;
    let places: Vec<(JobId, u64)> = manager
        .list()
        .await
        .unwrap()
        .iter()
        .map(|job| (job.id, job.place))
        .collect();
    // A new job still goes after every other.
    assert_eq!(places, [(first, 2), (second, 1), (third, 3)]);
}

#[tokio::test]
async fn categories_and_job_categories_survive_a_restart() {
    let s = setup().await;
    let manager = s.launch().await;
    let videos = CategorySettings {
        folder: Some(s.downloads.join("videos")),
        extensions: vec!["mkv".into(), "mp4".into()],
        speed_limit: 1_000_000,
        max_concurrent: 1,
        ..CategorySettings::named("Videos")
    };
    let id = manager.add_category(videos).await.unwrap();
    let job = s.add(&manager).await;
    manager.pause(job).await.unwrap();
    manager.set_category(job, id).await.unwrap();
    let before = manager.categories().await.unwrap();
    manager.shutdown().await.unwrap();

    let manager = s.launch().await;
    assert_eq!(manager.categories().await.unwrap(), before);
    assert_eq!(manager.list().await.unwrap()[0].category, id);
    // Removing it still works on a reloaded database.
    manager.remove_category(id).await.unwrap();
    manager.shutdown().await.unwrap();
    let manager = s.launch().await;
    assert_eq!(
        manager.categories().await.unwrap().len(),
        2,
        "General and Other"
    );
    assert_eq!(
        manager.list().await.unwrap()[0].category,
        CategoryId::GENERAL
    );
}

#[tokio::test]
async fn a_job_waiting_to_retry_continues_after_a_restart() {
    let s = setup().await;
    s.server.configure(|b| {
        b.chunk_delay = None;
        b.fail_next = Some((axum::http::StatusCode::SERVICE_UNAVAILABLE, 1));
    });
    let manager = s.launch().await;
    let long_wait = RetryPolicy {
        first_wait: Duration::from_secs(3600),
        ..RetryPolicy::default()
    };
    manager.set_retry_policy(long_wait).await.unwrap();
    let id = s.add(&manager).await;
    wait_until(&manager, id, JobState::Retrying).await;
    manager.shutdown().await.unwrap();

    // The hour-long wait doesn't survive the restart: it's queued again.
    let manager = s.launch().await;
    wait_until(&manager, id, JobState::Completed).await;
    s.assert_complete();
}

#[tokio::test]
async fn removed_jobs_stay_removed_and_their_ids_are_not_reused() {
    let s = setup().await;
    let manager = s.launch().await;
    let kept = s.add(&manager).await;
    wait_until(&manager, kept, JobState::Completed).await;
    let removed = s.add(&manager).await;
    manager.remove(removed).await.unwrap();
    manager.shutdown().await.unwrap();

    let manager = s.launch().await;
    let ids: Vec<JobId> = manager.list().await.unwrap().iter().map(|j| j.id).collect();
    assert_eq!(ids, [kept]);
    // Likely within the same second as `removed`, which only the saved
    // last ID can tell apart.
    let next = s.add(&manager).await;
    assert!(next > removed, "{next} reuses or precedes {removed}");
}

/// Makes these writes to `table` fail, as a full disk would, until
/// `writes_work` (through a second connection to the same database).
fn writes_fail(db: &std::path::Path, table: &str, events: &[&str]) {
    let conn = rusqlite::Connection::open(db).unwrap();
    for event in events {
        conn.execute_batch(&format!(
            "CREATE TRIGGER fail_{table}_{event} BEFORE {event} ON {table} \
             BEGIN SELECT RAISE(ABORT, 'test: the disk is full'); END;"
        ))
        .unwrap();
    }
}

fn writes_work(db: &std::path::Path, table: &str, events: &[&str]) {
    let conn = rusqlite::Connection::open(db).unwrap();
    for event in events {
        conn.execute_batch(&format!("DROP TRIGGER fail_{table}_{event};"))
            .unwrap();
    }
}

/// Every way a row of the settings is written.
const SAVING: [&str; 2] = ["INSERT", "UPDATE"];

/// Waits for settings whose `storage_failing` is `failing`.
async fn wait_for_storage(
    events: &mut tokio::sync::broadcast::Receiver<ottie_core::JobEvent>,
    failing: bool,
) -> Settings {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let ottie_core::JobEvent::SettingsChanged(settings) = events.recv().await.unwrap()
                && settings.storage_failing.is_some() == failing
            {
                return settings;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("storage never became failing: {failing}"))
}

#[tokio::test]
async fn a_failing_database_is_told_and_shutdown_says_so() {
    let s = setup().await;
    let manager = s.launch().await;
    let mut events = manager.subscribe();
    let change = |max| SettingsChange {
        max_concurrent: Some(max),
        ..SettingsChange::default()
    };

    // A change in the background: the progress of a download.
    writes_fail(&s.db, "jobs", &SAVING);
    let id = s.add_later(&manager).await.unwrap_err();
    assert!(matches!(id, Error::StorageFailed(_)), "{id:?}");
    let told = wait_for_storage(&mut events, true).await;
    assert!(
        told.storage_failing
            .as_deref()
            .unwrap()
            .contains("the disk is full"),
        "{told:?}"
    );
    assert!(manager.settings().await.unwrap().storage_failing.is_some());

    // Working again: said too.
    writes_work(&s.db, "jobs", &SAVING);
    manager.change_settings(change(6)).await.unwrap();
    wait_for_storage(&mut events, false).await;

    // A shutdown whose last saves failed says so: here a job's, which was
    // queued (a scheduled one is saved as its time is cleared).
    let id = s.add_later(&manager).await.unwrap();
    writes_fail(&s.db, "jobs", &SAVING);
    let clear = JobChange {
        start_at: Some(None),
        ..JobChange::default()
    };
    manager.change_job(id, clear).await.unwrap();
    let err = manager.shutdown().await.unwrap_err();
    assert!(
        matches!(&err, Error::StorageFailed(text) if text.contains("the disk is full")),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_change_that_cant_be_saved_isnt_made() {
    let s = setup().await;
    let manager = s.launch().await;
    let failed = |result: Result<(), Error>| {
        assert!(matches!(result, Err(Error::StorageFailed(_))), "{result:?}");
    };

    writes_fail(&s.db, "settings", &SAVING);
    let change = SettingsChange {
        max_concurrent: Some(5),
        ..SettingsChange::default()
    };
    failed(manager.change_settings(change).await.map(drop));
    assert_eq!(manager.settings().await.unwrap().max_concurrent, 3);
    writes_work(&s.db, "settings", &SAVING);

    writes_fail(&s.db, "jobs", &["INSERT"]);
    failed(s.add_later(&manager).await.map(drop));
    let one = s.server.url("/files/one.bin");
    let many = vec![NewJob::new(one).in_dir(&s.downloads)];
    failed(manager.add_many(many).await.map(drop));
    assert!(manager.list().await.unwrap().is_empty());
    writes_work(&s.db, "jobs", &["INSERT"]);

    let kept = s.add_later(&manager).await.unwrap();
    writes_fail(&s.db, "jobs", &["DELETE"]);
    failed(manager.remove(kept).await);
    assert_eq!(manager.list().await.unwrap().len(), 1, "still listed");
    writes_work(&s.db, "jobs", &["DELETE"]);

    writes_fail(&s.db, "categories", &["INSERT"]);
    let films = CategorySettings::named("Films");
    failed(manager.add_category(films).await.map(drop));
    let names: Vec<String> = manager
        .categories()
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.settings.name)
        .collect();
    assert!(!names.contains(&"Films".to_owned()), "{names:?}");
    writes_work(&s.db, "categories", &["INSERT"]);
    manager.shutdown().await.unwrap();

    // What it said is what the database holds.
    let manager = s.launch().await;
    assert_eq!(manager.settings().await.unwrap().max_concurrent, 3);
    let ids: Vec<JobId> = manager.list().await.unwrap().iter().map(|j| j.id).collect();
    assert_eq!(ids, [kept]);
}

#[tokio::test]
async fn a_new_plan_is_saved_before_the_file_takes_its_full_size() {
    let s = setup().await;
    let manager = s
        .launch_with(HttpConfig {
            connections: 4,
            min_split: 64 * 1024,
            ..HttpConfig::default()
        })
        .await;
    let id = s.add_later(&manager).await.unwrap();

    // Its plan can't be saved: it must not preallocate the file, or a crash
    // could leave the old map over it.
    writes_fail(&s.db, "jobs", &SAVING);
    let now = JobChange {
        start_at: Some(None),
        ..JobChange::default()
    };
    manager.change_job(id, now).await.unwrap();
    let failed = wait_until(&manager, id, JobState::Failed).await;
    assert!(
        failed
            .problem
            .as_ref()
            .unwrap()
            .to_string()
            .contains("database"),
        "{:?}",
        failed.problem
    );
    assert_eq!(s.part_len(), 0, "the file never took its full size");
}
