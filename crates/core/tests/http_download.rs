#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::Duration;

use ottie_core::{
    Download, Error, Event, HttpConfig, Outcome, PART_SUFFIX, Placement, RestartReason, Throttle,
    build_client, run,
};
use reqwest::Client;
use support::{TestServer, test_data};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const SIZE: usize = 1024 * 1024;
const PAUSE_AT: u64 = 400 * 1024;

/// These tests cover the single-connection path; see `segmented.rs` for the rest.
fn single() -> HttpConfig {
    HttpConfig {
        connections: 1,
        ..HttpConfig::default()
    }
}

struct Setup {
    server: TestServer,
    dir: TempDir,
    client: Client,
    data: Vec<u8>,
}

async fn setup() -> Setup {
    let data = test_data(SIZE, 1);
    let server = TestServer::start(data.clone()).await;
    // Trickle the body so pausing lands mid-file instead of after it.
    server.configure(|b| b.chunk_delay = Some(Duration::from_millis(1)));
    Setup {
        server,
        dir: tempfile::tempdir().unwrap(),
        client: build_client().unwrap(),
        data,
    }
}

impl Setup {
    fn download(&self, path: &str) -> Download {
        Download::new(self.server.url(path), self.dir.path()).unwrap()
    }
}

/// Runs until done (or until `pause_at` bytes, if given), collecting events.
async fn run_collect(
    client: &Client,
    dl: &mut Download,
    pause_at: Option<u64>,
) -> (Result<Outcome, Error>, Vec<Event>) {
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let result = run(
        client,
        &single(),
        dl,
        &Throttle::unlimited(),
        &cancel,
        |event| {
            if let (Some(limit), Event::Progress { downloaded, .. }) = (pause_at, &event)
                && *downloaded >= limit
            {
                cancel.cancel();
            }
            events.push(event);
        },
    )
    .await;
    (result, events)
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

/// Pauses partway and checks the `.part.ottie` file matches what `dl` claims.
async fn pause_midway(s: &Setup, dl: &mut Download) {
    let (result, _) = run_collect(&s.client, dl, Some(PAUSE_AT)).await;
    assert_eq!(result.unwrap(), Outcome::Paused);
    assert!(dl.downloaded >= PAUSE_AT && dl.downloaded < SIZE as u64);
    let part_len = std::fs::metadata(dl.part_path().unwrap()).unwrap().len();
    assert_eq!(part_len, dl.downloaded);
}

fn assert_completed_with(outcome: Outcome, expected: &[u8]) {
    let Outcome::Completed { path } = outcome else {
        panic!("expected completion, got {outcome:?}");
    };
    // Byte-for-byte comparison: stricter than comparing hashes.
    assert!(std::fs::read(&path).unwrap() == expected, "content differs");
    assert!(
        !path
            .with_file_name(format!(
                "{}{PART_SUFFIX}",
                path.file_name().unwrap().display()
            ))
            .exists()
    );
}

#[tokio::test]
async fn downloads_whole_file() {
    let s = setup().await;
    let mut dl = s.download("/files/data.bin");
    let (result, events) = run_collect(&s.client, &mut dl, None).await;
    let outcome = result.unwrap();
    assert_eq!(
        outcome,
        Outcome::Completed {
            path: s.dir.path().join("data.bin")
        }
    );
    assert_completed_with(outcome, &s.data);
    assert_eq!(dl.total, Some(SIZE as u64));
    assert!(dl.resumable);
    assert_eq!(
        events.last(),
        Some(&Event::Progress {
            downloaded: SIZE as u64,
            total: Some(SIZE as u64),
            segments: Vec::new(),
        })
    );
}

/// Windows warns before running it, as for a browser's download
/// (docs/decisions/0045).
#[cfg(windows)]
#[tokio::test]
async fn a_finished_file_carries_the_mark_of_the_web() {
    let s = setup().await;
    let mut dl = s.download("/files/setup.exe?key=secret");
    let (result, _) = run_collect(&s.client, &mut dl, None).await;
    let Outcome::Completed { path } = result.unwrap() else {
        panic!("not completed");
    };
    let mut stream = path.into_os_string();
    stream.push(":Zone.Identifier");
    let mark = std::fs::read_to_string(stream).unwrap();
    assert!(mark.contains("ZoneId=3"), "{mark}");
    assert!(
        mark.contains(&format!("HostUrl={}", s.server.url("/files/setup.exe"))),
        "{mark}"
    );
    assert!(!mark.contains("secret"), "{mark}");
}

#[tokio::test]
async fn pause_then_resume_is_byte_identical() {
    let s = setup().await;
    let mut dl = s.download("/files/data.bin");
    pause_midway(&s, &mut dl).await;
    let offset = dl.downloaded;

    let (result, events) = run_collect(&s.client, &mut dl, None).await;
    assert_completed_with(result.unwrap(), &s.data);
    assert!(restarts(&events).is_empty());

    let resume = &s.server.requests()[1];
    assert_eq!(
        resume.range.as_deref(),
        Some(format!("bytes={offset}-").as_str())
    );
    assert_eq!(resume.if_range.as_deref(), Some("\"v1\""));
}

#[tokio::test]
async fn resume_restarts_when_remote_file_changed() {
    let s = setup().await;
    let mut dl = s.download("/files/data.bin");
    pause_midway(&s, &mut dl).await;

    let new_data = test_data(SIZE + 1000, 2);
    s.server.configure(|b| {
        b.body = new_data.clone().into();
        b.etag = Some("\"v2\"".to_owned());
    });
    let (result, events) = run_collect(&s.client, &mut dl, None).await;
    assert_completed_with(result.unwrap(), &new_data);
    assert_eq!(restarts(&events), [RestartReason::RemoteChanged]);
}

#[tokio::test]
async fn resume_restarts_when_server_ignores_ranges() {
    let s = setup().await;
    let mut dl = s.download("/files/data.bin");
    pause_midway(&s, &mut dl).await;

    s.server.configure(|b| b.ranges = false);
    let (result, events) = run_collect(&s.client, &mut dl, None).await;
    assert_completed_with(result.unwrap(), &s.data);
    assert_eq!(restarts(&events), [RestartReason::RangesIgnored]);
    assert!(!dl.resumable, "server now says Accept-Ranges: none");
}

#[tokio::test]
async fn without_validator_resume_starts_over() {
    let s = setup().await;
    s.server.configure(|b| b.etag = None);
    let mut dl = s.download("/files/data.bin");
    pause_midway(&s, &mut dl).await;
    assert!(!dl.resumable);

    let (result, events) = run_collect(&s.client, &mut dl, None).await;
    assert_completed_with(result.unwrap(), &s.data);
    assert_eq!(restarts(&events), [RestartReason::NotResumable]);
    let restart = &s.server.requests()[1];
    assert_eq!(restart.range.as_deref(), Some("bytes=0-"));
    assert_eq!(restart.if_range, None);
}

#[tokio::test]
async fn dropped_connection_can_be_resumed() {
    let s = setup().await;
    s.server.configure(|b| b.drop_after = Some(300_000));
    let mut dl = s.download("/files/data.bin");

    let (result, _) = run_collect(&s.client, &mut dl, None).await;
    assert!(matches!(result, Err(Error::Http(_))), "got {result:?}");
    // It says what went wrong under reqwest's label.
    let message = result.unwrap_err().to_string();
    assert!(
        message.starts_with("HTTP request failed: error decoding response body: "),
        "{message}"
    );
    // Bytes in flight when the connection died are lost, so expect up to 300 000.
    let offset = dl.downloaded;
    assert!(offset > 0 && offset <= 300_000, "downloaded {offset}");
    let part_len = std::fs::metadata(dl.part_path().unwrap()).unwrap().len();
    assert_eq!(part_len, offset);

    let (result, events) = run_collect(&s.client, &mut dl, None).await;
    assert_completed_with(result.unwrap(), &s.data);
    assert!(restarts(&events).is_empty());
    let resume = &s.server.requests()[1];
    assert_eq!(
        resume.range.as_deref(),
        Some(format!("bytes={offset}-").as_str())
    );
}

#[tokio::test]
async fn deleted_part_file_restarts() {
    let s = setup().await;
    let mut dl = s.download("/files/data.bin");
    pause_midway(&s, &mut dl).await;
    std::fs::remove_file(dl.part_path().unwrap()).unwrap();

    let (result, events) = run_collect(&s.client, &mut dl, None).await;
    assert_completed_with(result.unwrap(), &s.data);
    assert_eq!(restarts(&events), [RestartReason::PartFileMissing]);
}

#[tokio::test]
async fn pause_is_prompt_on_a_slow_server() {
    let s = setup().await;
    s.server
        .configure(|b| b.chunk_delay = Some(Duration::from_secs(30)));
    let mut dl = s.download("/files/data.bin");
    let cancel = CancellationToken::new();
    let pauser = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        pauser.cancel();
    });

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run(
            &s.client,
            &single(),
            &mut dl,
            &Throttle::unlimited(),
            &cancel,
            |_| {},
        ),
    )
    .await
    .expect("pause took too long");
    assert_eq!(result.unwrap(), Outcome::Paused);
}

/// Runs while the server never answers the request, and pauses 200 ms in:
/// the run must stop at once, not after the stall timeout (60 s).
async fn pause_before_the_answer(s: &Setup, dl: &mut Download) {
    s.server.configure(|b| b.silent_next = 1);
    let cancel = CancellationToken::new();
    let pauser = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        pauser.cancel();
    });
    let throttle = Throttle::unlimited();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run(&s.client, &single(), dl, &throttle, &cancel, |_| {}),
    )
    .await
    .expect("the pause waited for the server");
    assert_eq!(result.unwrap(), Outcome::Paused);
}

#[tokio::test]
async fn pause_is_prompt_before_the_server_answers() {
    let s = setup().await;
    let mut dl = s.download("/files/data.bin");
    pause_before_the_answer(&s, &mut dl).await;
    assert_eq!(dl.file_name, None, "named before any answer");

    let (result, _) = run_collect(&s.client, &mut dl, None).await;
    assert_completed_with(result.unwrap(), &s.data);
}

#[tokio::test]
async fn pause_is_prompt_while_a_resume_waits_for_the_server() {
    let s = setup().await;
    let mut dl = s.download("/files/data.bin");
    pause_midway(&s, &mut dl).await;
    let before = dl.downloaded;
    pause_before_the_answer(&s, &mut dl).await;
    assert_eq!(dl.downloaded, before);
    let part_len = std::fs::metadata(dl.part_path().unwrap()).unwrap().len();
    assert_eq!(part_len, before);

    let (result, events) = run_collect(&s.client, &mut dl, None).await;
    assert_completed_with(result.unwrap(), &s.data);
    assert_eq!(restarts(&events), []);
}

#[tokio::test]
async fn existing_file_gets_a_numbered_name() {
    let s = setup().await;
    std::fs::write(s.dir.path().join("data.bin"), b"mine").unwrap();
    let mut dl = s.download("/files/data.bin");

    let (result, _) = run_collect(&s.client, &mut dl, None).await;
    let outcome = result.unwrap();
    assert_eq!(
        outcome,
        Outcome::Completed {
            path: s.dir.path().join("data (1).bin")
        }
    );
    assert_completed_with(outcome, &s.data);
    assert_eq!(
        std::fs::read(s.dir.path().join("data.bin")).unwrap(),
        b"mine"
    );
}

#[tokio::test]
async fn redirect_to_another_scheme_is_rejected() {
    let s = setup().await;
    let mut dl = s.download("/redirect-to-ftp");
    let (result, _) = run_collect(&s.client, &mut dl, None).await;
    assert!(
        matches!(&result, Err(Error::Http(e)) if e.is_redirect()),
        "got {result:?}"
    );
    assert_eq!(std::fs::read_dir(s.dir.path()).unwrap().count(), 0);
}

#[test]
fn only_http_urls_are_accepted() {
    let url = reqwest::Url::parse("file:///etc/passwd").unwrap();
    assert!(matches!(
        Download::new(url, "."),
        Err(Error::UnsupportedScheme(s)) if s == "file"
    ));
}

#[tokio::test]
async fn content_disposition_names_the_file_across_a_pause() {
    let s = setup().await;
    let header = "attachment; filename=\"plain.pdf\"; filename*=UTF-8''caf%C3%A9%20menu.pdf";
    s.server
        .configure(|b| b.content_disposition = Some(header.as_bytes().to_vec()));
    let mut dl = s.download("/files/get");
    pause_midway(&s, &mut dl).await;
    assert_eq!(dl.file_name.as_deref(), Some("café menu.pdf"));

    let (result, _) = run_collect(&s.client, &mut dl, None).await;
    let outcome = result.unwrap();
    assert_eq!(
        outcome,
        Outcome::Completed {
            path: s.dir.path().join("café menu.pdf")
        }
    );
    assert_completed_with(outcome, &s.data);
}

#[tokio::test]
async fn a_placement_picks_the_folder_by_the_real_name() {
    let s = setup().await;
    let header = b"attachment; filename=\"report.PDF\"".to_vec();
    s.server.configure(|b| b.content_disposition = Some(header));
    let docs = s.dir.path().join("docs");
    let mut dl = s.download("/files/download.php");
    dl.placement = Some(Placement {
        by_extension: [("pdf".to_owned(), docs.clone())].into(),
        otherwise: s.dir.path().join("other"),
    });

    let (result, events) = run_collect(&s.client, &mut dl, None).await;
    assert_completed_with(result.unwrap(), &s.data);
    assert!(docs.join("report.PDF").is_file());
    let started_in = events.iter().find_map(|e| match e {
        Event::Started { dest_dir, .. } => Some(dest_dir.clone()),
        _ => None,
    });
    assert_eq!(started_in, Some(docs));
    assert_eq!(dl.placement, None, "used once");
}

#[tokio::test]
async fn a_name_without_extension_stays_put() {
    let s = setup().await;
    let mut dl = s.download("/files/README");
    dl.placement = Some(Placement {
        by_extension: Default::default(),
        otherwise: s.dir.path().join("other"),
    });
    let (result, _) = run_collect(&s.client, &mut dl, None).await;
    assert_completed_with(result.unwrap(), &s.data);
    assert!(s.dir.path().join("README").is_file());
}
