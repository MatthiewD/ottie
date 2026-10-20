//! Play while downloading (docs/decisions/0029). `POST /jobs/{id}/stream`
//! hands out a link, `GET /stream/{ticket}/{name}`, that serves the job's
//! file (complete or not) with `Range` support to players that can't send
//! the token: VLC, mpv, a browser's `<video>`. A link covers one job, only
//! reads, and expires 12 hours after it was last used. Links live in memory,
//! so a restarted daemon hands out new ones.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HeaderMap,
    RANGE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use ottie_core::{Error, JobId, Reader, Url};
use tokio_util::sync::CancellationToken;

use crate::AppState;
use crate::error::{Failure, Param};
use crate::types::{ApiError, StreamLink};

/// A link stays valid this long after it was last used.
const LIFETIME: Duration = Duration::from_secs(12 * 3600);
/// Read from the file this much at a time.
const CHUNK: usize = 256 * 1024;

/// The play links handed out so far, by ticket (32 random bytes as hex).
#[derive(Default)]
pub(crate) struct Links(Mutex<HashMap<String, Link>>);

struct Link {
    job: JobId,
    expires: Instant,
}

impl Links {
    /// The job's ticket: the one it already has, else a new one.
    fn ticket_for(&self, job: JobId) -> Result<String, Failure> {
        let mut links = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        links.retain(|_, link| now < link.expires);
        if let Some((ticket, link)) = links.iter_mut().find(|(_, link)| link.job == job) {
            link.expires = now + LIFETIME;
            return Ok(ticket.clone());
        }
        let ticket = random_ticket()?;
        let link = Link {
            job,
            expires: now + LIFETIME,
        };
        links.insert(ticket.clone(), link);
        Ok(ticket)
    }

    /// The job behind a ticket that hasn't expired. Using it keeps it valid longer.
    fn job(&self, ticket: &str) -> Option<JobId> {
        let mut links = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let link = links.get_mut(ticket)?;
        if now >= link.expires {
            links.remove(ticket);
            return None;
        }
        link.expires = now + LIFETIME;
        Some(link.job)
    }
}

fn random_ticket() -> Result<String, Failure> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| {
        tracing::error!(error = %err, "no random numbers available");
        Failure::from(Error::Io(io::Error::other("no random numbers available")))
    })?;
    let mut hex = String::with_capacity(64);
    for byte in bytes {
        // Writing to a String can't fail.
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// A download's play link, as a path on this server: the same one while it
/// is in use. It ends with the file name once known, so players can show it.
#[utoipa::path(
    post,
    path = "/jobs/{id}/stream",
    operation_id = "getPlayLink",
    tag = "play",
    params(("id" = String, Path, description = "Job ID, like `2026.10.13-14.28.59-1`")),
    responses(
        (status = OK, description = "Its play link", body = StreamLink),
        (status = BAD_REQUEST, description = "`invalidJobId`", body = ApiError),
        (status = NOT_FOUND, description = "`jobNotFound`", body = ApiError),
    )
)]
pub(crate) async fn link(
    State(state): State<AppState>,
    Param(id): Param<String>,
) -> Result<Json<StreamLink>, Failure> {
    let id = id.parse::<JobId>()?;
    let job = state.manager.job(id).await?;
    let ticket = state.links.ticket_for(id)?;
    // Any base does: only the path is kept, with each part percent-encoded.
    let mut url = Url::parse("http://ottie/api/v1/stream").map_err(|_| Failure::internal())?;
    if let Ok(mut path) = url.path_segments_mut() {
        path.push(&ticket);
        if let Some(name) = &job.file_name {
            path.push(name);
        }
    }
    Ok(Json(StreamLink {
        url: url.path().to_owned(),
    }))
}

/// The same as `GET /stream/{ticket}/{name}`, without the name.
#[utoipa::path(
    get,
    path = "/stream/{ticket}",
    operation_id = "playUnnamed",
    tag = "play",
    params(("ticket" = String, Path, description = "From the play link")),
    security(()),
    responses(
        (status = OK, description = "The whole file", content_type = "application/octet-stream"),
        (status = NOT_FOUND, description = "`noSuchLink`", body = ApiError),
    )
)]
pub(crate) async fn serve(
    State(state): State<AppState>,
    Param(ticket): Param<String>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    play(state, &ticket, &headers).await
}

/// Plays a download, complete or not: no token, the ticket in the path is
/// the permission. Bytes that haven't arrived yet are waited for (also while
/// the download is queued or paused). The answer breaks off if the download
/// fails, is cancelled or removed, or starts over. `Range` works when the
/// size is known (`Accept-Ranges: bytes`).
#[utoipa::path(
    get,
    path = "/stream/{ticket}/{name}",
    operation_id = "play",
    tag = "play",
    params(
        ("ticket" = String, Path, description = "From the play link"),
        ("name" = String, Path, description = "The file name; only for players to show"),
        ("Range" = Option<String>, Header, description = "One range, like `bytes=1000-` or `bytes=-500`"),
    ),
    security(()),
    responses(
        (status = OK, description = "The whole file, as it arrives", content_type = "application/octet-stream"),
        (status = PARTIAL_CONTENT, description = "The range asked for", content_type = "application/octet-stream"),
        (status = NOT_FOUND, description = "`noSuchLink` (unknown or expired) or `jobNotFound`", body = ApiError),
        (status = CONFLICT, description = "`downloadStopped`: it failed or was cancelled before it had a file", body = ApiError),
        (status = RANGE_NOT_SATISFIABLE, description = "The range starts past the end (`Content-Range: bytes */<size>`)"),
    )
)]
pub(crate) async fn serve_named(
    State(state): State<AppState>,
    Param((ticket, _name)): Param<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    play(state, &ticket, &headers).await
}

async fn play(state: AppState, ticket: &str, headers: &HeaderMap) -> Result<Response, Failure> {
    let id = state.links.job(ticket).ok_or_else(Failure::no_such_link)?;
    let asked = Instant::now();
    // Waits for the download's first response if it hasn't come yet.
    let (reader, opened) = tokio::select! {
        opened = Reader::open(&state.manager, id) => opened?,
        () = state.shutdown.cancelled() => return Err(Error::ManagerStopped.into()),
    };
    let mime = mime_guess::from_path(&opened.file_name).first_or_octet_stream();
    let mut response = Response::builder()
        .header(CONTENT_TYPE, mime.as_ref())
        // A downloaded page or SVG must not run scripts as this server (it
        // could take the token from the UI): no scripts, its own origin.
        .header(CONTENT_SECURITY_POLICY, "sandbox")
        .header(X_CONTENT_TYPE_OPTIONS, "nosniff");

    let Some(total) = opened.total else {
        // Size unknown: no ranges, just everything until the download ends.
        response = response.header(ACCEPT_RANGES, "none");
        tracing::debug!(%id, "play: the whole file, size unknown");
        let body = body(reader, 0, None, asked, state.shutdown);
        return response.body(body).map_err(|_| Failure::internal());
    };
    response = response.header(ACCEPT_RANGES, "bytes");
    let wanted = headers
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| requested(value, total));
    let (status, start, end) = match wanted {
        None => (StatusCode::OK, 0, total),
        Some(Ok((start, end))) => {
            let range = format!("bytes {start}-{}/{total}", end - 1);
            response = response.header(CONTENT_RANGE, range);
            (StatusCode::PARTIAL_CONTENT, start, end)
        }
        Some(Err(Unsatisfiable)) => {
            let range = HeaderValue::from_str(&format!("bytes */{total}"))
                .map_err(|_| Failure::internal())?;
            let mut refused = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            refused.headers_mut().insert(CONTENT_RANGE, range);
            return Ok(refused);
        }
    };
    tracing::debug!(%id, start, end, total, "play: bytes asked for");
    response
        .status(status)
        .header(CONTENT_LENGTH, end - start)
        .body(body(reader, start, Some(end), asked, state.shutdown))
        .map_err(|_| Failure::internal())
}

/// The file from `start` up to `end` (else its end), as it arrives. An
/// error ends the answer early, so the player sees a broken transfer rather
/// than wrong bytes. `asked` is when the request came, to log how long its
/// first bytes took.
fn body(
    reader: Reader,
    start: u64,
    end: Option<u64>,
    asked: Instant,
    shutdown: CancellationToken,
) -> Body {
    let state = (reader, start, shutdown);
    Body::from_stream(futures_util::stream::unfold(
        state,
        move |(mut reader, pos, shutdown)| async move {
            let max = end.map_or(CHUNK, |end| {
                usize::try_from(end - pos).map_or(CHUNK, |left| left.min(CHUNK))
            });
            if max == 0 {
                return None;
            }
            let read = tokio::select! {
                read = reader.read(pos, max) => read,
                () = shutdown.cancelled() => Err(Error::ManagerStopped),
            };
            match read {
                // The end of the file: fine without a size, an error with
                // one (the file is shorter than announced).
                Ok(bytes) if bytes.is_empty() => end.map(|_| {
                    let short = io::Error::from(io::ErrorKind::UnexpectedEof);
                    (Err(short), (reader, pos, shutdown))
                }),
                Ok(bytes) => {
                    if pos == start {
                        let waited = asked.elapsed();
                        tracing::debug!(start, ?waited, "play: first bytes sent");
                    }
                    let next = pos + bytes.len() as u64;
                    Some((Ok(bytes), (reader, next, shutdown)))
                }
                Err(err) => Some((Err(io::Error::other(err)), (reader, pos, shutdown))),
            }
        },
    ))
}

/// A `Range` that can't be served: it starts past the end of the file.
#[derive(Debug, PartialEq, Eq)]
struct Unsatisfiable;

/// The byte range a `Range` header asks for, as `start..end` (end
/// exclusive), cut to the file. `None` for anything but a single
/// `bytes=` range: the whole file is then sent, as HTTP allows.
fn requested(header: &str, total: u64) -> Option<Result<(u64, u64), Unsatisfiable>> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        // The last `n` bytes.
        let n: u64 = last.parse().ok()?;
        if n == 0 || total == 0 {
            return Some(Err(Unsatisfiable));
        }
        return Some(Ok((total.saturating_sub(n), total)));
    }
    let start: u64 = first.parse().ok()?;
    let end = if last.is_empty() {
        total
    } else {
        let last: u64 = last.parse().ok()?;
        if last < start {
            return None;
        }
        last.saturating_add(1).min(total)
    };
    if start >= total {
        return Some(Err(Unsatisfiable));
    }
    Some(Ok((start, end)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_ranges_players_send() {
        assert_eq!(requested("bytes=0-", 1000), Some(Ok((0, 1000))));
        assert_eq!(requested("bytes=100-199", 1000), Some(Ok((100, 200))));
        assert_eq!(requested("bytes=900-5000", 1000), Some(Ok((900, 1000))));
        assert_eq!(requested("bytes=-100", 1000), Some(Ok((900, 1000))));
        assert_eq!(requested("bytes=-5000", 1000), Some(Ok((0, 1000))));
    }

    #[test]
    fn a_range_past_the_end_cant_be_served() {
        assert_eq!(requested("bytes=1000-", 1000), Some(Err(Unsatisfiable)));
        assert_eq!(requested("bytes=-0", 1000), Some(Err(Unsatisfiable)));
        assert_eq!(requested("bytes=0-", 0), Some(Err(Unsatisfiable)));
    }

    #[test]
    fn anything_else_gets_the_whole_file() {
        assert_eq!(requested("bytes=0-1,5-9", 1000), None);
        assert_eq!(requested("bytes=9-1", 1000), None);
        assert_eq!(requested("items=0-1", 1000), None);
        assert_eq!(requested("bytes=x-", 1000), None);
    }

    #[test]
    fn a_link_is_reused_and_expires() {
        let links = Links::default();
        let job: JobId = "2026.10.13-08.00.00-1".parse().unwrap();
        let ticket = links.ticket_for(job).unwrap();
        assert_eq!(ticket.len(), 64);
        assert_eq!(links.ticket_for(job).unwrap(), ticket);
        assert_eq!(links.job(&ticket), Some(job));
        assert_eq!(links.job("nope"), None);

        let mut map = links.0.lock().unwrap();
        map.get_mut(&ticket).unwrap().expires = Instant::now();
        drop(map);
        assert_eq!(links.job(&ticket), None);
        assert_ne!(links.ticket_for(job).unwrap(), ticket);
    }
}
