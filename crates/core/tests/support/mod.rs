//! In-process HTTP server for engine tests, with fault injection.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, COOKIE, ETAG, IF_RANGE,
    LOCATION, RANGE, REFERER, RETRY_AFTER, USER_AGENT,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::get;
use futures_util::Stream;
use reqwest::Url;

const CHUNK: usize = 16 * 1024;

/// What the server does. Change it between requests with [`TestServer::configure`].
pub struct Behavior {
    pub body: Bytes,
    pub etag: Option<String>,
    /// Honour `Range` requests. When false, always answer 200 with the whole file.
    pub ranges: bool,
    /// Pause before each 16 KiB chunk.
    pub chunk_delay: Option<Duration>,
    /// Pause this long instead before chunks at or after this file offset,
    /// so one part of the file is slow.
    pub slow_from: Option<(usize, Duration)>,
    /// Kill the connection after this many body bytes, once (the next request is normal).
    pub drop_after: Option<usize>,
    /// Sent as is, so tests can include raw non-ASCII bytes.
    pub content_disposition: Option<Vec<u8>>,
    /// Answer the next `.1` requests with status `.0` instead (`usize::MAX`: always).
    pub fail_next: Option<(StatusCode, usize)>,
    /// The same, only for requests of the whole file (no `Range`, or
    /// `bytes=0-`): a fresh start or a restart, not a range or a resume.
    pub fail_from_start: Option<(StatusCode, usize)>,
    /// `Retry-After` seconds sent with those failures.
    pub retry_after: Option<u64>,
    /// After this many body bytes, stop sending without closing, once.
    pub stall_after: Option<usize>,
    /// Never answer the next this many requests, not even with headers;
    /// their connections stay open.
    pub silent_next: usize,
}

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub range: Option<String>,
    pub if_range: Option<String>,
    pub cookie: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
}

struct Shared {
    behavior: Behavior,
    requests: Vec<RecordedRequest>,
    /// Every client connection so far, by its address.
    connections: HashSet<SocketAddr>,
    /// Connections that went silent in [`TestServer::unplug`].
    dead: HashSet<SocketAddr>,
    /// Requests that arrived on one of them (and got no answer).
    on_dead: usize,
    /// By file name: answers the client stopped reading before their end.
    abandoned: HashMap<String, usize>,
}

type AppState = Arc<Mutex<Shared>>;

pub struct TestServer {
    addr: SocketAddr,
    state: AppState,
}

impl TestServer {
    /// Serves `body` at `/files/<any name>`, with ETag `"v1"` and range support.
    pub async fn start(body: Vec<u8>) -> Self {
        let state = Arc::new(Mutex::new(Shared {
            behavior: Behavior {
                body: Bytes::from(body),
                etag: Some("\"v1\"".to_owned()),
                ranges: true,
                chunk_delay: None,
                slow_from: None,
                drop_after: None,
                content_disposition: None,
                fail_next: None,
                fail_from_start: None,
                retry_after: None,
                stall_after: None,
                silent_next: 0,
            },
            requests: Vec::new(),
            connections: HashSet::new(),
            dead: HashSet::new(),
            on_dead: 0,
            abandoned: HashMap::new(),
        }));
        let app = Router::new()
            .route("/files/{name}", get(serve_file))
            .route("/redirect-to-ftp", get(redirect_to_ftp))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = app.into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, state }
    }

    pub fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://{}{path}", self.addr)).unwrap()
    }

    pub fn configure(&self, change: impl FnOnce(&mut Behavior)) {
        change(&mut self.state.lock().unwrap().behavior);
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    /// As when the network changes (a VPN goes up or down): every connection
    /// open now goes silent without closing, mid-answer or idle. New ones work.
    pub fn unplug(&self) {
        let mut shared = self.state.lock().unwrap();
        shared.dead = shared.connections.clone();
    }

    /// How many requests came over a connection that [`TestServer::unplug`]
    /// had silenced: a client that reuses one waits until it gives up.
    pub fn requests_on_dead_connections(&self) -> usize {
        self.state.lock().unwrap().on_dead
    }

    /// How many answers for `/files/<name>` the client let go of before
    /// their end (dropped connections injected here count too).
    pub fn abandoned(&self, name: &str) -> usize {
        let shared = self.state.lock().unwrap();
        shared.abandoned.get(name).copied().unwrap_or(0)
    }
}

async fn serve_file(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let dead = {
        let mut shared = state.lock().unwrap();
        shared.connections.insert(peer);
        let dead = shared.dead.contains(&peer);
        shared.on_dead += usize::from(dead);
        dead
    };
    if dead {
        std::future::pending::<()>().await;
    }
    let text = |name| headers.get(name).map(|v| v.to_str().unwrap().to_owned());
    let range = text(RANGE);
    let if_range = text(IF_RANGE);
    let (body, etag, ranges, pacing, drop_after, disposition, failure, silent) = {
        let mut shared = state.lock().unwrap();
        shared.requests.push(RecordedRequest {
            range: range.clone(),
            if_range: if_range.clone(),
            cookie: text(COOKIE),
            referer: text(REFERER),
            user_agent: text(USER_AGENT),
        });
        let b = &mut shared.behavior;
        let silent = b.silent_next > 0;
        b.silent_next = b.silent_next.saturating_sub(1);
        let drop_after = b.drop_after.take();
        let pacing = Pacing {
            delay: b.chunk_delay,
            slow_from: b.slow_from,
            stall_after: b.stall_after.take(),
        };
        let disposition = b.content_disposition.clone();
        let from_start = matches!(range.as_deref(), None | Some("bytes=0-"));
        let failing = match (&mut b.fail_next, &mut b.fail_from_start) {
            (Some((status, left)), _) if *left > 0 => Some((*status, left)),
            (_, Some((status, left))) if *left > 0 && from_start => Some((*status, left)),
            _ => None,
        };
        let failure = failing.map(|(status, left)| {
            *left = left.saturating_sub(1);
            (status, b.retry_after)
        });
        (
            b.body.clone(),
            b.etag.clone(),
            b.ranges,
            pacing,
            drop_after,
            disposition,
            failure,
            silent,
        )
    };
    if silent {
        std::future::pending::<()>().await;
    }
    if let Some((status, retry_after)) = failure {
        let mut response = Response::builder().status(status);
        if let Some(secs) = retry_after {
            response = response.header(RETRY_AFTER, secs);
        }
        return response.body(Body::empty()).unwrap();
    }

    let total = body.len();
    let validator_ok = if_range.is_none() || if_range == etag;
    let requested = match range.as_deref().and_then(|r| r.strip_prefix("bytes=")) {
        Some(spec) if ranges && validator_ok => {
            // "bytes=N-" or "bytes=N-M" (M inclusive).
            let (start, last) = spec.split_once('-').unwrap();
            let start: usize = start.parse().unwrap();
            let end = match last {
                "" => total,
                last => (last.parse::<usize>().unwrap() + 1).min(total),
            };
            if start >= total {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(CONTENT_RANGE, format!("bytes */{total}"))
                    .body(Body::empty())
                    .unwrap();
            }
            Some((start, end))
        }
        _ => None,
    };
    let (start, end) = requested.unwrap_or((0, total));

    let mut response = Response::builder()
        .header(ACCEPT_RANGES, if ranges { "bytes" } else { "none" })
        .header(CONTENT_LENGTH, end - start);
    if let Some(etag) = &etag {
        response = response.header(ETAG, etag);
    }
    if let Some(disposition) = disposition {
        let value = HeaderValue::from_bytes(&disposition).unwrap();
        response = response.header(CONTENT_DISPOSITION, value);
    }
    if requested.is_some() {
        response = response
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_RANGE, format!("bytes {start}-{}/{total}", end - 1));
    }
    let data = body.slice(start..end);
    response
        .body(Body::from_stream(chunked(
            data,
            start,
            pacing,
            drop_after,
            Watch {
                state,
                peer,
                name,
                finished: false,
            },
        )))
        .unwrap()
}

#[derive(Clone, Copy)]
struct Pacing {
    delay: Option<Duration>,
    slow_from: Option<(usize, Duration)>,
    stall_after: Option<usize>,
}

async fn redirect_to_ftp() -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(LOCATION, "ftp://127.0.0.1/file.bin")
        .body(Body::empty())
        .unwrap()
}

/// One answer being sent: goes silent if its connection is unplugged, and
/// counts as abandoned if dropped before its end.
struct Watch {
    state: AppState,
    peer: SocketAddr,
    name: String,
    finished: bool,
}

impl Drop for Watch {
    fn drop(&mut self) {
        if !self.finished {
            let mut shared = self.state.lock().unwrap();
            *shared.abandoned.entry(self.name.clone()).or_default() += 1;
        }
    }
}

/// Sends `data` (which starts at file offset `offset`) in 16 KiB chunks,
/// optionally slowly, optionally failing midway.
fn chunked(
    data: Bytes,
    offset: usize,
    pacing: Pacing,
    drop_after: Option<usize>,
    watch: Watch,
) -> impl Stream<Item = io::Result<Bytes>> {
    futures_util::stream::unfold((data, 0usize, watch), move |(mut data, sent, mut watch)| {
        async move {
            if data.is_empty() {
                return None;
            }
            let dead = watch.state.lock().unwrap().dead.contains(&watch.peer);
            if dead {
                std::future::pending::<()>().await;
            }
            let mut len = CHUNK.min(data.len());
            if let Some(limit) = drop_after {
                if sent >= limit {
                    let err = io::Error::other("injected connection drop");
                    return Some((Err(err), (Bytes::new(), sent, watch)));
                }
                len = len.min(limit - sent);
            }
            if pacing.stall_after.is_some_and(|limit| sent >= limit) {
                // Keep the connection open but send nothing more.
                std::future::pending::<()>().await;
            }
            let delay = match pacing.slow_from {
                Some((from, slow)) if offset + sent >= from => Some(slow),
                _ => pacing.delay,
            };
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let chunk = data.split_to(len);
            // With a known length, the server may never ask past the last chunk.
            watch.finished = data.is_empty();
            Some((Ok(chunk), (data, sent + len, watch)))
        }
    })
}

/// Deterministic, non-repeating test content.
pub fn test_data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 56) as u8
        })
        .collect()
}
