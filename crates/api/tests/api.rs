#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The API end to end: a real server on 127.0.0.1, called the way clients
//! will, next to a small file server.

use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::routing::get;
use futures_util::StreamExt;
use ottie_api::{Access, AccessConfig, AccessInfo, AddDevice, AddedDevice, AllowedFolders};
use ottie_api::{
    AddJob, AddJobs, Added, AddedCategory, AddedJobs, AddedView, AddedWithin, ApiError, Appearance,
    AppearanceMode, Arrangement, BuiltIn, Category, CategoryBody, EVENTS_PROTOCOL, Event, Existing,
    FoundLink, Job, JobChange, JobState, Matched, Palette, Preview, PreviewLinks, Priority,
    Problem, Reason, Reorder, ReorderJobs, Settings, SettingsChange, SetupStep, SortDirection,
    SortKey, SpeedHistory, StreamLink, TOKEN_PROTOCOL_PREFIX, Token, UiSource, View, ViewBody,
    ViewFilters,
};
use ottie_api::{AddedSlot, Day, ReorderSlots, Slot, SlotDownloads, SlotList};
use ottie_api::{Device, FolderList};
use ottie_api::{TrustCertificate, TrustedCertificate};
use ottie_core::{Manager, NativeBackend, Store};
use reqwest::{RequestBuilder, StatusCode};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "0123456789abcdef";
const SIZE: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

struct Daemon {
    /// `127.0.0.1:port` of the API.
    addr: String,
    /// Base URL of the file server.
    files: String,
    client: reqwest::Client,
    dir: TempDir,
    _shutdown: CancellationToken,
}

/// `/file.bin` arrives at once; `/slow.bin` takes about 4 s.
async fn start() -> Daemon {
    start_with(UiSource::Embedded).await
}

async fn start_with(ui: UiSource) -> Daemon {
    let access = Access::open(AccessConfig::new(Token::new(TOKEN))).unwrap();
    start_with_access(ui, access).await
}

async fn start_with_access(ui: UiSource, access: Access) -> Daemon {
    let files = Router::new()
        .route("/file.bin", get(|| async { vec![7u8; SIZE] }))
        .route("/slow.bin", get(|| async { slow_body() }));
    let files = serve(files).await;

    let store = Store::open_in_memory().await.unwrap();
    let manager = Manager::start(NativeBackend::new().unwrap(), store)
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let api = ottie_api::router(manager, access, shutdown.clone(), ui);
    Daemon {
        addr: serve(api).await,
        files: format!("http://{files}"),
        client: reqwest::Client::new(),
        dir: tempfile::tempdir().unwrap(),
        _shutdown: shutdown,
    }
}

async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn slow_body() -> Body {
    let chunks = futures_util::stream::unfold(0, |sent| async move {
        if sent == 200 {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        Some((
            Ok::<_, std::io::Error>(Bytes::from(vec![1u8; 1024])),
            sent + 1,
        ))
    });
    Body::from_stream(chunks)
}

impl Daemon {
    fn url(&self, path: &str) -> String {
        format!("http://{}/api/v1{path}", self.addr)
    }

    fn get(&self, path: &str) -> RequestBuilder {
        self.client.get(self.url(path)).bearer_auth(TOKEN)
    }

    fn post(&self, path: &str) -> RequestBuilder {
        self.client.post(self.url(path)).bearer_auth(TOKEN)
    }

    fn new_job(&self, file: &str) -> AddJob {
        AddJob {
            url: format!("{}/{file}", self.files),
            dest_dir: Some(self.dir.path().to_string_lossy().into_owned()),
            category: None,
            streaming: None,
            start_at: None,
            stop_at: None,
            existing: None,
            cookie: None,
            referrer: None,
            user_agent: None,
        }
    }

    async fn add(&self, file: &str) -> String {
        let response = self.post("/jobs").json(&self.new_job(file)).send().await;
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        response.json::<Added>().await.unwrap().id
    }

    async fn job(&self, id: &str) -> Job {
        let response = self.get(&format!("/jobs/{id}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn wait_until(&self, id: &str, state: JobState) {
        tokio::time::timeout(TIMEOUT, async {
            while self.job(id).await.state != state {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("job {id} never became {state:?}"));
    }

    /// Opens the events WebSocket, offering `protocols`.
    async fn events(
        &self,
        protocols: &str,
    ) -> Result<
        (
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tungstenite::handshake::client::Response,
        ),
        tungstenite::Error,
    > {
        let mut request = format!("ws://{}/api/v1/events", self.addr)
            .into_client_request()
            .unwrap();
        if !protocols.is_empty() {
            let value = protocols.parse().unwrap();
            request
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", value);
        }
        tokio_tungstenite::connect_async(request).await
    }
}

impl Daemon {
    /// The job's play link, as a full URL.
    async fn play_link(&self, id: &str) -> String {
        let response = self
            .post(&format!("/jobs/{id}/stream"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let link: StreamLink = response.json().await.unwrap();
        assert!(link.url.starts_with("/api/v1/stream/"), "{}", link.url);
        format!("http://{}{}", self.addr, link.url)
    }
}

async fn error_of(response: reqwest::Response) -> (StatusCode, ApiError) {
    let status = response.status();
    (status, response.json::<ApiError>().await.unwrap())
}

#[tokio::test]
async fn every_request_needs_the_token() {
    let d = start().await;
    let anonymous = d.client.get(d.url("/jobs")).send().await.unwrap();
    let (status, refused) = error_of(anonymous).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused.reason, Reason::Unauthorized);
    assert!(refused.error.contains("token"));

    let wrong = d.client.get(d.url("/jobs")).bearer_auth("guess");
    assert_eq!(
        wrong.send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    // Unknown paths don't reveal themselves without the token either.
    let unknown = d.client.get(d.url("/nope")).send().await.unwrap();
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
    let unknown = d.get("/nope").send().await.unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let jobs: Vec<Job> = d.get("/jobs").send().await.unwrap().json().await.unwrap();
    assert!(jobs.is_empty());
}

#[tokio::test]
async fn the_event_stream_needs_the_token() {
    let d = start().await;
    for offered in [
        "",
        EVENTS_PROTOCOL,
        &format!("{EVENTS_PROTOCOL}, {TOKEN_PROTOCOL_PREFIX}guess"),
    ] {
        match d.events(offered).await {
            Err(tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
            other => panic!("expected a refusal for {offered:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_download_from_start_to_finish() {
    let d = start().await;
    let offered = format!("{EVENTS_PROTOCOL}, {TOKEN_PROTOCOL_PREFIX}{TOKEN}");
    let (mut events, response) = d.events(&offered).await.unwrap();
    // The server picks the events protocol, never echoing the token.
    assert_eq!(
        response.headers()["sec-websocket-protocol"],
        EVENTS_PROTOCOL
    );

    let id = d.add("file.bin").await;
    let mut states = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        while states.last() != Some(&JobState::Completed) {
            let Some(Ok(Message::Text(text))) = events.next().await else {
                continue;
            };
            if let Event::Changed { job } = serde_json::from_str(&text).unwrap() {
                assert_eq!(job.id, id);
                if states.last() != Some(&job.state) {
                    states.push(job.state);
                }
            }
        }
    })
    .await
    .expect("never completed");
    assert_eq!(states.first(), Some(&JobState::Queued));

    let job = d.job(&id).await;
    assert_eq!(job.file_name.as_deref(), Some("file.bin"));
    assert_eq!(
        (job.downloaded, job.total),
        (SIZE as u64, Some(SIZE as u64))
    );
    let file = Path::new(&job.dest_dir).join("file.bin");
    assert_eq!(std::fs::read(file).unwrap(), vec![7u8; SIZE]);
}

#[tokio::test]
async fn the_speed_history_gives_a_sample_per_second() {
    let d = start().await;
    let history = || async {
        let response = d.get("/speed").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<SpeedHistory>().await.unwrap()
    };
    let before = history().await.samples.len();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let after = history().await.samples;
    assert!(after.len() >= before + 2, "{before} then {}", after.len());
    assert!(
        after.iter().all(|&bytes| bytes == 0),
        "nothing was downloading"
    );
}

#[tokio::test]
async fn a_browsers_cookie_is_kept_but_never_given_back() {
    let d = start().await;
    let job = AddJob {
        cookie: Some("session=secret-cookie".to_owned()),
        referrer: Some("https://example.com/page".to_owned()),
        user_agent: Some("Browser/1.0".to_owned()),
        ..d.new_job("file.bin")
    };
    let response = d.post("/jobs").json(&job).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = response.json::<Added>().await.unwrap().id;
    d.wait_until(&id, JobState::Completed).await;
    let listed = d.get("/jobs").send().await.unwrap().text().await.unwrap();
    let one = d.get(&format!("/jobs/{id}")).send().await;
    let one = one.unwrap().text().await.unwrap();
    for text in [listed, one] {
        assert!(!text.contains("secret-cookie") && !text.contains("Browser/1.0"));
    }

    let broken = AddJob {
        cookie: Some("a=1\r\nX-Other: 1".to_owned()),
        ..d.new_job("file.bin")
    };
    let response = d.post("/jobs").json(&broken).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["code"], "invalidHeader");
    assert_eq!(body["name"], "cookie");
}

#[tokio::test]
async fn a_download_whose_file_is_there_asks_then_replaces_it() {
    let d = start().await;
    std::fs::write(d.dir.path().join("file.bin"), b"mine").unwrap();
    let job = AddJob {
        existing: Some(Existing::Ask),
        ..d.new_job("file.bin")
    };
    let response = d.post("/jobs").json(&job).send().await.unwrap();
    let id = response.json::<Added>().await.unwrap().id;
    d.wait_until(&id, JobState::Failed).await;
    let problem = d.job(&id).await.problem;
    assert_eq!(
        problem,
        Some(Problem::FileExists {
            name: "file.bin".into()
        })
    );

    let replace = JobChange {
        existing: Some(Existing::Replace),
        ..JobChange::default()
    };
    let request = d
        .client
        .patch(d.url(&format!("/jobs/{id}")))
        .bearer_auth(TOKEN);
    let changed = request.json(&replace).send().await.unwrap();
    assert_eq!(changed.status(), StatusCode::NO_CONTENT);
    let resumed = d.post(&format!("/jobs/{id}/resume")).send().await.unwrap();
    assert_eq!(resumed.status(), StatusCode::NO_CONTENT);
    d.wait_until(&id, JobState::Completed).await;
    assert_eq!(d.job(&id).await.file_name.as_deref(), Some("file.bin"));
    assert_ne!(
        std::fs::read(d.dir.path().join("file.bin")).unwrap(),
        b"mine"
    );
}

#[tokio::test]
async fn pause_resume_and_cancel() {
    let d = start().await;
    let id = d.add("slow.bin").await;
    d.wait_until(&id, JobState::Downloading).await;

    let paused = d.post(&format!("/jobs/{id}/pause")).send().await.unwrap();
    assert_eq!(paused.status(), StatusCode::NO_CONTENT);
    assert_eq!(d.job(&id).await.state, JobState::Paused);

    let resumed = d.post(&format!("/jobs/{id}/resume")).send().await.unwrap();
    assert_eq!(resumed.status(), StatusCode::NO_CONTENT);
    d.wait_until(&id, JobState::Downloading).await;

    let cancelled = d.post(&format!("/jobs/{id}/cancel")).send().await.unwrap();
    assert_eq!(cancelled.status(), StatusCode::NO_CONTENT);
    assert_eq!(d.job(&id).await.state, JobState::Cancelled);
}

#[tokio::test]
async fn a_jobs_own_settings_change_while_it_runs() {
    let d = start().await;
    let offered = format!("{EVENTS_PROTOCOL}, {TOKEN_PROTOCOL_PREFIX}{TOKEN}");
    let (mut events, _) = d.events(&offered).await.unwrap();
    let id = d.add("slow.bin").await;
    d.wait_until(&id, JobState::Downloading).await;
    let patch = |id: &str, change: serde_json::Value| {
        let request = d.client.patch(d.url(&format!("/jobs/{id}")));
        request.bearer_auth(TOKEN).json(&change).send()
    };

    let change = serde_json::json!({
        "speedLimit": 32_768,
        "priority": "urgent",
        "streaming": true,
    });
    let response = patch(&id, change).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let job = d.job(&id).await;
    assert_eq!(
        (job.speed_limit, job.priority, job.streaming),
        (32_768, Priority::Urgent, true)
    );
    assert_eq!(job.state, JobState::Downloading, "it keeps running");
    // Every client hears of it.
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let Some(Ok(Message::Text(text))) = events.next().await else {
                continue;
            };
            if let Event::Changed { job } = serde_json::from_str(&text).unwrap()
                && job.priority == Priority::Urgent
            {
                assert_eq!(job.speed_limit, 32_768);
                return;
            }
        }
    })
    .await
    .expect("a changed event");

    // A field left out stays as it is.
    let response = patch(&id, serde_json::json!({ "speedLimit": 0 }))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let job = d.job(&id).await;
    assert_eq!(
        (job.speed_limit, job.priority, job.streaming),
        (0, Priority::Urgent, true)
    );

    let response = patch(&id, serde_json::json!({ "priority": "asap" }))
        .await
        .unwrap();
    let (status, error) = error_of(response).await;
    assert_eq!(
        (status, error.reason),
        (StatusCode::UNPROCESSABLE_ENTITY, Reason::InvalidBody)
    );
    let missing = "2020.01.01-00.00.00-1";
    let response = patch(missing, serde_json::json!({})).await.unwrap();
    assert_eq!(error_of(response).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_download_waits_for_its_start_time() {
    let d = start().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let in_an_hour = u64::try_from(now.as_millis()).unwrap() + 3_600_000;
    let later = AddJob {
        start_at: Some(in_an_hour),
        stop_at: Some(in_an_hour + 60_000),
        ..d.new_job("file.bin")
    };
    let response = d.post("/jobs").json(&later).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = response.json::<Added>().await.unwrap().id;
    let job = d.job(&id).await;
    assert_eq!(job.state, JobState::Scheduled);
    assert_eq!(
        (job.start_at, job.stop_at),
        (Some(in_an_hour), Some(in_an_hour + 60_000))
    );

    let patch = |change: serde_json::Value| {
        let request = d.client.patch(d.url(&format!("/jobs/{id}")));
        request.bearer_auth(TOKEN).json(&change).send()
    };
    let response = patch(serde_json::json!({ "stopAt": in_an_hour - 1 }))
        .await
        .unwrap();
    let (status, error) = error_of(response).await;
    assert_eq!(
        (status, error.reason),
        (StatusCode::BAD_REQUEST, Reason::StopBeforeStart)
    );
    // `null` takes a time away: it starts now.
    let response = patch(serde_json::json!({ "startAt": null, "stopAt": null }))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    d.wait_until(&id, JobState::Completed).await;
    let job = d.job(&id).await;
    assert_eq!((job.start_at, job.stop_at), (None, None));

    // A finished download takes no times.
    let response = patch(serde_json::json!({ "startAt": in_an_hour }))
        .await
        .unwrap();
    let (status, error) = error_of(response).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(matches!(error.reason, Reason::InvalidTransition { .. }));
}

#[tokio::test]
async fn time_slots_hold_downloads_while_they_apply() {
    let d = start().await;
    let list = d.get("/slots").send().await.unwrap();
    let list = list.json::<SlotList>().await.unwrap();
    assert!(list.slots.is_empty());

    let put = |path: &str, body: serde_json::Value| {
        let request = d.client.put(d.url(path)).bearer_auth(TOKEN);
        request.json(&body).send()
    };
    // Every day, midnight to midnight: it applies now.
    let always = serde_json::json!({
        "name": "Night",
        "days": ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"],
        "from": "00:00",
        "to": "00:00",
        "downloads": "stop",
    });
    let response = d.post("/slots").json(&always).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let night = response.json::<AddedSlot>().await.unwrap().id;
    let list = d.get("/slots").send().await.unwrap();
    let slots = list.json::<SlotList>().await.unwrap().slots;
    let expected = Slot {
        id: night,
        place: 0,
        name: "Night".into(),
        enabled: true,
        category: None,
        days: vec![
            Day::Monday,
            Day::Tuesday,
            Day::Wednesday,
            Day::Thursday,
            Day::Friday,
            Day::Saturday,
            Day::Sunday,
        ],
        date: None,
        from: "00:00".into(),
        to: "00:00".into(),
        downloads: Some(SlotDownloads::Stop),
        let_finish: false,
        stay_paused: false,
        speed_limit: None,
        max_concurrent: None,
        active: true,
    };
    assert_eq!(slots, [expected]);

    let id = d.add("file.bin").await;
    let job = d.job(&id).await;
    assert_eq!((job.state, job.held_by), (JobState::Scheduled, Some(night)));

    // What's wrong is said.
    let refusals = [
        (
            serde_json::json!({ "from": "25:00", "to": "01:00", "downloads": "stop", "days": ["monday"] }),
            Reason::InvalidTime {
                text: "25:00".into(),
            },
        ),
        (
            serde_json::json!({ "from": "01:00", "to": "02:00", "downloads": "stop" }),
            Reason::SlotWithoutDays,
        ),
        (
            serde_json::json!({ "from": "01:00", "to": "02:00", "date": "2026-10-14" }),
            Reason::SlotWithoutEffect,
        ),
        (
            serde_json::json!({ "from": "01:00", "to": "02:00", "date": "2026-10-14", "maxConcurrent": 0 }),
            Reason::OutOfRange {
                setting: "maxConcurrent".into(),
                min: 1,
                max: 50,
            },
        ),
        (
            serde_json::json!({ "from": "01:00", "to": "02:00", "date": "2026-10-14", "speedLimit": 0, "category": 99 }),
            Reason::CategoryNotFound { id: 99 },
        ),
    ];
    for (body, reason) in refusals {
        let response = d.post("/slots").json(&body).send().await.unwrap();
        assert_eq!(error_of(response).await.1.reason, reason, "{body}");
    }
    let response = d
        .post("/slots/reorder")
        .json(&ReorderSlots { ids: vec![] })
        .send()
        .await
        .unwrap();
    assert_eq!(error_of(response).await.1.reason, Reason::InvalidSlotOrder);

    // Switched off, it lets the download go.
    let off = serde_json::json!({ "enabled": false, "from": "00:00", "to": "00:00", "downloads": "stop", "days": ["monday"] });
    let response = put(&format!("/slots/{night}"), off).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    d.wait_until(&id, JobState::Completed).await;
    let response = d
        .client
        .delete(d.url(&format!("/slots/{night}")))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = put(&format!("/slots/{night}"), always).await.unwrap();
    assert_eq!(
        error_of(response).await.1.reason,
        Reason::SlotNotFound { id: night }
    );
}

#[tokio::test]
async fn waiting_jobs_are_reordered() {
    let d = start().await;
    let first = d.add("slow.bin").await;
    let second = d.add("slow.bin").await;
    for id in [&first, &second] {
        let paused = d.post(&format!("/jobs/{id}/pause")).send().await.unwrap();
        assert_eq!(paused.status(), StatusCode::NO_CONTENT);
    }
    let reorder = |ids: &[&str], to: Reorder| {
        let body = ReorderJobs {
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            to,
        };
        d.post("/jobs/reorder").json(&body).send()
    };
    let (before, after) = (d.job(&first).await.place, d.job(&second).await.place);
    assert!(before < after, "new jobs go last");

    let response = reorder(&[&second], Reorder::Top).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(d.job(&second).await.place, before);
    assert_eq!(d.job(&first).await.place, after);
    // Dragged back below the other one.
    let dropped = serde_json::json!({ "ids": [second], "to": { "after": first } });
    let response = d.post("/jobs/reorder").json(&dropped).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(d.job(&second).await.place, after);
    let response = reorder(&[&second], Reorder::Before(first.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(d.job(&second).await.place, before);

    for (ids, to) in [
        (vec!["soon"], Reorder::Up),
        (vec![first.as_str()], Reorder::After("soon".into())),
    ] {
        let response = reorder(&ids, to).await.unwrap();
        let (status, error) = error_of(response).await;
        let invalid = Reason::InvalidJobId { id: "soon".into() };
        assert_eq!((status, error.reason), (StatusCode::BAD_REQUEST, invalid));
    }
    let missing = "2020.01.01-00.00.00-1";
    let response = reorder(&[&first, missing], Reorder::Top).await.unwrap();
    let (status, error) = error_of(response).await;
    let not_found = Reason::JobNotFound { id: missing.into() };
    assert_eq!((status, error.reason), (StatusCode::NOT_FOUND, not_found));
    assert_eq!(d.job(&first).await.place, after, "nothing moved");
    let wrong = serde_json::json!({ "ids": [first], "to": "sideways" });
    let response = d.post("/jobs/reorder").json(&wrong).send().await.unwrap();
    assert_eq!(error_of(response).await.0, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn videos_start_in_streaming_mode_unless_asked_otherwise() {
    let d = start().await;
    let video = d.add("clip.mp4").await;
    let other = d.add("data.bin").await;
    let asked = AddJob {
        streaming: Some(true),
        ..d.new_job("asked.bin")
    };
    let response = d.post("/jobs").json(&asked).send().await.unwrap();
    let asked = response.json::<Added>().await.unwrap().id;
    assert!(d.job(&video).await.streaming);
    assert!(!d.job(&other).await.streaming);
    assert!(d.job(&asked).await.streaming);
}

#[tokio::test]
async fn remove_takes_a_job_off_the_list() {
    let d = start().await;
    let offered = format!("{EVENTS_PROTOCOL}, {TOKEN_PROTOCOL_PREFIX}{TOKEN}");
    let (mut events, _) = d.events(&offered).await.unwrap();
    let id = d.add("slow.bin").await;
    d.wait_until(&id, JobState::Downloading).await;

    let removed = d
        .client
        .delete(d.url(&format!("/jobs/{id}")))
        .bearer_auth(TOKEN);
    assert_eq!(
        removed.send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    let jobs: Vec<Job> = d.get("/jobs").send().await.unwrap().json().await.unwrap();
    assert!(jobs.is_empty());
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let Some(Ok(Message::Text(text))) = events.next().await else {
                continue;
            };
            if let Event::Removed { id: gone } = serde_json::from_str(&text).unwrap() {
                assert_eq!(gone, id);
                return;
            }
        }
    })
    .await
    .expect("no removed event");

    let again = d
        .client
        .delete(d.url(&format!("/jobs/{id}")))
        .bearer_auth(TOKEN);
    let (status, refused) = error_of(again.send().await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(refused.reason, Reason::JobNotFound { id: id.clone() });
    let gone = d.get(&format!("/jobs/{id}")).send().await.unwrap();
    let (status, refused) = error_of(gone).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(refused.reason, Reason::JobNotFound { id });
}

#[tokio::test]
async fn several_downloads_are_added_at_once() {
    let d = start().await;
    let mut sftp = d.new_job("file.bin");
    sftp.url = "sftp://example.com/file.bin".into();
    let mut relative = d.new_job("file.bin");
    relative.dest_dir = Some("downloads".into());
    let jobs = vec![d.new_job("file.bin"), sftp, relative, d.new_job("slow.bin")];

    let response = d.post("/jobs/batch").json(&AddJobs { jobs }).send().await;
    let response = response.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let added: AddedJobs = response.json().await.unwrap();
    let [Some(first), None, None, Some(last)] = &added.ids[..] else {
        panic!("expected the first and last added, got {:?}", added.ids);
    };
    let refused: Vec<(u32, Reason)> = added
        .refused
        .into_iter()
        .map(|refused| (refused.index, refused.error.reason))
        .collect();
    assert_eq!(
        refused,
        [
            (
                1,
                Reason::UnsupportedScheme {
                    scheme: "sftp".into()
                }
            ),
            (2, Reason::RelativeFolder),
        ]
    );
    let jobs: Vec<Job> = d.get("/jobs").send().await.unwrap().json().await.unwrap();
    let listed: Vec<&str> = jobs.iter().map(|job| job.id.as_str()).collect();
    assert_eq!(listed, [first.as_str(), last.as_str()]);
    d.wait_until(first, JobState::Completed).await;

    // More than the setting allows: none.
    let request = d.client.patch(d.url("/settings")).bearer_auth(TOKEN);
    let response = request
        .json(&serde_json::json!({ "maxBatch": 10 }))
        .send()
        .await;
    assert_eq!(response.unwrap().status(), StatusCode::NO_CONTENT);
    let jobs = vec![d.new_job("file.bin"); 11];
    let response = d.post("/jobs/batch").json(&AddJobs { jobs }).send().await;
    let (status, error) = error_of(response.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error.reason, Reason::TooManyLinks { count: 11, max: 10 });
    let jobs: Vec<Job> = d.get("/jobs").send().await.unwrap().json().await.unwrap();
    assert_eq!(jobs.len(), 2);
}

#[tokio::test]
async fn a_preview_lists_the_links_in_some_text() {
    let d = start().await;
    let id = d.add("file.bin").await;
    let known = d.job(&id).await.url;
    let other = format!("{}/clip%20one.mp4", d.files);
    let text = format!("Here: {known}\n{other}, and sftp://example.com/x.zip.\n{known}");
    let preview = |text: String, dest_dir: Option<String>| {
        d.post("/jobs/preview")
            .json(&PreviewLinks { text, dest_dir })
            .send()
    };

    let response = preview(text, None).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let links = response.json::<Preview>().await.unwrap().links;
    let general = Matched {
        category: 1,
        extension: Some("bin".into()),
        by_folder: false,
    };
    assert_eq!(
        links[0],
        FoundLink::Link {
            url: known.clone(),
            name: "file.bin".into(),
            matched: general,
            known: true,
            exists: false,
        }
    );
    let FoundLink::Link {
        name, known: false, ..
    } = &links[1]
    else {
        panic!("expected a new link, got {:?}", links[1]);
    };
    assert_eq!(name, "clip one.mp4");
    let FoundLink::Invalid { text, refused } = &links[2] else {
        panic!("expected an invalid link, got {:?}", links[2]);
    };
    assert_eq!(text, "sftp://example.com/x.zip");
    assert_eq!(
        refused.reason,
        Reason::UnsupportedScheme {
            scheme: "sftp".into()
        }
    );
    assert_eq!(links.len(), 3);

    let response = preview(known, Some("downloads".into())).await.unwrap();
    let (status, error) = error_of(response).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error.reason, Reason::RelativeFolder);

    // Big lists fit (axum takes only 2 MB by default).
    let long = format!("{}/{}.bin", d.files, "x".repeat(400));
    let text = (0..9_000)
        .map(|i| format!("{long}?{i}\n"))
        .collect::<String>();
    assert!(text.len() > 3_000_000);
    let response = preview(text, None).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Preview>().await.unwrap().links.len(), 9_000);
}

#[tokio::test]
async fn a_preview_gives_each_link_of_a_sequence() {
    let d = start().await;
    let preview = |text: String| {
        d.post("/jobs/preview")
            .json(&PreviewLinks {
                text,
                dest_dir: None,
            })
            .send()
    };

    let response = preview(format!("{}/part[08-10].{{zip,rar}}", d.files)).await;
    let links = response.unwrap().json::<Preview>().await.unwrap().links;
    let names: Vec<&str> = links
        .iter()
        .map(|link| match link {
            FoundLink::Link { name, .. } => name.as_str(),
            FoundLink::Invalid { text, .. } => panic!("expected a link, got {text}"),
        })
        .collect();
    assert_eq!(
        names,
        [
            "part08.zip",
            "part08.rar",
            "part09.zip",
            "part09.rar",
            "part10.zip",
            "part10.rar"
        ]
    );

    // Refused at once, counted before making them.
    let response = preview(format!("{}/[1-999999999999]", d.files)).await;
    let (status, error) = error_of(response.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.reason,
        Reason::TooManyLinks {
            count: 999_999_999_999,
            max: 10_000
        }
    );
}

#[tokio::test]
async fn bad_requests_say_what_is_wrong() {
    let d = start().await;
    let add = |job: &AddJob| d.post("/jobs").json(job).send();
    let refused = async |request: RequestBuilder| error_of(request.send().await.unwrap()).await;

    let mut job = d.new_job("file.bin");
    job.url = "not a url".to_owned();
    let (status, body) = error_of(add(&job).await.unwrap()).await;
    assert_eq!(
        (status, body.reason),
        (StatusCode::BAD_REQUEST, Reason::InvalidUrl)
    );
    assert!(body.error.contains("not a URL"), "{}", body.error);

    job.url = "sftp://example.com/file".to_owned();
    let (status, body) = error_of(add(&job).await.unwrap()).await;
    let scheme = Reason::UnsupportedScheme {
        scheme: "sftp".into(),
    };
    assert_eq!((status, body.reason), (StatusCode::BAD_REQUEST, scheme));

    let mut job = d.new_job("file.bin");
    job.dest_dir = Some("relative/folder".to_owned());
    let (status, body) = error_of(add(&job).await.unwrap()).await;
    assert_eq!(
        (status, body.reason),
        (StatusCode::BAD_REQUEST, Reason::RelativeFolder)
    );

    // General has no folder yet, so a folder is required.
    job.dest_dir = None;
    let (status, body) = error_of(add(&job).await.unwrap()).await;
    let no_folder = Reason::NoFolder {
        category: "General".into(),
    };
    assert_eq!((status, body.reason), (StatusCode::BAD_REQUEST, no_folder));

    let broken = d.post("/jobs").header("content-type", "application/json");
    let (status, body) = refused(broken.body("{")).await;
    assert_eq!(
        (status, body.reason),
        (StatusCode::BAD_REQUEST, Reason::InvalidBody)
    );
    let missing_url = d
        .post("/jobs")
        .json(&serde_json::json!({ "destDir": "/x" }));
    let (status, body) = refused(missing_url).await;
    let expected = (StatusCode::UNPROCESSABLE_ENTITY, Reason::InvalidBody);
    assert_eq!((status, body.reason), expected);

    let (status, body) = refused(d.post("/jobs/nonsense/pause")).await;
    let invalid = Reason::InvalidJobId {
        id: "nonsense".into(),
    };
    assert_eq!((status, body.reason), (StatusCode::BAD_REQUEST, invalid));
    let unknown_id = "2026.01.01-00.00.00-1";
    let (status, body) = refused(d.post(&format!("/jobs/{unknown_id}/pause"))).await;
    let not_found = Reason::JobNotFound {
        id: unknown_id.into(),
    };
    assert_eq!((status, body.reason), (StatusCode::NOT_FOUND, not_found));

    let id = d.add("file.bin").await;
    d.wait_until(&id, JobState::Completed).await;
    let (status, body) = refused(d.post(&format!("/jobs/{id}/resume"))).await;
    let conflict = Reason::InvalidTransition {
        from: JobState::Completed,
        to: JobState::Queued,
    };
    assert_eq!((status, body.reason), (StatusCode::CONFLICT, conflict));
    assert!(body.error.contains("Completed"), "{}", body.error);
}

#[tokio::test]
async fn numbers_ottie_cant_hold_are_refused() {
    // Fits a u64, not the database's signed integers.
    const TOO_BIG: u64 = 1 << 63;
    const MOST_LIMIT: u64 = 1 << 40;
    const END_OF_9999: u64 = 253_402_300_799_999;
    let d = start().await;
    let out_of_range = |setting: &str, max: u64| Reason::OutOfRange {
        setting: setting.into(),
        min: 0,
        max,
    };
    let refused = async |request: RequestBuilder| {
        let (status, body) = error_of(request.send().await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{}", body.error);
        body.reason
    };

    // Times past the end of year 9999: u64::MAX made Windows' clock panic.
    let never = AddJob {
        start_at: Some(u64::MAX),
        ..d.new_job("file.bin")
    };
    let expected = out_of_range("startAt", END_OF_9999);
    assert_eq!(refused(d.post("/jobs").json(&never)).await, expected);
    let late = AddJob {
        stop_at: Some(END_OF_9999 + 1),
        ..d.new_job("file.bin")
    };
    let expected = out_of_range("stopAt", END_OF_9999);
    assert_eq!(refused(d.post("/jobs").json(&late)).await, expected);
    // In a batch, only that link is refused.
    let jobs = vec![never, d.new_job("file.bin")];
    let response = d.post("/jobs/batch").json(&AddJobs { jobs }).send().await;
    let added: AddedJobs = response.unwrap().json().await.unwrap();
    assert!(matches!(&added.ids[..], [None, Some(_)]), "{:?}", added.ids);
    let refused_one: Vec<_> = added.refused.into_iter().map(|r| r.index).collect();
    assert_eq!(refused_one, [0]);
    // The last moment of 9999 is taken.
    let last = AddJob {
        start_at: Some(END_OF_9999),
        ..d.new_job("file.bin")
    };
    let response = d.post("/jobs").json(&last).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = response.json::<Added>().await.unwrap().id;
    assert_eq!(d.job(&id).await.start_at, Some(END_OF_9999));

    // A download's own limit and times; nothing changes.
    let patch = |change: serde_json::Value| {
        let request = d.client.patch(d.url(&format!("/jobs/{id}")));
        request.bearer_auth(TOKEN).json(&change)
    };
    let expected = out_of_range("speedLimit", MOST_LIMIT);
    let limit = serde_json::json!({ "speedLimit": TOO_BIG });
    assert_eq!(refused(patch(limit)).await, expected);
    let expected = out_of_range("stopAt", END_OF_9999);
    let stop = serde_json::json!({ "stopAt": u64::MAX });
    assert_eq!(refused(patch(stop)).await, expected);
    let job = d.job(&id).await;
    assert_eq!((job.speed_limit, job.stop_at), (0, None));

    // The total limit, which the database couldn't save: refused with the
    // rest of the change. 1 TiB/s is taken.
    let settings = |change: serde_json::Value| {
        let request = d.client.patch(d.url("/settings"));
        request.bearer_auth(TOKEN).json(&change)
    };
    let before: Settings = d
        .get("/settings")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let change = serde_json::json!({ "globalLimit": TOO_BIG, "maxRetries": 3 });
    let expected = out_of_range("globalLimit", MOST_LIMIT);
    assert_eq!(refused(settings(change)).await, expected);
    let after: Settings = d
        .get("/settings")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after, before);
    let most = serde_json::json!({ "globalLimit": MOST_LIMIT });
    let response = settings(most).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // A category's and a time slot's limit.
    let category = CategoryBody {
        name: "Huge".into(),
        folder: None,
        extensions: vec![],
        speed_limit: TOO_BIG,
        max_concurrent: 0,
    };
    let expected = out_of_range("speedLimit", MOST_LIMIT);
    let request = d.post("/categories").json(&category);
    assert_eq!(refused(request).await, expected);
    let slot = serde_json::json!({
        "days": ["monday"], "from": "01:00", "to": "02:00", "speedLimit": TOO_BIG
    });
    assert_eq!(refused(d.post("/slots").json(&slot)).await, expected);
}

#[tokio::test]
async fn failures_carry_a_problem_code() {
    let d = start().await;
    let id = d.add("missing.bin").await;
    d.wait_until(&id, JobState::Failed).await;
    let job = d.job(&id).await;
    assert_eq!(job.problem, Some(Problem::HttpStatus { status: 404 }));
    assert_eq!(job.error.as_deref(), Some("server answered 404 Not Found"));
}

#[tokio::test]
async fn the_ui_is_served_without_the_token() {
    let ui = tempfile::tempdir().unwrap();
    std::fs::write(ui.path().join("index.html"), "<!doctype html>app").unwrap();
    std::fs::create_dir_all(ui.path().join("_app/immutable")).unwrap();
    std::fs::write(ui.path().join("_app/immutable/start.abc.js"), "code").unwrap();
    std::fs::write(ui.path().parent().unwrap().join("secret.txt"), "no").unwrap();
    let d = start_with(UiSource::Folder(ui.path().to_owned())).await;
    let get = |path: &str| d.client.get(format!("http://{}{path}", d.addr)).send();

    let index = get("/").await.unwrap();
    assert_eq!(index.headers()["cache-control"], "no-cache");
    assert_eq!(
        index.headers()["content-security-policy"],
        "frame-ancestors 'self' chrome-extension: moz-extension:"
    );
    assert!(
        index.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert_eq!(index.text().await.unwrap(), "<!doctype html>app");

    let script = get("/_app/immutable/start.abc.js").await.unwrap();
    assert!(!script.headers().contains_key("content-security-policy"));
    assert!(
        script.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    assert!(
        script.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("javascript")
    );

    // A page of the app gets index.html; a missing file doesn't.
    assert_eq!(
        get("/settings").await.unwrap().text().await.unwrap(),
        "<!doctype html>app"
    );
    assert_eq!(
        get("/missing.js").await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    // Nothing outside the folder, and the API stays the API.
    let escape = get("/..%2Fsecret.txt").await.unwrap();
    assert_ne!(escape.text().await.unwrap(), "no");
    let (status, body) = error_of(get("/api/v2/jobs").await.unwrap()).await;
    assert_eq!(
        (status, body.reason),
        (StatusCode::NOT_FOUND, Reason::NoSuchEndpoint)
    );
}

#[tokio::test]
async fn without_a_ui_build_the_page_says_how_to_make_one() {
    let empty = tempfile::tempdir().unwrap();
    let d = start_with(UiSource::Folder(empty.path().to_owned())).await;
    let page = d
        .client
        .get(format!("http://{}/", d.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        page.text()
            .await
            .unwrap()
            .contains("pnpm --filter ui build")
    );
}

#[tokio::test]
async fn settings_are_read_and_changed() {
    let d = start().await;
    let offered = format!("{EVENTS_PROTOCOL}, {TOKEN_PROTOCOL_PREFIX}{TOKEN}");
    let (mut events, _) = d.events(&offered).await.unwrap();
    let settings = || async {
        let response = d.get("/settings").send().await.unwrap();
        response.json::<Settings>().await.unwrap()
    };
    let defaults = Settings {
        setup_done: false,
        setup_step: None,
        max_concurrent: 3,
        global_limit: 0,
        max_retries: 10,
        retries_before_yield: 3,
        first_retry_wait_ms: 2000,
        max_batch: 10_000,
        stream_media: true,
        appearance: None,
        language: None,
        existing: Existing::Rename,
    };
    assert_eq!(settings().await, defaults, "a new daemon offers the setup");

    let patch = |change: serde_json::Value| {
        let request = d.client.patch(d.url("/settings")).bearer_auth(TOKEN);
        request.json(&change).send()
    };
    let change = serde_json::json!({
        "setupDone": true,
        "setupStep": "categories",
        "maxConcurrent": 5,
        "globalLimit": 4_000_000,
        "firstRetryWaitMs": 500,
        "maxBatch": 500,
        "streamMedia": false,
        "appearance": { "mode": "dark", "palette": "neutral" },
        "language": "fr",
        "existing": "ask",
    });
    let response = patch(change).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let changed = Settings {
        setup_done: true,
        setup_step: Some(SetupStep::Categories),
        max_concurrent: 5,
        global_limit: 4_000_000,
        first_retry_wait_ms: 500,
        max_batch: 500,
        stream_media: false,
        appearance: Some(Appearance {
            mode: AppearanceMode::Dark,
            palette: Palette::Neutral,
        }),
        language: Some("fr".into()),
        existing: Existing::Ask,
        ..defaults
    };
    assert_eq!(settings().await, changed);
    // Every client hears of it, in one event.
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let Some(Ok(Message::Text(text))) = events.next().await else {
                continue;
            };
            if let Event::SettingsChanged { settings } = serde_json::from_str(&text).unwrap() {
                assert_eq!(settings, changed);
                return;
            }
        }
    })
    .await
    .expect("no settingsChanged event");

    // Fields left out stay as they are.
    patch(serde_json::to_value(SettingsChange::default()).unwrap())
        .await
        .unwrap();
    assert_eq!(settings().await, changed);

    // Unknown values are refused, and nothing changes.
    let wrong = serde_json::json!({ "maxConcurrent": 1, "appearance": { "mode": "teal", "palette": "sea" } });
    let response = patch(wrong).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response.json::<ApiError>().await.unwrap();
    assert_eq!(body.reason, Reason::InvalidBody);
    assert_eq!(settings().await, changed);

    // So are values out of their range, and odd languages.
    let refused = |change: serde_json::Value| async {
        let response = patch(change).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        response.json::<ApiError>().await.unwrap().reason
    };
    let too_many = serde_json::json!({ "maxRetries": 2, "maxConcurrent": 0 });
    let out_of_range = Reason::OutOfRange {
        setting: "maxConcurrent".into(),
        min: 1,
        max: 50,
    };
    assert_eq!(refused(too_many).await, out_of_range);
    let odd = serde_json::json!({ "language": "../fr" });
    let invalid = Reason::InvalidLanguage {
        language: "../fr".into(),
    };
    assert_eq!(refused(odd).await, invalid);
    assert_eq!(settings().await, changed);

    // Null puts every device back on its own system's language.
    let system = serde_json::json!({ "language": null });
    assert_eq!(
        patch(system).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    let language = None;
    assert_eq!(
        settings().await,
        Settings {
            language,
            ..changed.clone()
        }
    );

    // The setup starts over with null; a step it doesn't have is refused.
    let unknown = serde_json::json!({ "setupStep": "later" });
    let response = patch(unknown).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let over = serde_json::json!({ "setupDone": false, "setupStep": null });
    assert_eq!(patch(over).await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(
        settings().await,
        Settings {
            setup_done: false,
            setup_step: None,
            language: None,
            ..changed
        }
    );
}

#[tokio::test]
async fn categories_are_listed_added_matched_changed_and_removed() {
    let d = start().await;
    let offered = format!("{EVENTS_PROTOCOL}, {TOKEN_PROTOCOL_PREFIX}{TOKEN}");
    let (mut events, _) = d.events(&offered).await.unwrap();
    let list = || async {
        let response = d.get("/categories").send().await.unwrap();
        response.json::<Vec<Category>>().await.unwrap()
    };
    let built_in: Vec<_> = list().await.iter().map(|c| c.built_in).collect();
    assert_eq!(built_in, [Some(BuiltIn::General), Some(BuiltIn::Other)]);

    let folder = d.dir.path().join("videos").to_string_lossy().into_owned();
    let videos = CategoryBody {
        name: "Videos".into(),
        folder: Some(folder.clone()),
        extensions: vec![".MKV".into(), "mp4".into()],
        speed_limit: 0,
        max_concurrent: 2,
    };
    let added = d.post("/categories").json(&videos).send().await.unwrap();
    assert_eq!(added.status(), StatusCode::CREATED);
    let id = added.json::<AddedCategory>().await.unwrap().id;
    let listed = list().await;
    assert_eq!(listed[2].extensions, ["mkv", "mp4"]);
    assert_eq!(listed[2].folder.as_deref(), Some(folder.as_str()));
    assert_eq!(listed[2].built_in, None);
    // Subscribers hear about it.
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let Some(Ok(Message::Text(text))) = events.next().await else {
                continue;
            };
            if let Event::CategoryChanged { category } = serde_json::from_str(&text).unwrap() {
                assert_eq!(category.id, id);
                return;
            }
        }
    })
    .await
    .expect("no categoryChanged event");

    let link = serde_json::json!({ "url": format!("{}/Film.MKV?token=x", d.files) });
    let matched: Matched = d
        .post("/categories/match")
        .json(&link)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (matched.category, matched.extension.as_deref()),
        (id, Some("mkv"))
    );
    // A folder of no category: Other (ID 2 on a new database).
    let elsewhere = d
        .dir
        .path()
        .join("elsewhere")
        .to_string_lossy()
        .into_owned();
    let link = serde_json::json!({ "url": format!("{}/Film.MKV", d.files), "destDir": elsewhere });
    let matched: Matched = d
        .post("/categories/match")
        .json(&link)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!((matched.category, matched.by_folder), (2, true));

    let taken = CategoryBody {
        name: "Films".into(),
        extensions: vec!["mkv".into()],
        ..videos.clone()
    };
    let (status, refused) =
        error_of(d.post("/categories").json(&taken).send().await.unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(matches!(refused.reason, Reason::ExtensionTaken { .. }));
    let relative = CategoryBody {
        name: "Films".into(),
        folder: Some("films".into()),
        extensions: vec![],
        ..videos.clone()
    };
    let (status, refused) =
        error_of(d.post("/categories").json(&relative).send().await.unwrap()).await;
    assert_eq!(
        (status, refused.reason),
        (StatusCode::BAD_REQUEST, Reason::RelativeFolder)
    );

    let renamed = CategoryBody {
        name: "Movies".into(),
        ..videos
    };
    let changed = d
        .client
        .put(d.url(&format!("/categories/{id}")))
        .bearer_auth(TOKEN);
    assert_eq!(
        changed.json(&renamed).send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(list().await[2].name, "Movies");

    // A download can be moved in, its file with it, and goes back to General
    // (its file staying put) when its category goes.
    let job = d.add("file.bin").await;
    d.wait_until(&job, JobState::Completed).await;
    let moved = d
        .post(&format!("/jobs/{job}/category"))
        .json(&serde_json::json!({ "category": id }));
    assert_eq!(moved.send().await.unwrap().status(), StatusCode::NO_CONTENT);
    let listed = d.job(&job).await;
    assert_eq!((listed.category, listed.dest_dir), (id, folder.clone()));
    assert!(!listed.moving);
    assert!(Path::new(&folder).join("file.bin").is_file());
    let removed = d
        .client
        .delete(d.url(&format!("/categories/{id}")))
        .bearer_auth(TOKEN);
    assert_eq!(
        removed.send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    let listed = d.job(&job).await;
    assert_eq!((listed.category, listed.dest_dir), (1, folder));
    assert_eq!(list().await.len(), 2);

    let general = d.client.delete(d.url("/categories/1")).bearer_auth(TOKEN);
    let (status, refused) = error_of(general.send().await.unwrap()).await;
    let category = "General".into();
    assert_eq!(
        (status, refused.reason),
        (
            StatusCode::CONFLICT,
            Reason::CannotRemoveBuiltIn { category }
        )
    );
}

#[tokio::test]
async fn views_are_listed_added_changed_and_removed() {
    let d = start().await;
    let offered = format!("{EVENTS_PROTOCOL}, {TOKEN_PROTOCOL_PREFIX}{TOKEN}");
    let (mut events, _) = d.events(&offered).await.unwrap();
    let list = || async {
        let response = d.get("/views").send().await.unwrap();
        response.json::<Vec<View>>().await.unwrap()
    };
    assert_eq!(list().await, []);

    // Sent the way the UI writes it.
    let body = serde_json::json!({
        "name": "Trouble",
        "filters": {
            "states": ["failed", "retrying", "connecting"],
            "categories": null,
            "nameContains": " iso ",
            "added": "week",
        },
        "arrangement": { "sort": "size", "direction": "asc", "grouped": false },
    });
    let added = d.post("/views").json(&body).send().await.unwrap();
    assert_eq!(added.status(), StatusCode::CREATED);
    let id = added.json::<AddedView>().await.unwrap().id;
    let expected = View {
        id,
        name: "Trouble".into(),
        filters: ViewFilters {
            states: Some(vec![
                JobState::Downloading,
                JobState::Retrying,
                JobState::Failed,
            ]),
            categories: None,
            name_contains: "iso".into(),
            added: AddedWithin::Week,
        },
        arrangement: Arrangement {
            sort: SortKey::Size,
            direction: SortDirection::Ascending,
            grouped: false,
        },
    };
    assert_eq!(list().await, std::slice::from_ref(&expected));
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let Some(Ok(Message::Text(text))) = events.next().await else {
                continue;
            };
            if let Event::ViewChanged { view } = serde_json::from_str(&text).unwrap() {
                assert_eq!(view, expected);
                return;
            }
        }
    })
    .await
    .expect("no viewChanged event");

    let taken = ViewBody {
        name: "TROUBLE".into(),
        filters: expected.filters.clone(),
        arrangement: expected.arrangement,
    };
    let (status, refused) = error_of(d.post("/views").json(&taken).send().await.unwrap()).await;
    let name = "TROUBLE".into();
    assert_eq!(
        (status, refused.reason),
        (StatusCode::CONFLICT, Reason::ViewNameTaken { name })
    );
    let nowhere = ViewBody {
        name: "Nowhere".into(),
        filters: ViewFilters {
            categories: Some(vec![99]),
            ..expected.filters.clone()
        },
        ..taken.clone()
    };
    let (status, refused) = error_of(d.post("/views").json(&nowhere).send().await.unwrap()).await;
    assert_eq!(
        (status, refused.reason),
        (StatusCode::NOT_FOUND, Reason::CategoryNotFound { id: 99 })
    );

    let renamed = ViewBody {
        name: "Problems".into(),
        ..taken
    };
    let changed = d
        .client
        .put(d.url(&format!("/views/{id}")))
        .bearer_auth(TOKEN);
    assert_eq!(
        changed.json(&renamed).send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(list().await[0].name, "Problems");

    let removed = d
        .client
        .delete(d.url(&format!("/views/{id}")))
        .bearer_auth(TOKEN);
    assert_eq!(
        removed.send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(list().await, []);
    let again = d
        .client
        .delete(d.url(&format!("/views/{id}")))
        .bearer_auth(TOKEN);
    let (status, refused) = error_of(again.send().await.unwrap()).await;
    assert_eq!(
        (status, refused.reason),
        (StatusCode::NOT_FOUND, Reason::ViewNotFound { id })
    );
}

#[tokio::test]
async fn a_play_link_serves_the_file_without_the_token() {
    let d = start().await;
    let id = d.add("file.bin").await;
    d.wait_until(&id, JobState::Completed).await;
    let anonymous = d.client.post(d.url(&format!("/jobs/{id}/stream")));
    let refused = anonymous.send().await.unwrap();
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);

    let link = d.play_link(&id).await;
    assert!(link.ends_with("/file.bin"), "named after the file: {link}");
    assert_eq!(d.play_link(&id).await, link, "the same link every time");

    let whole = d.client.get(&link).send().await.unwrap();
    assert_eq!(whole.status(), StatusCode::OK);
    let headers = whole.headers();
    assert_eq!(headers["accept-ranges"], "bytes");
    assert_eq!(headers["content-length"], SIZE.to_string().as_str());
    assert_eq!(headers["content-security-policy"], "sandbox");
    assert_eq!(whole.bytes().await.unwrap(), vec![7u8; SIZE]);

    let part = d.client.get(&link).header("Range", "bytes=100-199");
    let part = part.send().await.unwrap();
    assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
    let range = format!("bytes 100-199/{SIZE}");
    assert_eq!(part.headers()["content-range"], range.as_str());
    assert_eq!(part.bytes().await.unwrap().len(), 100);

    let past = d
        .client
        .get(&link)
        .header("Range", format!("bytes={SIZE}-"));
    let past = past.send().await.unwrap();
    assert_eq!(past.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    let range = format!("bytes */{SIZE}");
    assert_eq!(past.headers()["content-range"], range.as_str());

    // The name is only for players.
    let (unnamed, _) = link.rsplit_once('/').unwrap();
    let unnamed = d.client.get(unnamed).send().await.unwrap();
    assert_eq!(unnamed.status(), StatusCode::OK);

    let guess = d.client.get(d.url("/stream/0123/file.bin")).send().await;
    let (status, refused) = error_of(guess.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(refused.reason, Reason::NoSuchLink);
}

#[tokio::test]
async fn a_play_link_streams_a_download_as_it_arrives() {
    let d = start().await;
    let id = d.add("slow.bin").await;
    let link = d.play_link(&id).await;
    let mut response = d.client.get(&link).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The server sends no size, so there are no ranges to ask for.
    assert_eq!(response.headers()["accept-ranges"], "none");

    let first = response.chunk().await.unwrap().unwrap();
    assert_ne!(d.job(&id).await.state, JobState::Completed);
    let mut received = first.len();
    tokio::time::timeout(TIMEOUT, async {
        while let Some(chunk) = response.chunk().await.unwrap() {
            assert!(chunk.iter().all(|&byte| byte == 1));
            received += chunk.len();
        }
    })
    .await
    .expect("never finished");
    assert_eq!(received, 200 * 1024);
}

#[tokio::test]
async fn the_api_describes_itself() {
    let d = start().await;
    let anonymous = d.client.get(d.url("/openapi.json")).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let spec: serde_json::Value = d
        .get("/openapi.json")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(spec, serde_json::to_value(ottie_api::openapi()).unwrap());
    assert!(spec["paths"]["/api/v1/jobs"]["post"].is_object());
}

impl Daemon {
    /// A device with a token of its own, added by this computer.
    async fn add_device(&self, name: &str, read_only: bool) -> AddedDevice {
        let body = AddDevice {
            name: name.into(),
            read_only: Some(read_only),
        };
        let response = self.post("/access/devices").json(&body).send().await;
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        response.json().await.unwrap()
    }
}

#[tokio::test]
async fn devices_get_tokens_of_their_own() {
    let d = start().await;
    let me: AccessInfo = d.get("/access").send().await.unwrap().json().await.unwrap();
    assert_eq!(me.device, None, "the token file's token is this computer's");

    let phone = d.add_device("  Phone ", false).await;
    assert_eq!(phone.device.name, "Phone");
    assert_eq!(phone.token.len(), 64);
    let as_phone = |request: reqwest::RequestBuilder| request.bearer_auth(&phone.token);

    let jobs = as_phone(d.client.get(d.url("/jobs"))).send().await.unwrap();
    assert_eq!(jobs.status(), StatusCode::OK);
    let seen: AccessInfo = as_phone(d.client.get(d.url("/access")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        seen.device.as_ref().map(|device| device.id),
        Some(phone.device.id)
    );
    assert!(seen.device.unwrap().last_used.is_some());

    // Devices are this computer's to manage.
    let mint = AddDevice {
        name: "Another".into(),
        read_only: None,
    };
    let refused = as_phone(d.client.post(d.url("/access/devices")))
        .json(&mint)
        .send()
        .await
        .unwrap();
    let (status, refused) = error_of(refused).await;
    assert_eq!(
        (status, refused.reason),
        (StatusCode::FORBIDDEN, Reason::OwnerOnly)
    );

    let empty = AddDevice {
        name: " ".into(),
        read_only: None,
    };
    let refused = d.post("/access/devices").json(&empty).send().await.unwrap();
    assert_eq!(error_of(refused).await.1.reason, Reason::InvalidDeviceName);

    // The events stream closes once the device is removed.
    let protocols = format!("{EVENTS_PROTOCOL}, {TOKEN_PROTOCOL_PREFIX}{}", phone.token);
    let (mut events, _) = d.events(&protocols).await.unwrap();
    let devices: Vec<Device> = d
        .get("/access/devices")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(devices.len(), 1);
    let path = format!("/access/devices/{}", phone.device.id);
    let removed = d
        .client
        .delete(d.url(&path))
        .bearer_auth(TOKEN)
        .send()
        .await;
    assert_eq!(removed.unwrap().status(), StatusCode::NO_CONTENT);
    let closed = tokio::time::timeout(TIMEOUT, async {
        loop {
            match events.next().await {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
    });
    closed.await.expect("the events stream stayed open");
    let jobs = as_phone(d.client.get(d.url("/jobs"))).send().await.unwrap();
    assert_eq!(jobs.status(), StatusCode::UNAUTHORIZED);
    let again = d
        .client
        .delete(d.url(&path))
        .bearer_auth(TOKEN)
        .send()
        .await;
    let (status, refused) = error_of(again.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        refused.reason,
        Reason::DeviceNotFound {
            id: phone.device.id
        }
    );
}

#[tokio::test]
async fn a_read_only_device_only_looks() {
    let d = start().await;
    let id = d.add("file.bin").await;
    let tv = d.add_device("TV", true).await;
    let as_tv = |request: reqwest::RequestBuilder| request.bearer_auth(&tv.token);

    let listed = as_tv(d.client.get(d.url("/jobs"))).send().await.unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let play = as_tv(d.client.post(d.url(&format!("/jobs/{id}/stream"))));
    assert_eq!(play.send().await.unwrap().status(), StatusCode::OK);
    let preview = PreviewLinks {
        text: format!("{}/file.bin", d.files),
        dest_dir: None,
    };
    let previewed = as_tv(d.client.post(d.url("/jobs/preview"))).json(&preview);
    assert_eq!(previewed.send().await.unwrap().status(), StatusCode::OK);

    let added = as_tv(d.client.post(d.url("/jobs")))
        .json(&d.new_job("file.bin"))
        .send()
        .await
        .unwrap();
    let (status, refused) = error_of(added).await;
    assert_eq!(
        (status, refused.reason),
        (StatusCode::FORBIDDEN, Reason::ReadOnly)
    );
    let paused = as_tv(d.client.post(d.url(&format!("/jobs/{id}/pause"))));
    assert_eq!(paused.send().await.unwrap().status(), StatusCode::FORBIDDEN);
    let removed = as_tv(d.client.delete(d.url(&format!("/jobs/{id}"))));
    assert_eq!(
        removed.send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let change = SettingsChange {
        max_concurrent: Some(1),
        ..SettingsChange::default()
    };
    let changed = as_tv(d.client.patch(d.url("/settings"))).json(&change);
    assert_eq!(
        changed.send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn a_new_token_closes_the_old_one_s_streams_without_a_request() {
    let folder = tempfile::tempdir().unwrap();
    let token_file = folder.path().join("token");
    std::fs::write(&token_file, TOKEN).unwrap();
    let mut config = AccessConfig::new(Token::new(TOKEN));
    config.token_file = Some(token_file.clone());
    let d = start_with_access(UiSource::Embedded, Access::open(config).unwrap()).await;
    let protocols = format!("{EVENTS_PROTOCOL}, {TOKEN_PROTOCOL_PREFIX}{TOKEN}");
    let (mut events, _) = d.events(&protocols).await.unwrap();

    // As `ottie token --new` does; nobody sends a request afterwards.
    std::fs::write(&token_file, "a-new-token").unwrap();
    let closed = tokio::time::timeout(TIMEOUT, async {
        loop {
            match events.next().await {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
    });
    closed
        .await
        .expect("the old token's events stream stayed open");
}

#[tokio::test]
async fn devices_look_for_files_only_where_they_may_write() {
    let d = start().await;
    let tv = d.add_device("TV", true).await;
    let private = tempfile::tempdir().unwrap();
    std::fs::write(private.path().join("secret.key"), "x").unwrap();
    let preview = PreviewLinks {
        text: format!("{}/secret.key", d.files),
        dest_dir: Some(private.path().to_string_lossy().into_owned()),
    };

    let looked = d
        .client
        .post(d.url("/jobs/preview"))
        .bearer_auth(&tv.token)
        .json(&preview)
        .send()
        .await;
    let (status, refused) = error_of(looked.unwrap()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(matches!(refused.reason, Reason::FolderNotAllowed { .. }));
    // This computer may look anywhere.
    let looked = d.post("/jobs/preview").json(&preview).send().await.unwrap();
    let links = looked.json::<Preview>().await.unwrap().links;
    assert!(matches!(&links[..], [FoundLink::Link { exists: true, .. }]));
}

#[tokio::test]
async fn devices_write_only_where_they_may() {
    let d = start().await;
    let laptop = d.add_device("Laptop", false).await;
    let as_laptop = |request: reqwest::RequestBuilder| request.bearer_auth(&laptop.token);
    let inside = d.dir.path().join("Films");
    let elsewhere = tempfile::tempdir().unwrap();

    // Nowhere yet: General has no folder in these tests, and none is listed.
    let mut job = d.new_job("file.bin");
    job.dest_dir = Some(inside.to_string_lossy().into_owned());
    let refused = as_laptop(d.client.post(d.url("/jobs")))
        .json(&job)
        .send()
        .await;
    let (status, refused) = error_of(refused.unwrap()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        refused.reason,
        Reason::FolderNotAllowed {
            folder: inside.to_string_lossy().into_owned()
        }
    );
    // This computer isn't limited.
    let added = d.post("/jobs").json(&job).send().await.unwrap();
    assert_eq!(added.status(), StatusCode::CREATED);

    // Listed in Settings, by this computer only.
    let list = FolderList {
        folders: vec![d.dir.path().to_string_lossy().into_owned()],
    };
    let by_laptop = as_laptop(d.client.put(d.url("/access/folders"))).json(&list);
    assert_eq!(
        by_laptop.send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let listed = d
        .client
        .put(d.url("/access/folders"))
        .bearer_auth(TOKEN)
        .json(&list);
    let listed: AllowedFolders = listed.send().await.unwrap().json().await.unwrap();
    assert_eq!(listed.chosen, list.folders);
    let added = as_laptop(d.client.post(d.url("/jobs")))
        .json(&job)
        .send()
        .await;
    assert_eq!(added.unwrap().status(), StatusCode::CREATED);
    job.dest_dir = Some(elsewhere.path().to_string_lossy().into_owned());
    let refused = as_laptop(d.client.post(d.url("/jobs")))
        .json(&job)
        .send()
        .await;
    assert_eq!(refused.unwrap().status(), StatusCode::FORBIDDEN);

    // A category's folder is allowed once this computer chose it.
    let category = CategoryBody {
        name: "Elsewhere".into(),
        folder: job.dest_dir.clone(),
        extensions: vec![],
        speed_limit: 0,
        max_concurrent: 0,
    };
    let by_laptop = as_laptop(d.client.post(d.url("/categories"))).json(&category);
    assert_eq!(
        by_laptop.send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let by_owner = d.post("/categories").json(&category).send().await.unwrap();
    assert_eq!(by_owner.status(), StatusCode::CREATED);
    let added = as_laptop(d.client.post(d.url("/jobs")))
        .json(&job)
        .send()
        .await;
    assert_eq!(added.unwrap().status(), StatusCode::CREATED);
}

#[tokio::test]
async fn certificates_are_trusted_and_forgotten() {
    let d = start().await;
    let list = || async {
        let response = d.get("/certificates").send().await.unwrap();
        response.json::<Vec<TrustedCertificate>>().await.unwrap()
    };
    assert_eq!(list().await, []);

    let bad = TrustCertificate {
        host: "nas.local".into(),
        fingerprint: "abc".into(),
    };
    let refused = d.post("/certificates").json(&bad).send().await.unwrap();
    let (status, refused) = error_of(refused).await;
    assert_eq!(
        (status, refused.reason),
        (StatusCode::BAD_REQUEST, Reason::InvalidCertificate)
    );

    // As a certificate problem gives them, or written by hand.
    let body = TrustCertificate {
        host: "NAS.local".into(),
        fingerprint: vec!["AB"; 32].join(":"),
    };
    let trusted = d.post("/certificates").json(&body).send().await.unwrap();
    assert_eq!(trusted.status(), StatusCode::CREATED);
    let trusted: TrustedCertificate = trusted.json().await.unwrap();
    assert_eq!(
        (trusted.host.as_str(), trusted.fingerprint.clone()),
        ("nas.local", "ab".repeat(32))
    );
    let again = d.post("/certificates").json(&body).send().await.unwrap();
    assert_eq!(again.json::<TrustedCertificate>().await.unwrap(), trusted);
    assert_eq!(list().await, std::slice::from_ref(&trusted));

    let tv = d.add_device("TV", true).await;
    let as_tv = d.client.post(d.url("/certificates")).bearer_auth(&tv.token);
    let as_tv = as_tv.json(&body).send().await.unwrap();
    assert_eq!(as_tv.status(), StatusCode::FORBIDDEN);

    let path = format!("/certificates/{}", trusted.id);
    let removed = d.client.delete(d.url(&path)).bearer_auth(TOKEN);
    assert_eq!(
        removed.send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    let again = d.client.delete(d.url(&path)).bearer_auth(TOKEN);
    let (status, refused) = error_of(again.send().await.unwrap()).await;
    assert_eq!(
        (status, refused.reason),
        (
            StatusCode::NOT_FOUND,
            Reason::CertificateNotFound { id: trusted.id }
        )
    );
    assert_eq!(list().await, []);
}

#[tokio::test]
async fn an_ftp_link_s_password_stays_with_the_daemon() {
    let d = start().await;
    // Nothing answers there: it fails and retries, which is fine here.
    let mut job = d.new_job("unused");
    job.url = "ftp://me:s3cret@127.0.0.1:9/pub/file.bin".into();
    let response = d.post("/jobs").json(&job).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = response.json::<Added>().await.unwrap().id;

    let shown = d.job(&id).await;
    assert_eq!(shown.url, "ftp://me@127.0.0.1:9/pub/file.bin");
    let listed = d.get("/jobs").send().await.unwrap().text().await.unwrap();
    assert!(!listed.contains("s3cret"), "{listed}");
}
