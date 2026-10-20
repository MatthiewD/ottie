#![allow(clippy::unwrap_used, clippy::expect_used)]

//! A download whose file fails as it stops (disk full, a drive gone): the
//! pause still happens, but says so, and keeps only what's on disk.

use std::time::Duration;

use ottie_core::{
    Backend, Download, Error, Event, JobEvent, JobId, JobSnapshot, JobState, Manager, NewJob,
    Outcome, PART_SUFFIX, Playback, Problem, Store, Throttle,
};
use reqwest::Url;
use tempfile::TempDir;
use tokio::sync::broadcast::Receiver;
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(10);
/// What really reached the disk; the download believes it has more.
const ON_DISK: usize = 300;

/// Writes `ON_DISK` bytes, says 512 arrived, then fails to flush the rest
/// when asked to stop.
struct FailingDisk;

impl Backend for FailingDisk {
    async fn run(
        &self,
        dl: &mut Download,
        _: &Throttle,
        _: &Playback,
        cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Outcome, Error> {
        dl.file_name = Some("flush.bin".to_owned());
        let part = dl.dest_dir.join(format!("flush.bin{PART_SUFFIX}"));
        std::fs::write(&part, vec![7; ON_DISK]).unwrap();
        on_event(Event::Started {
            file_name: "flush.bin".to_owned(),
            dest_dir: dl.dest_dir.clone(),
            total: Some(1024),
            validator: None,
            resumable: true,
            downloaded: 0,
            segments: Vec::new(),
        });
        dl.downloaded = 512;
        on_event(Event::Progress {
            downloaded: 512,
            total: Some(1024),
            segments: Vec::new(),
        });
        cancel.cancelled().await;
        Err(std::io::Error::other("no space left on the test disk").into())
    }
}

struct Setup {
    _dir: TempDir,
    manager: Manager,
    events: Receiver<JobEvent>,
    id: JobId,
}

async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().await.unwrap();
    let manager = Manager::start(FailingDisk, store).await.unwrap();
    let mut events = manager.subscribe();
    let url = Url::parse("http://127.0.0.1:9/flush.bin").unwrap();
    let id = manager
        .add(NewJob::new(url).in_dir(dir.path()))
        .await
        .unwrap();
    wait_for(&mut events, id, JobState::Downloading).await;
    Setup {
        _dir: dir,
        manager,
        events,
        id,
    }
}

async fn wait_for(events: &mut Receiver<JobEvent>, id: JobId, state: JobState) -> JobSnapshot {
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

/// Paused, with the disk's problem, and only what's on disk.
fn assert_paused_by_the_disk(job: &JobSnapshot) {
    assert_eq!(job.state, JobState::Paused);
    let Some(Problem::Disk { detail }) = &job.problem else {
        panic!("{:?}", job.problem);
    };
    assert!(detail.contains("no space left"), "{detail}");
    assert_eq!(job.downloaded, ON_DISK as u64);
}

#[tokio::test]
async fn a_pause_whose_file_fails_says_so() {
    let mut s = setup().await;
    let err = s.manager.pause(s.id).await.unwrap_err();
    assert!(matches!(err, Error::Io(_)), "{err:?}");
    let job = wait_for(&mut s.events, s.id, JobState::Paused).await;
    assert_paused_by_the_disk(&job);
    // Resuming is still possible: it's paused.
    s.manager.resume(s.id).await.unwrap();
}

#[tokio::test]
async fn a_shutdown_whose_file_fails_leaves_it_paused_with_the_problem() {
    let mut s = setup().await;
    s.manager.shutdown().await.unwrap();
    let job = wait_for(&mut s.events, s.id, JobState::Paused).await;
    assert_paused_by_the_disk(&job);
}
