#![allow(clippy::unwrap_used, clippy::expect_used)]

//! A download's task that panics (a bug): the job fails with a reason, and
//! pause, cancel and shutdown still get their answer.

use std::time::Duration;

use ottie_core::{
    Backend, Download, Error, Event, JobEvent, JobId, JobSnapshot, JobState, Manager, NewJob,
    Outcome, Playback, Problem, Store, Throttle,
};
use reqwest::Url;
use tempfile::TempDir;
use tokio::sync::broadcast::Receiver;
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Announces the download, then panics: at once, or once asked to stop.
struct Crashing {
    at_once: bool,
}

impl Backend for Crashing {
    async fn run(
        &self,
        dl: &mut Download,
        _: &Throttle,
        _: &Playback,
        cancel: &CancellationToken,
        on_event: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Outcome, Error> {
        on_event(Event::Started {
            file_name: "crash.bin".to_owned(),
            dest_dir: dl.dest_dir.clone(),
            total: Some(1024),
            validator: None,
            resumable: false,
            downloaded: 0,
            segments: Vec::new(),
        });
        // Bytes arriving make it `Downloading`.
        on_event(Event::Progress {
            downloaded: 512,
            total: Some(1024),
            segments: Vec::new(),
        });
        if !self.at_once {
            cancel.cancelled().await;
        }
        panic!("the test backend crashes");
    }
}

struct Setup {
    _dir: TempDir,
    manager: Manager,
    events: Receiver<JobEvent>,
    id: JobId,
}

async fn setup(at_once: bool) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().await.unwrap();
    let manager = Manager::start(Crashing { at_once }, store).await.unwrap();
    let mut events = manager.subscribe();
    let url = Url::parse("http://127.0.0.1:9/crash.bin").unwrap();
    let id = manager
        .add(NewJob::new(url).in_dir(dir.path()))
        .await
        .unwrap();
    if !at_once {
        wait_for(&mut events, id, JobState::Downloading).await;
    }
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

/// Answers within the timeout, or the test fails.
async fn answers<T>(what: &str, call: impl Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, call)
        .await
        .unwrap_or_else(|_| panic!("{what} never answered"))
}

#[tokio::test]
async fn a_crashed_download_fails_with_a_reason() {
    let mut s = setup(true).await;
    let job = wait_for(&mut s.events, s.id, JobState::Failed).await;
    let Some(Problem::Other { detail }) = &job.problem else {
        panic!("{:?}", job.problem);
    };
    assert!(detail.contains("bug"), "{detail}");
    assert!(detail.contains("the test backend crashes"), "{detail}");
    answers("shutdown", s.manager.shutdown()).await.unwrap();
}

#[tokio::test]
async fn pausing_a_download_that_crashes_still_answers() {
    let s = setup(false).await;
    answers("pause", s.manager.pause(s.id)).await.unwrap();
    answers("shutdown", s.manager.shutdown()).await.unwrap();
}

#[tokio::test]
async fn cancelling_a_download_that_crashes_still_answers() {
    let s = setup(false).await;
    answers("cancel", s.manager.cancel(s.id)).await.unwrap();
    answers("shutdown", s.manager.shutdown()).await.unwrap();
}

#[tokio::test]
async fn shutdown_answers_while_a_download_crashes() {
    let s = setup(false).await;
    answers("shutdown", s.manager.shutdown()).await.unwrap();
}
