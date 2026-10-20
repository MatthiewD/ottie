#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::Duration;

use ottie_core::{
    Error, HttpBackend, HttpConfig, JobChange, JobEvent, JobId, JobState, Manager, NewJob, Reader,
    Segment, Store,
};
use support::{TestServer, test_data};
use tempfile::TempDir;
use tokio::sync::broadcast::Receiver;

const TIMEOUT: Duration = Duration::from_secs(20);
const READ: usize = 64 * 1024;

struct Setup {
    server: TestServer,
    dir: TempDir,
    manager: Manager,
    events: Receiver<JobEvent>,
    data: Vec<u8>,
}

/// `chunk_delay_ms` before each 16 KiB chunk keeps downloads running while
/// they're read.
async fn setup(size: usize, connections: usize, chunk_delay_ms: u64) -> Setup {
    let data = test_data(size, 3);
    let server = TestServer::start(data.clone()).await;
    server.configure(|b| b.chunk_delay = Some(Duration::from_millis(chunk_delay_ms)));
    // Streaming pieces of 4 × `min_split` (256 KiB) at the speeds used here.
    let config = HttpConfig {
        connections,
        min_split: 64 * 1024,
        piece_time: Duration::from_millis(100),
        ..HttpConfig::default()
    };
    let store = Store::open_in_memory().await.unwrap();
    let manager = Manager::start(HttpBackend::with_config(config).unwrap(), store)
        .await
        .unwrap();
    let events = manager.subscribe();
    Setup {
        server,
        dir: tempfile::tempdir().unwrap(),
        manager,
        events,
        data,
    }
}

impl Setup {
    /// Not in streaming mode, though it's a video: connections spread out.
    async fn add(&self) -> JobId {
        self.add_streaming(false).await
    }

    async fn add_streaming(&self, on: bool) -> JobId {
        let url = self.server.url("/files/movie.mp4");
        let job = NewJob::new(url).in_dir(self.dir.path()).streaming(on);
        self.manager.add(job).await.unwrap()
    }

    async fn wait_for(&mut self, id: JobId, state: JobState) {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if let JobEvent::Changed(job) = self.events.recv().await.unwrap()
                    && job.id == id
                    && job.state == state
                {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{id} never became {state:?}"));
    }
}

/// Reads from `offset` to the end of the file.
async fn read_rest(reader: &mut Reader, mut offset: u64) -> Vec<u8> {
    let mut all = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let bytes = reader.read(offset, READ).await.unwrap();
            if bytes.is_empty() {
                return;
            }
            offset += bytes.len() as u64;
            all.extend(bytes);
        }
    })
    .await
    .expect("reading took too long");
    all
}

#[tokio::test]
async fn reads_a_single_connection_download_as_it_arrives() {
    let s = setup(512 * 1024, 1, 10).await;
    let id = s.add().await;
    let (mut reader, opened) = Reader::open(&s.manager, id).await.unwrap();
    assert_eq!(opened.file_name, "movie.mp4");
    assert_eq!(opened.total, Some(512 * 1024));

    let first = reader.read(0, READ).await.unwrap();
    assert!(!first.is_empty());
    let job = s.manager.job(id).await.unwrap();
    assert_ne!(job.state, JobState::Completed, "read while downloading");

    // The reader keeps its file open while the download completes, which
    // renames it (Windows must allow that).
    let rest = read_rest(&mut reader, first.len() as u64).await;
    assert!([first, rest].concat() == s.data, "content differs");
    let job = s.manager.job(id).await.unwrap();
    assert_eq!(job.state, JobState::Completed);
}

#[tokio::test]
async fn reads_a_multi_connection_download_as_it_arrives() {
    let size = 2 * 1024 * 1024;
    let s = setup(size, 4, 100).await;
    let id = s.add().await;
    let (mut reader, _) = Reader::open(&s.manager, id).await.unwrap();

    // Each connection writes its own range: the start of the third one comes
    // long before the bytes before it.
    let middle = size as u64 / 2 + 123;
    let first = reader.read(middle, READ).await.unwrap();
    let job = s.manager.job(id).await.unwrap();
    assert_ne!(job.state, JobState::Completed, "read while downloading");
    let rest = read_rest(&mut reader, middle + first.len() as u64).await;
    assert!(
        [first, rest].concat() == s.data[size / 2 + 123..],
        "content differs"
    );

    let whole = read_rest(&mut reader, 0).await;
    assert!(whole == s.data, "content differs");
}

#[tokio::test]
async fn reads_a_finished_download() {
    let mut s = setup(256 * 1024, 1, 0).await;
    let id = s.add().await;
    s.wait_for(id, JobState::Completed).await;
    let (mut reader, opened) = Reader::open(&s.manager, id).await.unwrap();
    assert_eq!(opened.total, Some(256 * 1024));
    assert!(read_rest(&mut reader, 0).await == s.data, "content differs");
    assert!(reader.read(256 * 1024, READ).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_paused_download_keeps_the_reader_waiting() {
    let s = setup(512 * 1024, 1, 10).await;
    let id = s.add().await;
    let (mut reader, _) = Reader::open(&s.manager, id).await.unwrap();
    reader.read(0, READ).await.unwrap();
    s.manager.pause(id).await.unwrap();

    let reading = tokio::spawn(async move {
        let rest = read_rest(&mut reader, 0).await;
        (reader, rest)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!reading.is_finished(), "waits for the bytes to come");
    s.manager.resume(id).await.unwrap();
    let (_, all) = reading.await.unwrap();
    assert!(all == s.data, "content differs");
}

#[tokio::test]
async fn a_cancelled_download_stops_the_reader() {
    let s = setup(512 * 1024, 1, 20).await;
    let id = s.add().await;
    let (mut reader, _) = Reader::open(&s.manager, id).await.unwrap();
    let first = reader.read(0, READ).await.unwrap();
    s.manager.cancel(id).await.unwrap();
    let err = reader.read(first.len() as u64, READ).await.unwrap_err();
    assert!(matches!(err, Error::DownloadStopped(stopped) if stopped == id));
}

#[tokio::test]
async fn a_removed_download_stops_the_reader() {
    let s = setup(512 * 1024, 1, 20).await;
    let id = s.add().await;
    let (mut reader, _) = Reader::open(&s.manager, id).await.unwrap();
    let first = reader.read(0, READ).await.unwrap();
    let waiting = tokio::spawn(async move { reader.read(512 * 1024 - 1, READ).await });
    s.manager.remove(id).await.unwrap();
    let err = waiting.await.unwrap().unwrap_err();
    assert!(matches!(err, Error::JobNotFound(gone) if gone == id));
    assert!(!first.is_empty());
}

#[tokio::test]
async fn a_restart_the_reader_missed_still_stops_it() {
    let mut s = setup(512 * 1024, 1, 20).await;
    let id = s.add().await;
    let (mut reader, _) = Reader::open(&s.manager, id).await.unwrap();
    let first = reader.read(0, READ).await.unwrap();

    // Another version of the same size: the resume starts over.
    s.manager.pause(id).await.unwrap();
    s.server.configure(|b| {
        b.body = test_data(512 * 1024, 4).into();
        b.etag = Some("\"v2\"".to_owned());
    });
    s.manager.resume(id).await.unwrap();
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let JobEvent::Restarted { id: restarted, .. } = s.events.recv().await.unwrap()
                && restarted == id
            {
                return;
            }
        }
    })
    .await
    .expect("it never started over");
    // More news than the reader's queue holds: the restart is pushed out.
    for limit in 0..1100 {
        let change = JobChange {
            speed_limit: Some(1_000_000 + limit),
            ..JobChange::default()
        };
        s.manager.change_job(id, change).await.unwrap();
    }

    let err = reader.read(first.len() as u64, READ).await.unwrap_err();
    assert!(
        matches!(err, Error::FileChanged(changed) if changed == id),
        "{err:?}"
    );
}

impl Setup {
    /// Whether a request for a range starting at `start` reached the server.
    fn requested_from(&self, start: u64) -> bool {
        let wanted = format!("bytes={start}-");
        self.server
            .requests()
            .iter()
            .any(|r| r.range.as_deref().is_some_and(|r| r.starts_with(&wanted)))
    }
}

#[tokio::test]
async fn a_player_far_ahead_gets_a_connection_of_its_own() {
    // Four ranges of 1 MiB, each arriving at 80 KiB/s.
    let size = 4 * 1024 * 1024;
    let s = setup(size, 4, 200).await;
    let id = s.add().await;
    let (mut reader, _) = Reader::open(&s.manager, id).await.unwrap();

    // The second connection would need ~12 s to get here by itself.
    let seek = 1024 * 1024 + 960 * 1024;
    let started = std::time::Instant::now();
    let bytes = reader.read(seek, READ).await.unwrap();
    let waited = started.elapsed();
    assert!(waited < Duration::from_secs(5), "waited {waited:?}");
    assert!(s.requested_from(seek), "a connection went there");
    let seek = usize::try_from(seek).unwrap();
    assert!(bytes == s.data[seek..seek + bytes.len()], "content differs");
}

#[tokio::test]
async fn the_end_of_the_file_comes_early_while_someone_reads() {
    // Two ranges of 2 MiB; the player reads from the start.
    let size = 4 * 1024 * 1024;
    let s = setup(size, 2, 200).await;
    let id = s.add().await;
    let (mut reader, _) = Reader::open(&s.manager, id).await.unwrap();
    reader.read(0, READ).await.unwrap();

    let tail = size as u64 - 1024 * 1024;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !s.requested_from(tail) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the last MiB was never asked for");
}

#[tokio::test]
async fn following_players_keeps_the_file_intact() {
    let size = 2 * 1024 * 1024;
    let mut s = setup(size, 4, 10).await;
    let id = s.add().await;
    let (mut ahead, _) = Reader::open(&s.manager, id).await.unwrap();
    let (mut behind, _) = Reader::open(&s.manager, id).await.unwrap();

    // Two players far from every connection, each reading on to the end.
    let reading_ahead = read_rest(&mut ahead, 300 * 1024 + 7);
    let reading_behind = read_rest(&mut behind, 1300 * 1024 + 7);
    let (from_ahead, from_behind) = tokio::join!(reading_ahead, reading_behind);
    assert!(from_ahead == s.data[300 * 1024 + 7..], "content differs");
    assert!(from_behind == s.data[1300 * 1024 + 7..], "content differs");

    s.wait_for(id, JobState::Completed).await;
    let file = std::fs::read(s.dir.path().join("movie.mp4")).unwrap();
    assert!(file == s.data, "the finished file differs");
}

impl Setup {
    /// Where each range request that reached the server starts.
    fn requested_starts(&self) -> Vec<u64> {
        self.server
            .requests()
            .iter()
            .filter_map(|r| {
                let range = r.range.as_deref()?.strip_prefix("bytes=")?;
                range.split('-').next()?.parse().ok()
            })
            .collect()
    }
}

/// Bytes that have arrived before `tail` but past the first byte missing
/// from the start: what a player reading from the start can't play yet.
fn out_of_order(segments: &[Segment], tail: u64) -> u64 {
    let mut sorted = segments.to_vec();
    sorted.sort_by_key(|s| s.start);
    let playable = sorted
        .iter()
        .find(|s| !s.is_complete())
        .map_or(tail, |s| s.start + s.done);
    sorted
        .iter()
        .map(|s| {
            let (from, to) = (s.start.max(playable), (s.start + s.done).min(tail));
            to.saturating_sub(from)
        })
        .sum()
}

#[tokio::test]
async fn a_streaming_download_comes_in_order() {
    // Four connections at 800 KiB/s each.
    let size = 4 * 1024 * 1024;
    let mut s = setup(size, 4, 20).await;
    let id = s.add_streaming(true).await;
    let tail = size as u64 - 1024 * 1024;

    let mut most = 0;
    let events = &mut s.events;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            match events.recv().await.unwrap() {
                JobEvent::Progress {
                    id: of, segments, ..
                } if of == id => most = most.max(out_of_order(&segments, tail)),
                JobEvent::Changed(job) if job.id == id && job.state == JobState::Completed => {
                    return;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the download never completed");

    // Spread over four ranges of 1 MiB, it would be up to 2 MiB.
    assert!(most <= 1280 * 1024, "{most} bytes came out of order");
    let first_few = &s.requested_starts()[..4];
    assert!(
        first_few.contains(&tail),
        "the last MiB came late: {first_few:?}"
    );
    let file = std::fs::read(s.dir.path().join("movie.mp4")).unwrap();
    assert!(file == s.data, "the finished file differs");
}

#[tokio::test]
async fn switching_to_streaming_mode_brings_connections_to_the_front() {
    // Four ranges of 2 MiB at 320 KiB/s each: about 6 s as they are.
    let size = 8 * 1024 * 1024;
    let s = setup(size, 4, 50).await;
    let id = s.add().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let on = JobChange {
        streaming: Some(true),
        ..JobChange::default()
    };
    s.manager.change_job(id, on).await.unwrap();
    assert!(s.manager.job(id).await.unwrap().streaming);

    // Each range is cut to a piece; then its connection goes on from the
    // front, inside the first range.
    let front = 2 * 1024 * 1024;
    tokio::time::timeout(Duration::from_secs(4), async {
        while !s
            .requested_starts()
            .iter()
            .any(|&start| 0 < start && start < front)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("no connection came to the front");
}
