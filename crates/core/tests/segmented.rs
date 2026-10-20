#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::HashSet;
use std::time::Duration;

use axum::http::StatusCode;
use ottie_core::{
    Download, Error, Event, HttpConfig, Outcome, PART_SUFFIX, RestartReason, Segment, Throttle,
    build_client, run,
};
use reqwest::Client;
use support::{TestServer, test_data};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const SIZE: usize = 2 * 1024 * 1024;
const CONFIG: HttpConfig = HttpConfig {
    connections: 4,
    min_split: 64 * 1024,
    stall_timeout: Duration::from_secs(60),
    piece_time: Duration::from_secs(2),
    starved_after: Duration::from_secs(20),
};

struct Setup {
    server: TestServer,
    dir: TempDir,
    client: Client,
    data: Vec<u8>,
}

async fn setup(size: usize, chunk_delay_ms: u64) -> Setup {
    let data = test_data(size, 7);
    let server = TestServer::start(data.clone()).await;
    server.configure(|b| b.chunk_delay = Some(Duration::from_millis(chunk_delay_ms)));
    Setup {
        server,
        dir: tempfile::tempdir().unwrap(),
        client: build_client().unwrap(),
        data,
    }
}

impl Setup {
    fn download(&self) -> Download {
        Download::new(self.server.url("/files/big.bin"), self.dir.path()).unwrap()
    }

    /// Runs until done, or until `pause_at` bytes when given, collecting events.
    /// `on_started` runs once, when the download announces itself.
    async fn run(
        &self,
        dl: &mut Download,
        pause_at: Option<u64>,
        mut on_started: impl FnMut(),
    ) -> (Result<Outcome, Error>, Vec<Event>) {
        let cancel = CancellationToken::new();
        let mut events = Vec::new();
        let throttle = Throttle::unlimited();
        let result = run(&self.client, &CONFIG, dl, &throttle, &cancel, |event| {
            match &event {
                Event::Started { .. } => on_started(),
                Event::Progress { downloaded, .. }
                    if pause_at.is_some_and(|p| *downloaded >= p) =>
                {
                    cancel.cancel();
                }
                _ => {}
            }
            events.push(event);
        })
        .await;
        (result, events)
    }

    fn assert_complete(&self, outcome: Outcome, expected: &[u8]) {
        let Outcome::Completed { path } = outcome else {
            panic!("expected completion, got {outcome:?}");
        };
        assert!(std::fs::read(&path).unwrap() == expected, "content differs");
        let part = self.dir.path().join(format!("big.bin{PART_SUFFIX}"));
        assert!(!part.exists());
    }

    /// What the saved ranges say has arrived.
    fn part_ranges_done(&self, dl: &Download) -> u64 {
        dl.segments.iter().map(|segment| segment.done).sum()
    }

    fn part_len(&self) -> u64 {
        let part = self.dir.path().join(format!("big.bin{PART_SUFFIX}"));
        std::fs::metadata(part).unwrap().len()
    }
}

fn restarts(events: &[Event]) -> Vec<RestartReason> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Restarted(reason) => Some(*reason),
            _ => None,
        })
        .collect()
}

fn start_of(range: &str) -> u64 {
    let spec = range.strip_prefix("bytes=").unwrap();
    spec.split_once('-').unwrap().0.parse().unwrap()
}

#[tokio::test]
async fn downloads_over_several_connections() {
    let s = setup(SIZE, 1).await;
    let mut dl = s.download();
    let (result, events) = s.run(&mut dl, None, || {}).await;
    s.assert_complete(result.unwrap(), &s.data);
    assert!(dl.segments.is_empty(), "segments are dropped once complete");

    let planned = events.iter().find_map(|e| match e {
        Event::Started { segments, .. } => Some(segments.clone()),
        _ => None,
    });
    assert_eq!(planned.unwrap().len(), 4);

    let requests = s.server.requests();
    // The first request discovers the file and becomes the first range.
    assert_eq!(requests[0].range.as_deref(), Some("bytes=0-"));
    assert_eq!(requests[0].if_range, None);
    assert!(requests.len() >= 4, "only {} requests", requests.len());
    for extra in &requests[1..] {
        let range = extra.range.as_deref().unwrap();
        assert!(!range.ends_with('-'), "extra ranges are closed: {range}");
        assert_eq!(extra.if_range.as_deref(), Some("\"v1\""));
    }
}

#[tokio::test]
async fn pause_and_resume_is_byte_identical() {
    let s = setup(SIZE, 2).await;
    let mut dl = s.download();
    let (result, _) = s.run(&mut dl, Some(SIZE as u64 * 2 / 5), || {}).await;
    assert_eq!(result.unwrap(), Outcome::Paused);

    let unfinished: Vec<Segment> = dl
        .segments
        .iter()
        .copied()
        .filter(|s| !s.is_complete())
        .collect();
    assert!(!unfinished.is_empty());
    assert_eq!(
        dl.downloaded,
        dl.segments.iter().map(|s| s.done).sum::<u64>()
    );
    assert_eq!(
        s.part_len(),
        SIZE as u64,
        "the file has its full size from the start"
    );

    let before = s.server.requests().len();
    let (result, events) = s.run(&mut dl, None, || {}).await;
    s.assert_complete(result.unwrap(), &s.data);
    assert!(restarts(&events).is_empty());

    // Each unfinished range continued exactly where its flushed data ends.
    let resumed: HashSet<u64> = s.server.requests()[before..]
        .iter()
        .map(|r| start_of(r.range.as_deref().unwrap()))
        .collect();
    for segment in unfinished {
        assert!(
            resumed.contains(&(segment.start + segment.done)),
            "{segment:?}"
        );
    }
}

#[tokio::test]
async fn a_failed_restart_never_completes_a_file_with_holes() {
    let s = setup(SIZE, 2).await;
    let mut dl = s.download();
    let (result, _) = s.run(&mut dl, Some(SIZE as u64 * 2 / 5), || {}).await;
    assert_eq!(result.unwrap(), Outcome::Paused);
    assert_eq!(s.part_len(), SIZE as u64);

    // The same file, but the server ignores ranges now (a restart), and the
    // restart's own request fails.
    s.server.configure(|b| {
        b.ranges = false;
        b.fail_from_start = Some((StatusCode::SERVICE_UNAVAILABLE, 1));
    });
    let (result, events) = s.run(&mut dl, None, || {}).await;
    assert!(result.is_err(), "{result:?}");
    assert_eq!(restarts(&events), [RestartReason::RangesIgnored]);
    assert!(dl.segments.is_empty());
    assert_eq!(s.part_len(), 0, "the file is as empty as its map");

    // Ranges work again: the next try starts from zero, where a resume at
    // the old full length would have got a 416 and called it complete.
    s.server.configure(|b| b.ranges = true);
    let (result, _) = s.run(&mut dl, None, || {}).await;
    s.assert_complete(result.unwrap(), &s.data);
}

#[tokio::test]
async fn a_paused_download_resumes_after_its_file_moved() {
    // What moving a download to another category's folder does.
    let s = setup(SIZE, 2).await;
    let mut dl = s.download();
    let (result, _) = s.run(&mut dl, Some(SIZE as u64 / 3), || {}).await;
    assert_eq!(result.unwrap(), Outcome::Paused);
    let elsewhere = s.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let name = format!("big.bin{PART_SUFFIX}");
    std::fs::rename(s.dir.path().join(&name), elsewhere.join(&name)).unwrap();
    dl.dest_dir = elsewhere.clone();

    let (result, events) = s.run(&mut dl, None, || {}).await;
    let Outcome::Completed { path } = result.unwrap() else {
        panic!("not completed");
    };
    assert_eq!(path, elsewhere.join("big.bin"));
    assert!(std::fs::read(&path).unwrap() == s.data, "content differs");
    assert!(restarts(&events).is_empty());
}

#[tokio::test]
async fn idle_connections_take_over_a_slow_range() {
    let s = setup(SIZE, 0).await;
    // The last quarter of the file is slow; the other connections finish
    // early and should split it between them.
    s.server
        .configure(|b| b.slow_from = Some((SIZE * 3 / 4, Duration::from_millis(15))));
    let mut dl = s.download();
    let (result, _) = s.run(&mut dl, None, || {}).await;
    s.assert_complete(result.unwrap(), &s.data);
    let requests = s.server.requests().len();
    assert!(requests > 4, "expected splits, got {requests} requests");
}

#[tokio::test]
async fn file_changing_mid_download_restarts_cleanly() {
    let s = setup(SIZE, 1).await;
    let new_data = test_data(SIZE + 4096, 8);
    let mut changed = false;
    let mut dl = s.download();
    // Change the file right after the first response, before the other
    // connections ask for their ranges.
    let (result, events) = s
        .run(&mut dl, None, || {
            if !changed {
                changed = true;
                s.server.configure(|b| {
                    b.body = new_data.clone().into();
                    b.etag = Some("\"v2\"".to_owned());
                });
            }
        })
        .await;
    s.assert_complete(result.unwrap(), &new_data);
    assert_eq!(restarts(&events), [RestartReason::RemoteChanged]);
}

#[tokio::test]
async fn a_dropped_connection_doesnt_stop_the_others() {
    let s = setup(SIZE, 1).await;
    // Kills the first request (the range starting at 0) partway.
    s.server.configure(|b| b.drop_after = Some(100_000));
    let mut dl = s.download();
    let (result, events) = s.run(&mut dl, None, || {}).await;
    s.assert_complete(result.unwrap(), &s.data);
    assert!(restarts(&events).is_empty());
    // What the dropped connection didn't get was asked for again.
    let again = s.server.requests()[1..].iter().any(|request| {
        let from = request
            .range
            .as_deref()
            .and_then(|r| r.strip_prefix("bytes="));
        let from: u64 = from.unwrap().split('-').next().unwrap().parse().unwrap();
        from <= 100_000
    });
    assert!(again, "{:?}", s.server.requests());
}

#[tokio::test]
async fn when_every_connection_fails_the_download_fails_and_resumes() {
    let s = setup(SIZE, 1).await;
    let mut dl = s.download();
    // The first answer comes; every request after it is refused.
    let refuse = || {
        s.server
            .configure(|b| b.fail_next = Some((StatusCode::SERVICE_UNAVAILABLE, usize::MAX)));
    };
    let (result, _) = s.run(&mut dl, None, refuse).await;
    assert!(
        matches!(result, Err(Error::Status { .. })),
        "got {result:?}"
    );
    assert_eq!(dl.downloaded, s.part_ranges_done(&dl));

    s.server.configure(|b| b.fail_next = None);
    let (result, events) = s.run(&mut dl, None, || {}).await;
    s.assert_complete(result.unwrap(), &s.data);
    assert!(restarts(&events).is_empty());
}

#[tokio::test]
async fn a_damaged_part_file_restarts() {
    let s = setup(SIZE, 2).await;
    let mut dl = s.download();
    let (result, _) = s.run(&mut dl, Some(SIZE as u64 / 3), || {}).await;
    assert_eq!(result.unwrap(), Outcome::Paused);
    let part = s.dir.path().join(format!("big.bin{PART_SUFFIX}"));
    std::fs::OpenOptions::new()
        .write(true)
        .open(&part)
        .unwrap()
        .set_len(1000)
        .unwrap();

    let (result, events) = s.run(&mut dl, None, || {}).await;
    s.assert_complete(result.unwrap(), &s.data);
    assert_eq!(restarts(&events), [RestartReason::PartFileDamaged]);
}

#[tokio::test]
async fn one_connection_when_splitting_is_unsafe_or_pointless() {
    // No validator: we couldn't tell if the file changed between ranges.
    let s = setup(SIZE, 0).await;
    s.server.configure(|b| b.etag = None);
    let mut dl = s.download();
    s.assert_complete(s.run(&mut dl, None, || {}).await.0.unwrap(), &s.data);
    assert_eq!(s.server.requests().len(), 1);

    // No range support.
    let s = setup(SIZE, 0).await;
    s.server.configure(|b| b.ranges = false);
    let mut dl = s.download();
    s.assert_complete(s.run(&mut dl, None, || {}).await.0.unwrap(), &s.data);
    assert_eq!(s.server.requests().len(), 1);

    // Too small to be worth it.
    let s = setup(100 * 1024, 0).await;
    let mut dl = s.download();
    s.assert_complete(s.run(&mut dl, None, || {}).await.0.unwrap(), &s.data);
    assert_eq!(s.server.requests().len(), 1);
}

#[tokio::test]
async fn progress_shows_each_range_as_it_fills() {
    let s = setup(SIZE, 2).await;
    let mut dl = s.download();
    let (result, events) = s.run(&mut dl, None, || {}).await;
    s.assert_complete(result.unwrap(), &s.data);

    let mut with_ranges = 0;
    for event in &events {
        let Event::Progress {
            downloaded,
            segments,
            ..
        } = event
        else {
            continue;
        };
        if segments.is_empty() {
            continue;
        }
        with_ranges += 1;
        // What the ranges say has arrived is what the download counts…
        let received: u64 = segments.iter().map(|segment| segment.done).sum();
        assert_eq!(received, *downloaded);
        // …and, side by side, they cover the whole file once.
        let mut sorted = segments.clone();
        sorted.sort_by_key(|segment| segment.start);
        assert_eq!(sorted[0].start, 0);
        for pair in sorted.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        assert_eq!(sorted.last().unwrap().end, SIZE as u64);
        assert!(sorted.iter().all(|s| s.done <= s.end - s.start));
    }
    assert!(with_ranges > 0, "progress came with the ranges");
}
