#![allow(clippy::unwrap_used, clippy::expect_used)]

//! A hold released while the download it stopped is still stopping: the
//! download goes back to the queue and starts again (docs/decisions/0040).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use jiff::civil::Time;
use ottie_core::{
    Backend, Download, Downloads, Error, Event, JobChange, JobEvent, JobId, JobState, Manager,
    NewJob, Outcome, Playback, SlotSettings, Store, Throttle,
};
use reqwest::Url;
use tempfile::TempDir;
use tokio::sync::Semaphore;
use tokio::sync::broadcast::Receiver;
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Downloads until asked to stop, then waits at `gate` before it has
/// stopped: the moment to change what held it.
struct Gated {
    runs: Arc<AtomicUsize>,
    gate: Arc<Semaphore>,
}

impl Backend for Gated {
    async fn run(
        &self,
        dl: &mut Download,
        _: &Throttle,
        _: &Playback,
        cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Outcome, Error> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        on_event(Event::Started {
            file_name: "held.bin".to_owned(),
            dest_dir: dl.dest_dir.clone(),
            total: Some(1024),
            validator: None,
            resumable: false,
            downloaded: 0,
            segments: Vec::new(),
        });
        on_event(Event::Progress {
            downloaded: 512,
            total: Some(1024),
            segments: Vec::new(),
        });
        cancel.cancelled().await;
        self.gate.acquire().await.unwrap().forget();
        Ok(Outcome::Paused)
    }
}

struct Setup {
    _dir: TempDir,
    manager: Manager,
    events: Receiver<JobEvent>,
    runs: Arc<AtomicUsize>,
    gate: Arc<Semaphore>,
    id: JobId,
}

/// One download, running.
async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let (runs, gate) = (Arc::new(AtomicUsize::new(0)), Arc::new(Semaphore::new(0)));
    let backend = Gated {
        runs: Arc::clone(&runs),
        gate: Arc::clone(&gate),
    };
    let store = Store::open_in_memory().await.unwrap();
    let manager = Manager::start(backend, store).await.unwrap();
    let events = manager.subscribe();
    let url = Url::parse("http://127.0.0.1:9/held.bin").unwrap();
    let id = manager
        .add(NewJob::new(url).in_dir(dir.path()))
        .await
        .unwrap();
    let mut s = Setup {
        _dir: dir,
        manager,
        events,
        runs,
        gate,
        id,
    };
    s.wait_for(JobState::Downloading).await;
    s
}

impl Setup {
    async fn wait_for(&mut self, state: JobState) {
        let (events, id) = (&mut self.events, self.id);
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if let JobEvent::Changed(job) = events.recv().await.unwrap()
                    && job.id == id
                    && job.state == state
                {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never became {state:?}"));
    }

    /// Lets the stopping download end; it must then start again.
    async fn release_and_see_it_start_again(&mut self) {
        self.gate.add_permits(1);
        self.wait_for(JobState::Queued).await;
        self.wait_for(JobState::Downloading).await;
        assert_eq!(self.runs.load(Ordering::SeqCst), 2);
        self.gate.add_permits(1);
        self.manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn a_start_time_cleared_while_stopping_starts_it_again() {
    let mut s = setup().await;
    let later = SystemTime::now() + Duration::from_secs(3600);
    let hold = JobChange {
        start_at: Some(Some(later)),
        ..JobChange::default()
    };
    s.manager.change_job(s.id, hold).await.unwrap();
    let clear = JobChange {
        start_at: Some(None),
        ..JobChange::default()
    };
    s.manager.change_job(s.id, clear).await.unwrap();
    s.release_and_see_it_start_again().await;
}

#[tokio::test]
async fn a_slot_removed_while_stopping_starts_it_again() {
    let mut s = setup().await;
    let midnight = Time::midnight();
    let stop = SlotSettings {
        downloads: Some(Downloads::Stop {
            let_finish: false,
            stay_paused: false,
        }),
        ..SlotSettings::daily(midnight, midnight)
    };
    let slot = s.manager.add_slot(stop).await.unwrap();
    s.manager.remove_slot(slot).await.unwrap();
    s.release_and_see_it_start_again().await;
}
