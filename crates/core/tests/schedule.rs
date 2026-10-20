#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The scheduler (docs/decisions/0040): a download's own start and stop
//! time, and time slots.

mod support;

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime};

use ottie_core::{
    CategoryId, CategorySettings, Downloads, Error, HttpBackend, JobChange, JobEvent, JobId,
    JobSnapshot, JobState, Manager, NewJob, PART_SUFFIX, Reorder, Repeat, SlotId, SlotSettings,
    Store, Time, Weekday,
};
use support::{TestServer, test_data};
use tempfile::TempDir;
use tokio::sync::broadcast::Receiver;

const SIZE: usize = 512 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const HOUR: Duration = Duration::from_secs(3600);

struct Setup {
    server: TestServer,
    dir: TempDir,
    manager: Manager,
    events: Receiver<JobEvent>,
    data: Vec<u8>,
}

/// `chunk_delay_ms` slows the server down so jobs stay running long enough to poke at.
async fn setup(max_concurrent: usize, chunk_delay_ms: u64) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().await.unwrap();
    setup_with(store, dir, max_concurrent, chunk_delay_ms).await
}

async fn setup_with(store: Store, dir: TempDir, max_concurrent: usize, delay_ms: u64) -> Setup {
    let data = test_data(SIZE, 1);
    let server = TestServer::start(data.clone()).await;
    server.configure(|b| b.chunk_delay = Some(Duration::from_millis(delay_ms)));
    let manager = Manager::start(HttpBackend::new().unwrap(), store)
        .await
        .unwrap();
    manager.set_max_concurrent(max_concurrent).await.unwrap();
    // Subscribe before adding anything so no event is missed.
    let events = manager.subscribe();
    Setup {
        server,
        dir,
        manager,
        events,
        data,
    }
}

fn from_now(after: Duration) -> SystemTime {
    SystemTime::now() + after
}

impl Setup {
    fn new_job(&self, name: &str) -> NewJob {
        NewJob::new(self.server.url(&format!("/files/{name}"))).in_dir(self.dir.path())
    }

    async fn add(&self, job: NewJob) -> JobId {
        self.manager.add(job).await.unwrap()
    }

    async fn job(&self, id: JobId) -> JobSnapshot {
        self.manager.job(id).await.unwrap()
    }

    async fn change(&self, id: JobId, change: JobChange) -> Result<(), Error> {
        self.manager.change_job(id, change).await
    }

    async fn start_at(&self, id: JobId, at: Option<SystemTime>) -> Result<(), Error> {
        let change = JobChange {
            start_at: Some(at),
            ..JobChange::default()
        };
        self.change(id, change).await
    }

    /// Waits for a `Changed` event putting `id` in `state`.
    async fn wait_for(&mut self, id: JobId, state: JobState) -> JobSnapshot {
        let events = &mut self.events;
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if let JobEvent::Changed(job) = events.recv().await.unwrap()
                    && job.id == id
                    && job.state == state
                {
                    return job;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{id} never became {state:?}"))
    }

    /// Waits until every job in `ids` has reached `state`, in any order.
    async fn wait_for_all(&mut self, ids: &[JobId], state: JobState) -> Vec<JobSnapshot> {
        let events = &mut self.events;
        let mut reached: Vec<JobSnapshot> = Vec::new();
        tokio::time::timeout(TIMEOUT, async {
            while reached.len() < ids.len() {
                if let JobEvent::Changed(job) = events.recv().await.unwrap()
                    && ids.contains(&job.id)
                    && job.state == state
                    && !reached.iter().any(|seen| seen.id == job.id)
                {
                    reached.push(job);
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("not all of {ids:?} became {state:?}"));
        reached
    }

    fn assert_file(&self, job: &JobSnapshot) {
        let path = self.dir.path().join(job.file_name.as_ref().unwrap());
        assert!(std::fs::read(path).unwrap() == self.data, "content differs");
    }

    fn part_len(&self, job: &JobSnapshot) -> u64 {
        let name = format!("{}{PART_SUFFIX}", job.file_name.as_ref().unwrap());
        std::fs::metadata(self.dir.path().join(name)).unwrap().len()
    }
}

#[tokio::test]
async fn a_new_job_waits_for_its_start_time() {
    let mut s = setup(3, 0).await;
    let start = from_now(Duration::from_millis(800));
    let added = Instant::now();
    let id = s.add(s.new_job("a.bin").starting_at(start)).await;

    let job = s.job(id).await;
    assert_eq!(
        (job.state, job.start_at),
        (JobState::Scheduled, Some(start))
    );
    // It joins the queue when the time comes, and the time goes.
    let queued = s.wait_for(id, JobState::Queued).await;
    assert!(
        added.elapsed() >= Duration::from_millis(700),
        "started early"
    );
    assert_eq!(queued.start_at, None);
    let done = s.wait_for(id, JobState::Completed).await;
    s.assert_file(&done);
}

#[tokio::test]
async fn a_start_time_already_past_queues_at_once() {
    let mut s = setup(3, 0).await;
    let past = SystemTime::now() - Duration::from_secs(60);
    let id = s.add(s.new_job("a.bin").starting_at(past)).await;
    let job = s.job(id).await;
    assert_ne!(job.state, JobState::Scheduled);
    assert_eq!(job.start_at, None);
    s.wait_for(id, JobState::Completed).await;
}

#[tokio::test]
async fn resuming_a_scheduled_job_starts_it_now() {
    let mut s = setup(3, 0).await;
    let id = s.add(s.new_job("a.bin").starting_at(from_now(HOUR))).await;
    s.manager.resume(id).await.unwrap();
    let queued = s.job(id).await;
    assert_ne!(queued.state, JobState::Scheduled);
    assert_eq!(queued.start_at, None);
    s.wait_for(id, JobState::Completed).await;
}

#[tokio::test]
async fn a_scheduled_job_can_be_paused_cancelled_and_reordered() {
    let mut s = setup(1, 0).await;
    let first = s.add(s.new_job("a.bin").starting_at(from_now(HOUR))).await;
    let second = s.add(s.new_job("b.bin").starting_at(from_now(HOUR))).await;
    let place = s.job(first).await.place;
    // It keeps a place in the queue, which it takes when its time comes.
    s.manager.reorder(vec![second], Reorder::Top).await.unwrap();
    assert_eq!(s.job(second).await.place, place);

    s.manager.pause(first).await.unwrap();
    let paused = s.job(first).await;
    assert_eq!((paused.state, paused.start_at), (JobState::Paused, None));
    s.manager.cancel(second).await.unwrap();
    s.wait_for(second, JobState::Cancelled).await;
}

#[tokio::test]
async fn a_start_time_makes_a_running_job_wait() {
    let mut s = setup(3, 30).await;
    let id = s.add(s.new_job("data.bin")).await;
    s.wait_for(id, JobState::Downloading).await;

    let later = from_now(HOUR);
    s.start_at(id, Some(later)).await.unwrap();
    let held = s.wait_for(id, JobState::Scheduled).await;
    assert_eq!(held.start_at, Some(later));
    assert!(held.downloaded > 0 && held.downloaded < SIZE as u64);
    assert_eq!(s.part_len(&held), held.downloaded);

    // Taking the time away queues it again; it carries on from there.
    s.start_at(id, None).await.unwrap();
    assert_eq!(s.job(id).await.start_at, None);
    let done = s.wait_for(id, JobState::Completed).await;
    s.assert_file(&done);
    let resumed_from = s.server.requests()[1].range.clone();
    assert_eq!(resumed_from, Some(format!("bytes={}-", held.downloaded)));
}

#[tokio::test]
async fn a_start_time_reschedules_paused_and_queued_jobs() {
    let mut s = setup(1, 20).await;
    let running = s.add(s.new_job("a.bin")).await;
    let queued = s.add(s.new_job("b.bin")).await;
    let paused = s.add(s.new_job("c.bin")).await;
    s.manager.pause(paused).await.unwrap();
    s.wait_for(running, JobState::Downloading).await;

    for id in [queued, paused] {
        s.start_at(id, Some(from_now(HOUR))).await.unwrap();
        assert_eq!(s.job(id).await.state, JobState::Scheduled);
    }
    // Neither takes the slot when it frees up.
    s.wait_for(running, JobState::Completed).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for id in [queued, paused] {
        assert_eq!(s.job(id).await.state, JobState::Scheduled);
    }
    // A new time replaces the old one.
    let soon = from_now(Duration::from_millis(300));
    s.start_at(queued, Some(soon)).await.unwrap();
    assert_eq!(s.job(queued).await.start_at, Some(soon));
    s.wait_for(queued, JobState::Completed).await;
}

#[tokio::test]
async fn a_stop_time_pauses_a_running_job() {
    // About 1.3 s for the whole file.
    let mut s = setup(3, 40).await;
    let stop = from_now(Duration::from_millis(500));
    let id = s.add(s.new_job("data.bin").stopping_at(stop)).await;
    assert_eq!(s.job(id).await.stop_at, Some(stop));

    let paused = s.wait_for(id, JobState::Paused).await;
    assert_eq!(paused.stop_at, None);
    assert!(
        paused.downloaded < SIZE as u64,
        "finished before its stop time"
    );
    assert_eq!(s.part_len(&paused), paused.downloaded);
}

#[tokio::test]
async fn a_stop_time_pauses_a_job_that_never_started() {
    let mut s = setup(1, 20).await;
    let running = s.add(s.new_job("a.bin")).await;
    let waiting = s
        .add(
            s.new_job("b.bin")
                .stopping_at(from_now(Duration::from_millis(300))),
        )
        .await;
    let paused = s.wait_for(waiting, JobState::Paused).await;
    assert_eq!((paused.downloaded, paused.stop_at), (0, None));
    s.wait_for(running, JobState::Completed).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.job(waiting).await.state, JobState::Paused);
}

#[tokio::test]
async fn a_scheduled_job_pauses_if_its_whole_window_has_passed() {
    // Stopped by its own stop time before it could start: a shutdown, say,
    // over the whole window. Both times have passed when it's checked.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ottie.db");
    let s = setup_with(Store::open(&db).await.unwrap(), dir, 3, 0).await;
    let window = s
        .new_job("a.bin")
        .starting_at(from_now(Duration::from_millis(200)))
        .stopping_at(from_now(Duration::from_millis(400)));
    let id = s.add(window).await;
    s.manager.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let Setup { dir, .. } = s;
    let again = setup_with(Store::open(&db).await.unwrap(), dir, 3, 0).await;
    let job = again.job(id).await;
    assert_eq!(job.state, JobState::Paused);
    assert_eq!((job.start_at, job.stop_at), (None, None));
}

#[tokio::test]
async fn a_scheduled_job_still_waits_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ottie.db");
    let s = setup_with(Store::open(&db).await.unwrap(), dir, 3, 0).await;
    let later = from_now(HOUR);
    let id = s.add(s.new_job("a.bin").starting_at(later)).await;
    s.manager.shutdown().await.unwrap();

    let Setup { dir, .. } = s;
    let again = setup_with(Store::open(&db).await.unwrap(), dir, 3, 0).await;
    let job = again.job(id).await;
    // Saved to the millisecond.
    let millis = |at: SystemTime| {
        at.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    };
    assert_eq!(job.state, JobState::Scheduled);
    assert_eq!(job.start_at.map(millis), Some(millis(later)));
}

#[tokio::test]
async fn the_stop_time_must_come_after_the_start_time() {
    let s = setup(3, 0).await;
    let start = from_now(HOUR);
    let refused = s
        .manager
        .add(s.new_job("a.bin").starting_at(start).stopping_at(start))
        .await;
    assert!(
        matches!(refused, Err(Error::StopBeforeStart)),
        "{refused:?}"
    );

    let id = s.add(s.new_job("a.bin").starting_at(start)).await;
    let early_stop = JobChange {
        stop_at: Some(Some(start - Duration::from_secs(1))),
        ..JobChange::default()
    };
    let refused = s.change(id, early_stop).await;
    assert!(
        matches!(refused, Err(Error::StopBeforeStart)),
        "{refused:?}"
    );
    // Both at once are checked together.
    let both = JobChange {
        start_at: Some(Some(start + HOUR)),
        stop_at: Some(Some(start + 2 * HOUR)),
        ..JobChange::default()
    };
    s.change(id, both).await.unwrap();
    let job = s.job(id).await;
    assert_eq!(
        (job.start_at, job.stop_at),
        (Some(start + HOUR), Some(start + 2 * HOUR))
    );
}

#[tokio::test]
async fn finished_jobs_take_no_times() {
    let mut s = setup(3, 0).await;
    let id = s.add(s.new_job("a.bin")).await;
    s.wait_for(id, JobState::Completed).await;
    let refused = s.start_at(id, Some(from_now(HOUR))).await;
    assert!(
        matches!(
            refused,
            Err(Error::InvalidTransition {
                from: JobState::Completed,
                to: JobState::Scheduled
            })
        ),
        "{refused:?}"
    );
    // Taking away a time it doesn't have changes nothing.
    s.start_at(id, None).await.unwrap();
}

#[tokio::test]
async fn a_change_of_times_is_announced_once() {
    let mut s = setup(3, 0).await;
    let id = s.add(s.new_job("a.bin").starting_at(from_now(HOUR))).await;
    // Drain the `Changed` of the add.
    s.wait_for(id, JobState::Scheduled).await;

    let change = JobChange {
        start_at: Some(Some(from_now(2 * HOUR))),
        stop_at: Some(Some(from_now(3 * HOUR))),
        speed_limit: Some(1000),
        ..JobChange::default()
    };
    s.change(id, change).await.unwrap();
    s.change(id, change).await.unwrap(); // The same again: nothing changes.
    s.manager.pause(id).await.unwrap(); // A marker after them.
    let mut changes = Vec::new();
    while let Ok(event) = s.events.try_recv() {
        if let JobEvent::Changed(job) = event {
            changes.push(job.state);
        }
    }
    assert_eq!(changes, [JobState::Scheduled, JobState::Paused]);
}

// Time slots.

/// A slot that covers every moment (each day, midnight to midnight).
fn always(downloads: Downloads) -> SlotSettings {
    let midnight = Time::midnight();
    SlotSettings {
        downloads: Some(downloads),
        ..SlotSettings::daily(midnight, midnight)
    }
}

const STOP: Downloads = Downloads::Stop {
    let_finish: false,
    stay_paused: false,
};

/// Stops downloads, and keeps them paused once it ends.
const STOP_THEN_PAUSE: Downloads = Downloads::Stop {
    let_finish: false,
    stay_paused: true,
};

/// A slot that never covers now: an hour on the day after tomorrow.
fn elsewhere(downloads: Downloads) -> SlotSettings {
    let later = jiff::Zoned::now()
        .date()
        .tomorrow()
        .unwrap()
        .tomorrow()
        .unwrap();
    SlotSettings {
        repeat: Repeat::Weekly(vec![later.weekday()]),
        downloads: Some(downloads),
        ..SlotSettings::daily(Time::constant(3, 0, 0, 0), Time::constant(4, 0, 0, 0))
    }
}

impl Setup {
    /// Waits for a `SlotChanged` event of `id` that says whether it applies.
    async fn wait_for_slot(&mut self, id: SlotId, active: bool) {
        let events = &mut self.events;
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if let JobEvent::SlotChanged(slot) = events.recv().await.unwrap()
                    && slot.id == id
                    && slot.active == active
                {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("slot {id} never became active: {active}"));
    }
}

#[tokio::test]
async fn a_slot_that_stops_downloads_holds_them_until_it_goes() {
    let mut s = setup(3, 30).await;
    let running = s.add(s.new_job("a.bin")).await;
    s.wait_for(running, JobState::Downloading).await;

    let night = s.manager.add_slot(always(STOP)).await.unwrap();
    s.wait_for_slot(night, true).await;
    let held = s.wait_for(running, JobState::Scheduled).await;
    assert_eq!((held.held_by, held.start_at), (Some(night), None));
    assert_eq!(s.part_len(&held), held.downloaded);
    // One added meanwhile waits too.
    let added = s.add(s.new_job("b.bin")).await;
    let job = s.job(added).await;
    assert_eq!((job.state, job.held_by), (JobState::Scheduled, Some(night)));

    s.manager.remove_slot(night).await.unwrap();
    // Both start again at once: either may finish first.
    for done in s.wait_for_all(&[running, added], JobState::Completed).await {
        s.assert_file(&done);
    }
}

#[tokio::test]
async fn a_slot_can_keep_what_it_held_paused_once_it_ends() {
    let mut s = setup(3, 30).await;
    let running = s.add(s.new_job("a.bin")).await;
    s.wait_for(running, JobState::Downloading).await;
    let night = s.manager.add_slot(always(STOP_THEN_PAUSE)).await.unwrap();
    s.wait_for(running, JobState::Scheduled).await;
    let waiting = s.add(s.new_job("b.bin")).await;
    assert_eq!(s.job(waiting).await.held_by, Some(night));

    // It ends (its hours no longer cover now): both pause.
    s.manager
        .update_slot(night, elsewhere(STOP_THEN_PAUSE))
        .await
        .unwrap();
    for paused in s.wait_for_all(&[running, waiting], JobState::Paused).await {
        assert_eq!(paused.held_by, None);
    }
    // Resumed by hand, they finish.
    s.manager.resume(running).await.unwrap();
    s.wait_for(running, JobState::Completed).await;
}

#[tokio::test]
async fn switching_off_a_slot_that_keeps_paused_lets_them_go() {
    let mut s = setup(3, 0).await;
    let night = always(STOP_THEN_PAUSE);
    let id_slot = s.manager.add_slot(night.clone()).await.unwrap();
    let id = s.add(s.new_job("a.bin")).await;
    assert_eq!(s.job(id).await.state, JobState::Scheduled);
    let off = SlotSettings {
        enabled: false,
        ..night
    };
    s.manager.update_slot(id_slot, off).await.unwrap();
    s.wait_for(id, JobState::Completed).await;
}

#[tokio::test]
async fn a_slot_that_ended_while_ottie_was_off_keeps_them_paused() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ottie.db");
    let s = setup_with(Store::open(&db).await.unwrap(), dir, 3, 0).await;
    let night = s.manager.add_slot(always(STOP_THEN_PAUSE)).await.unwrap();
    let id = s.add(s.new_job("a.bin")).await;
    s.manager.shutdown().await.unwrap();

    // Meanwhile, it ended: now it's an hour on another day.
    let Setup { dir, .. } = s;
    let elsewhere = elsewhere(STOP_THEN_PAUSE);
    let Repeat::Weekly(days) = &elsewhere.repeat else {
        unreachable!()
    };
    // A bit per weekday, Monday first.
    let day = 1 << days[0].to_monday_zero_offset();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE slots SET days = ?1, start_minute = 180, end_minute = 240 WHERE id = ?2",
            (day, night.0),
        )
        .unwrap();
    // Settled before the first command is answered.
    let again = setup_with(Store::open(&db).await.unwrap(), dir, 3, 0).await;
    let job = again.job(id).await;
    assert_eq!((job.state, job.held_by), (JobState::Paused, None));
}

#[tokio::test]
async fn a_slot_can_let_running_downloads_finish() {
    let mut s = setup(3, 20).await;
    let running = s.add(s.new_job("a.bin")).await;
    s.wait_for(running, JobState::Downloading).await;
    let gentle = always(Downloads::Stop {
        let_finish: true,
        stay_paused: false,
    });
    let night = s.manager.add_slot(gentle).await.unwrap();
    let waiting = s.add(s.new_job("b.bin")).await;
    assert_eq!(s.job(waiting).await.held_by, Some(night));
    s.wait_for(running, JobState::Completed).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.job(waiting).await.state, JobState::Scheduled);
}

#[tokio::test]
async fn starting_a_held_download_by_hand_runs_it_anyway() {
    let mut s = setup(3, 20).await;
    s.manager.add_slot(always(STOP)).await.unwrap();
    let id = s.add(s.new_job("a.bin")).await;
    assert_eq!(s.job(id).await.state, JobState::Scheduled);
    s.manager.resume(id).await.unwrap();
    s.wait_for(id, JobState::Downloading).await;
    // Paused, the slot holds it again, until it's started by hand again.
    s.manager.pause(id).await.unwrap();
    s.manager.resume(id).await.unwrap();
    let done = s.wait_for(id, JobState::Completed).await;
    s.assert_file(&done);
}

#[tokio::test]
async fn a_category_slot_holds_only_its_category() {
    let mut s = setup(3, 0).await;
    let videos = s
        .manager
        .add_category(CategorySettings::named("Videos"))
        .await
        .unwrap();
    let quiet = SlotSettings {
        category: Some(videos),
        ..always(STOP)
    };
    let slot = s.manager.add_slot(quiet).await.unwrap();
    let video = s.add(s.new_job("film.mkv").in_category(videos)).await;
    let other = s.add(s.new_job("a.bin")).await;
    s.wait_for(other, JobState::Completed).await;
    assert_eq!(s.job(video).await.held_by, Some(slot));

    // Its slots go with the category, and what they held is let go.
    s.manager.remove_category(videos).await.unwrap();
    assert!(s.manager.slots().await.unwrap().is_empty());
    s.wait_for(video, JobState::Completed).await;
}

#[tokio::test]
async fn a_category_slot_that_stops_at_once_wins_over_a_queue_that_lets_finish() {
    let mut s = setup(3, 30).await;
    let videos = s
        .manager
        .add_category(CategorySettings::named("Videos"))
        .await
        .unwrap();
    let video = s.add(s.new_job("film.mkv").in_category(videos)).await;
    s.wait_for(video, JobState::Downloading).await;
    let gentle = always(Downloads::Stop {
        let_finish: true,
        stay_paused: false,
    });
    s.manager.add_slot(gentle).await.unwrap();
    let at_once = SlotSettings {
        category: Some(videos),
        ..always(STOP)
    };
    let quiet = s.manager.add_slot(at_once).await.unwrap();
    let held = s.wait_for(video, JobState::Scheduled).await;
    assert_eq!(held.held_by, Some(quiet));
}

#[tokio::test]
async fn the_higher_slot_wins() {
    let mut s = setup(3, 0).await;
    let stop = s.manager.add_slot(always(STOP)).await.unwrap();
    let allow = s.manager.add_slot(always(Downloads::Allow)).await.unwrap();
    let id = s.add(s.new_job("a.bin")).await;
    assert_eq!(s.job(id).await.held_by, Some(stop));
    // Allow goes above: it wins.
    s.manager.reorder_slots(vec![allow, stop]).await.unwrap();
    let slots = s.manager.slots().await.unwrap();
    let order: Vec<SlotId> = slots.iter().map(|slot| slot.id).collect();
    assert_eq!(order, [allow, stop]);
    s.wait_for(id, JobState::Completed).await;

    let refused = s.manager.reorder_slots(vec![allow]).await;
    assert!(
        matches!(refused, Err(Error::InvalidSlotOrder)),
        "{refused:?}"
    );
    let refused = s.manager.reorder_slots(vec![allow, allow]).await;
    assert!(
        matches!(refused, Err(Error::InvalidSlotOrder)),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_slot_can_lower_how_many_run_at_once() {
    let mut s = setup(3, 5).await;
    let one = SlotSettings {
        max_concurrent: Some(1),
        ..SlotSettings::daily(Time::midnight(), Time::midnight())
    };
    s.manager.add_slot(one).await.unwrap();
    let mut ids = Vec::new();
    for name in ["a.bin", "b.bin", "c.bin"] {
        ids.push(s.add(s.new_job(name)).await);
    }
    let events = &mut s.events;
    let (mut running, mut completed) = (HashSet::new(), 0);
    tokio::time::timeout(TIMEOUT, async {
        while completed < ids.len() {
            if let JobEvent::Changed(job) = events.recv().await.unwrap() {
                match job.state {
                    JobState::Connecting => {
                        running.insert(job.id);
                    }
                    JobState::Completed => {
                        running.remove(&job.id);
                        completed += 1;
                    }
                    _ => {}
                }
                assert!(running.len() <= 1, "more than one running");
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn slots_that_dont_cover_now_change_nothing_until_they_do() {
    let mut s = setup(3, 0).await;
    let later = s.manager.add_slot(elsewhere(STOP)).await.unwrap();
    assert!(!s.manager.slots().await.unwrap()[0].active);
    let id = s.add(s.new_job("a.bin")).await;
    s.wait_for(id, JobState::Completed).await;

    // Switched off, a slot that covers now does nothing either.
    let off = SlotSettings {
        enabled: false,
        ..always(STOP)
    };
    let switched = s.manager.add_slot(off.clone()).await.unwrap();
    let id = s.add(s.new_job("b.bin")).await;
    s.wait_for(id, JobState::Completed).await;
    // Switched on, it applies.
    let on = SlotSettings {
        enabled: true,
        ..off
    };
    s.manager.update_slot(switched, on).await.unwrap();
    s.wait_for_slot(switched, true).await;
    let id = s.add(s.new_job("c.bin")).await;
    assert_eq!(s.job(id).await.held_by, Some(switched));
    assert!(matches!(
        s.manager.update_slot(SlotId(99), elsewhere(STOP)).await,
        Err(Error::SlotNotFound(_))
    ));
    s.manager.remove_slot(later).await.unwrap();
}

#[tokio::test]
async fn slots_say_what_they_need() {
    let s = setup(3, 0).await;
    let no_days = SlotSettings {
        repeat: Repeat::Weekly(Vec::new()),
        ..always(STOP)
    };
    let refused = s.manager.add_slot(no_days).await;
    assert!(
        matches!(refused, Err(Error::SlotWithoutDays)),
        "{refused:?}"
    );
    let idle = SlotSettings::daily(Time::midnight(), Time::midnight());
    let refused = s.manager.add_slot(idle).await;
    assert!(
        matches!(refused, Err(Error::SlotWithoutEffect)),
        "{refused:?}"
    );
    let nowhere = SlotSettings {
        category: Some(CategoryId(77)),
        ..always(STOP)
    };
    let refused = s.manager.add_slot(nowhere).await;
    assert!(
        matches!(refused, Err(Error::CategoryNotFound(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn slots_and_what_they_hold_are_kept_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ottie.db");
    let s = setup_with(Store::open(&db).await.unwrap(), dir, 3, 0).await;
    let weekend = SlotSettings {
        name: "Weekend".into(),
        repeat: Repeat::Weekly(vec![Weekday::Saturday, Weekday::Sunday]),
        speed_limit: Some(1000),
        ..elsewhere(STOP)
    };
    let night = s.manager.add_slot(always(STOP)).await.unwrap();
    s.manager.add_slot(weekend).await.unwrap();
    let id = s.add(s.new_job("a.bin")).await;
    let before = s.manager.slots().await.unwrap();
    s.manager.shutdown().await.unwrap();

    let Setup { dir, .. } = s;
    let again = setup_with(Store::open(&db).await.unwrap(), dir, 3, 0).await;
    assert_eq!(again.manager.slots().await.unwrap(), before);
    let job = again.job(id).await;
    assert_eq!((job.state, job.held_by), (JobState::Scheduled, Some(night)));
}

#[tokio::test]
async fn a_slot_limits_the_total_speed_while_it_lasts() {
    let mut s = setup(3, 0).await;
    // 512 KiB at 160 KiB/s: over 3 s, where it would take a moment.
    let slow = SlotSettings {
        speed_limit: Some(160 * 1024),
        ..SlotSettings::daily(Time::midnight(), Time::midnight())
    };
    s.manager.add_slot(slow).await.unwrap();
    let started = Instant::now();
    let id = s.add(s.new_job("a.bin")).await;
    s.wait_for(id, JobState::Completed).await;
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    // The setting itself didn't change.
    assert_eq!(s.manager.settings().await.unwrap().global_limit, 0);
}
