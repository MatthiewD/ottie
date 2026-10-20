//! HTTP(S) transfers with pause/resume: a single connection, or several in
//! parallel (see `segmented.rs`) when the server supports ranges and the file
//! is big enough.

use std::io::{self, SeekFrom};
use std::path::Path;
use std::time::Duration;

use reqwest::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_RANGE, ETAG, HeaderMap, IF_RANGE,
    LAST_MODIFIED, RANGE, RETRY_AFTER,
};
use reqwest::{Client, Response, StatusCode, Url, redirect};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncSeekExt;
use tokio_util::sync::CancellationToken;

use crate::download::{
    Download, Event, Outcome, RequestHeaders, RestartReason, Validator, check_http_scheme,
};
use crate::error::Error;
use crate::filename;
use crate::playback::Playback;
use crate::segmented::{self, Segmented};
use crate::throttle::Throttle;
use crate::transfer::{
    self, Body, Same, Source, complete, redact, restarting, single, splittable, started,
};

const MAX_REDIRECTS: usize = 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// An HTTP/2 connection is pinged after this long without hearing from the
/// server, and dropped if the answer takes longer than this too: one that
/// died silently (the network changed) is noticed sooner than a stall.
const PING_AFTER: Duration = Duration::from_secs(10);

/// How downloads are split across connections, and how long a server may
/// keep quiet. For FTP too, despite the name.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Connections per download. 1 disables splitting.
    pub connections: usize,
    /// Never split a range into pieces smaller than this many bytes.
    pub min_split: u64,
    /// A server that sends nothing for this long, while we're waiting for
    /// it, counts as a dropped connection (and the download is retried).
    pub stall_timeout: Duration,
    /// In streaming mode, each connection takes a piece of the file that
    /// it fetches in about this long at the download's speed, and at least
    /// 4 × `min_split` bytes. Longer means fewer requests, shorter a file
    /// more in order.
    pub piece_time: Duration,
    /// Over several connections, a connection the speed limit holds back
    /// this long lets go of its request, and asks again once there is
    /// bandwidth: servers drop a request that isn't read for long.
    pub starved_after: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            connections: 4,
            min_split: 1024 * 1024,
            stall_timeout: Duration::from_secs(60),
            piece_time: Duration::from_secs(2),
            starved_after: Duration::from_secs(20),
        }
    }
}

/// HTTP client with our redirect rules: at most 10 hops, http/https only.
/// It keeps connections open to use them again, so the manager's downloads
/// take a new one for each run (see [`crate::HttpBackend`]).
pub fn build_client() -> Result<Client, Error> {
    client_with(&RequestHeaders::default())
}

/// [`build_client`], sending `headers` with every request: those a browser
/// sent with the download (docs/decisions/0055). On a redirect to another
/// server, reqwest leaves the cookies out.
pub(crate) fn client_with(headers: &RequestHeaders) -> Result<Client, Error> {
    let policy = redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if check_http_scheme(attempt.url()).is_err() {
            let scheme = attempt.url().scheme().to_owned();
            attempt.error(format!("redirect to unsupported scheme `{scheme}`"))
        } else {
            attempt.follow()
        }
    });
    Ok(Client::builder()
        .redirect(policy)
        .connect_timeout(CONNECT_TIMEOUT)
        .http2_keep_alive_interval(PING_AFTER)
        .http2_keep_alive_timeout(PING_AFTER)
        .http2_keep_alive_while_idle(true)
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        // After `user_agent`: the browser's replaces Ottie's.
        .default_headers(headers.header_map())
        .build()?)
}

/// Downloads into `<name>.part.ottie` until the file is complete or `cancel` fires,
/// then renames it. Call again with the same `Download` to resume.
///
/// `dl` is kept up to date as bytes arrive, so after an error or a pause it
/// describes what's on disk.
pub async fn run(
    client: &Client,
    config: &HttpConfig,
    dl: &mut Download,
    throttle: &Throttle,
    cancel: &CancellationToken,
    on_event: impl FnMut(Event),
) -> Result<Outcome, Error> {
    let env = Env {
        client,
        config,
        throttle,
        playback: &Playback::none(),
        cancel,
    };
    run_in(&env, dl, on_event).await
}

/// [`run`], for a job of the manager: players may be reading it.
pub(crate) async fn run_in(
    env: &Env<'_>,
    dl: &mut Download,
    mut on_event: impl FnMut(Event),
) -> Result<Outcome, Error> {
    let config = env.config;
    let mut opened = match (dl.segments.is_empty(), dl.file_name.clone(), dl.part_path()) {
        (false, Some(name), Some(part)) => {
            match resume_segmented(env, dl, &name, &part, &mut on_event).await? {
                Resumed::Stopped(outcome) => return Ok(outcome),
                Resumed::Restart(reason) => {
                    restarting(dl, reason, &mut on_event).await?;
                    fresh(env, dl, name, &part).await?
                }
            }
        }
        _ => {
            dl.segments.clear();
            open(env, dl, &mut on_event).await?
        }
    };

    // Loops at most twice: once more if the file changes on the server
    // in the middle of a multi-connection download.
    let mut restarted_mid_run = false;
    loop {
        let Opened { name, body } = opened;
        let Some((response, file)) = body else {
            started(dl, &name, &mut on_event);
            return complete(dl, &name).await;
        };
        let ranges = response.status() == StatusCode::PARTIAL_CONTENT;
        let Some(total) = splittable(config, dl, ranges) else {
            started(dl, &name, &mut on_event);
            tracing::debug!(%name, "one connection");
            let body = Body::Http(response);
            return single(&env.common(), body, file, dl, &name, &mut on_event).await;
        };

        dl.segments = segmented::plan(total, config);
        started(dl, &name, &mut on_event);
        // The database has the new plan before the file has its full length:
        // a crash in between would leave the old map over a reset file, and
        // its finished ranges would never be fetched again (0059).
        transfer::persisted(dl).await?;
        // Full size up front, so every connection can write at its offset
        // and a later resume can tell from the length that the file is intact.
        file.set_len(total).await?;
        drop(file);
        let part = dl.dest_dir.join(filename::part_name(&name));
        let first = Some(Body::Http(response));
        let source = env.source(dl);
        match segmented::run(&env.common(), source, dl, &part, first, &mut on_event).await? {
            Segmented::Finished => return complete(dl, &name).await,
            Segmented::Cancelled => return Ok(Outcome::Paused),
            Segmented::Restart(reason) if !restarted_mid_run => {
                restarted_mid_run = true;
                restarting(dl, reason, &mut on_event).await?;
                opened = fresh(env, dl, name, &part).await?;
            }
            Segmented::Restart(_) => return Err(Error::RemoteUnstable),
        }
    }
}

/// What every step of one run shares: [`transfer::Env`] and the client.
pub(crate) struct Env<'a> {
    pub client: &'a Client,
    pub config: &'a HttpConfig,
    pub throttle: &'a Throttle,
    pub playback: &'a Playback,
    pub cancel: &'a CancellationToken,
}

impl<'a> Env<'a> {
    fn common(&self) -> transfer::Env<'a> {
        transfer::Env {
            config: self.config,
            throttle: self.throttle,
            playback: self.playback,
            cancel: self.cancel,
        }
    }

    /// Where the other connections get their ranges.
    fn source(&self, dl: &Download) -> Source {
        Source::Http {
            client: self.client.clone(),
            url: dl.url.clone(),
        }
    }
}

struct Opened {
    name: String,
    /// `None` when the file was already fully downloaded.
    body: Option<(Response, File)>,
}

/// Sends the request and prepares a single-connection `.part.ottie` file: a
/// fresh start, a resume, or a restart from zero when resuming isn't safe.
async fn open(
    env: &Env<'_>,
    dl: &mut Download,
    on_event: &mut impl FnMut(Event),
) -> Result<Opened, Error> {
    let (Some(name), Some(part)) = (dl.file_name.clone(), dl.part_path()) else {
        let response = get_from_start(env, &dl.url).await?;
        let (name, file) = transfer::reserve(dl, suggested_name(&response)).await?;
        tracing::debug!(url = %redact(&dl.url), %name, "starting download");
        learn(dl, &response);
        return Ok(Opened {
            name,
            body: Some((response, file)),
        });
    };

    let on_disk = match fs::metadata(&part).await {
        Ok(meta) => Some(meta.len()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let reason = match (on_disk, dl.validator.clone()) {
        (None, _) => RestartReason::PartFileMissing,
        (Some(offset), Some(validator)) if dl.resumable => {
            match try_resume(env, dl, &part, offset, &validator).await? {
                Resume::Continue(body) => return Ok(Opened { name, body }),
                Resume::Restart(response, reason) => {
                    // The server already sent the whole file: use it.
                    restarting(dl, reason, on_event).await?;
                    learn(dl, &response);
                    let file = File::create(&part).await?;
                    return Ok(Opened {
                        name,
                        body: Some((response, file)),
                    });
                }
                Resume::Refused(reason) => reason,
            }
        }
        (Some(_), _) => RestartReason::NotResumable,
    };

    restarting(dl, reason, on_event).await?;
    fresh(env, dl, name, &part).await
}

/// The name from the server's `Content-Disposition`, else the last part of
/// the URL it answered from (after redirects). Sanitized either way.
fn suggested_name(response: &Response) -> String {
    response
        .headers()
        .get(CONTENT_DISPOSITION)
        .and_then(|value| filename::from_content_disposition(value.as_bytes()))
        .unwrap_or_else(|| filename::from_url(response.url()))
}

/// Requests the file from the start again and empties the `.part.ottie` file.
async fn fresh(
    env: &Env<'_>,
    dl: &mut Download,
    name: String,
    part: &Path,
) -> Result<Opened, Error> {
    let response = get_from_start(env, &dl.url).await?;
    learn(dl, &response);
    let file = File::create(part).await?;
    Ok(Opened {
        name,
        body: Some((response, file)),
    })
}

enum Resume {
    /// Continue appending to the `.part.ottie` file (or nothing left to fetch).
    Continue(Option<(Response, File)>),
    /// The server answered with the full file instead of the rest.
    Restart(Response, RestartReason),
    /// Resuming isn't possible; a new full request is needed.
    Refused(RestartReason),
}

async fn try_resume(
    env: &Env<'_>,
    dl: &mut Download,
    part: &Path,
    offset: u64,
    validator: &Validator,
) -> Result<Resume, Error> {
    let response = get(
        env.client,
        env.config.stall_timeout,
        &dl.url,
        Some((offset, None)),
        Some(validator),
    )
    .await?;
    match response.status() {
        StatusCode::PARTIAL_CONTENT => {
            if !content_range_matches(response.headers(), offset, None, dl.total) {
                return Ok(Resume::Refused(RestartReason::RemoteChanged));
            }
            // Written from `offset` rather than appended: a resume from zero
            // may still be split, which sets the file's length, and Windows
            // refuses that on a file opened to append.
            let mut file = OpenOptions::new().write(true).open(part).await?;
            file.seek(SeekFrom::Start(offset)).await?;
            dl.downloaded = offset;
            Ok(Resume::Continue(Some((response, file))))
        }
        StatusCode::OK => {
            let reason = response_reason(&response, validator);
            Ok(Resume::Restart(response, reason))
        }
        // Nothing left: safe because a file at its full length here was
        // written in order (one connection appends; a restart empties the
        // file at once), never one allocated in full with holes.
        StatusCode::RANGE_NOT_SATISFIABLE if dl.total == Some(offset) => {
            dl.downloaded = offset;
            Ok(Resume::Continue(None))
        }
        StatusCode::RANGE_NOT_SATISFIABLE => Ok(Resume::Refused(RestartReason::RemoteChanged)),
        _ => Err(status_error(&response)),
    }
}

enum Resumed {
    /// Completed, or paused again.
    Stopped(Outcome),
    Restart(RestartReason),
}

async fn resume_segmented(
    env: &Env<'_>,
    dl: &mut Download,
    name: &str,
    part: &Path,
    on_event: &mut impl FnMut(Event),
) -> Result<Resumed, Error> {
    let reason = match fs::metadata(part).await {
        Err(e) if e.kind() == io::ErrorKind::NotFound => RestartReason::PartFileMissing,
        Err(e) => return Err(e.into()),
        Ok(meta) if Some(meta.len()) != dl.total => RestartReason::PartFileDamaged,
        Ok(_) if !dl.resumable || dl.validator.is_none() => RestartReason::NotResumable,
        Ok(_) => {
            dl.downloaded = dl.segments.iter().map(|s| s.done).sum();
            started(dl, name, on_event);
            let source = env.source(dl);
            match segmented::run(&env.common(), source, dl, part, None, on_event).await? {
                Segmented::Finished => return complete(dl, name).await.map(Resumed::Stopped),
                Segmented::Cancelled => return Ok(Resumed::Stopped(Outcome::Paused)),
                Segmented::Restart(reason) => reason,
            }
        }
    };
    Ok(Resumed::Restart(reason))
}

/// A 200 to a range request means either the `If-Range` validator didn't
/// match (the file changed) or the server ignores ranges. The validator tells which.
pub(crate) fn response_reason(response: &Response, validator: &Validator) -> RestartReason {
    if validator_from(response.headers()).as_ref() == Some(validator) {
        RestartReason::RangesIgnored
    } else {
        RestartReason::RemoteChanged
    }
}

/// Records what a response from the start of the file tells us about it.
fn learn(dl: &mut Download, response: &Response) {
    let headers = response.headers();
    let refuses_ranges = headers
        .get(ACCEPT_RANGES)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"none"));
    dl.total = match response.status() {
        StatusCode::PARTIAL_CONTENT => content_range(headers).and_then(|(_, _, size)| size),
        _ => response.content_length(),
    };
    dl.validator = validator_from(headers);
    dl.resumable = dl.validator.is_some() && !refuses_ranges;
    dl.downloaded = 0;
}

/// Sends a GET. `range` is `(first byte, last byte if any)`; `if_range` makes
/// the server send the whole file instead if it has changed since.
/// Waiting longer than `stall` for the answer's headers is an error.
pub(crate) async fn get(
    client: &Client,
    stall: Duration,
    url: &Url,
    range: Option<(u64, Option<u64>)>,
    if_range: Option<&Validator>,
) -> Result<Response, Error> {
    check_http_scheme(url)?;
    // Ask for the raw bytes: no compression, so ranges map to file offsets.
    let mut request = client.get(url.clone()).header(ACCEPT_ENCODING, "identity");
    if let Some((first, last)) = range {
        let last = last.map(|l| l.to_string()).unwrap_or_default();
        request = request.header(RANGE, format!("bytes={first}-{last}"));
    }
    if let Some(validator) = if_range {
        request = request.header(IF_RANGE, validator.header_value());
    }
    tokio::time::timeout(stall, request.send())
        .await
        .map_err(|_| Error::Stalled(stall))?
        .map_err(Error::from)
}

/// An answer we can't use, with the server's `Retry-After` if it gave one in
/// seconds. (The HTTP-date form is ignored; the usual backoff applies.)
pub(crate) fn status_error(response: &Response) -> Error {
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .map(Duration::from_secs);
    Error::Status {
        status: response.status(),
        retry_after,
    }
}

/// Asks for `bytes=0-`: a 206 answer proves the server really supports
/// ranges, which the `Accept-Ranges` header alone doesn't.
async fn get_from_start(env: &Env<'_>, url: &Url) -> Result<Response, Error> {
    let response = get(
        env.client,
        env.config.stall_timeout,
        url,
        Some((0, None)),
        None,
    )
    .await?;
    match response.status() {
        StatusCode::OK => Ok(response),
        StatusCode::PARTIAL_CONTENT if content_range_matches(response.headers(), 0, None, None) => {
            Ok(response)
        }
        // Some servers refuse any range on an empty file.
        StatusCode::RANGE_NOT_SATISFIABLE => {
            let response = get(env.client, env.config.stall_timeout, url, None, None).await?;
            match response.status() {
                StatusCode::OK => Ok(response),
                _ => Err(status_error(&response)),
            }
        }
        _ => Err(status_error(&response)),
    }
}

/// One range of a multi-connection download: `pos..end` with `If-Range`.
/// `Ok(Err(_))`: the file changed on the server, or ranges stopped working.
pub(crate) async fn range(
    client: &Client,
    stall: Duration,
    url: &Url,
    same: Same<'_>,
    pos: u64,
    end: u64,
) -> Result<Result<Body, RestartReason>, Error> {
    let last = end - 1;
    let response = get(
        client,
        stall,
        url,
        Some((pos, Some(last))),
        Some(same.validator),
    )
    .await?;
    Ok(match response.status() {
        StatusCode::PARTIAL_CONTENT
            if content_range_matches(response.headers(), pos, Some(last), Some(same.total)) =>
        {
            Ok(Body::Http(response))
        }
        StatusCode::PARTIAL_CONTENT | StatusCode::RANGE_NOT_SATISFIABLE => {
            Err(RestartReason::RemoteChanged)
        }
        StatusCode::OK => Err(response_reason(&response, same.validator)),
        _ => return Err(status_error(&response)),
    })
}

/// Weak ETags (`W/"…"`) can't be used with `If-Range`, so fall back to `Last-Modified`.
fn validator_from(headers: &HeaderMap) -> Option<Validator> {
    let text = |name| headers.get(name).and_then(|v| v.to_str().ok());
    match text(ETAG) {
        Some(etag) if !etag.starts_with("W/") => Some(Validator::ETag(etag.to_owned())),
        _ => text(LAST_MODIFIED).map(|lm| Validator::LastModified(lm.to_owned())),
    }
}

/// A 206 must start exactly where we asked, end where we asked (or at the
/// end of the file), and describe a file of the size we expect.
pub(crate) fn content_range_matches(
    headers: &HeaderMap,
    first: u64,
    last: Option<u64>,
    total: Option<u64>,
) -> bool {
    let Some((start, end, size)) = content_range(headers) else {
        return false;
    };
    let end_ok = match (last, size) {
        (Some(last), _) => end == last,
        (None, Some(size)) => end.checked_add(1) == Some(size),
        (None, None) => true,
    };
    let size_ok = match (total, size) {
        (Some(total), Some(size)) => total == size,
        _ => true,
    };
    start == first && end_ok && size_ok
}

fn content_range(headers: &HeaderMap) -> Option<(u64, u64, Option<u64>)> {
    headers
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range)
}

/// Parses `bytes <start>-<end>/<size or *>`. A range that ends before it
/// starts, or past the end of the file, describes nothing: `None`.
fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let (range, size) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let size = match size {
        "*" => None,
        n => Some(n.parse().ok()?),
    };
    let (start, end): (u64, u64) = (start.parse().ok()?, end.parse().ok()?);
    let fits = start <= end && size.is_none_or(|size| end < size);
    fits.then_some((start, end, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        assert_eq!(
            parse_content_range("bytes 10-99/100"),
            Some((10, 99, Some(100)))
        );
        assert_eq!(parse_content_range("bytes 10-99/*"), Some((10, 99, None)));
        assert_eq!(parse_content_range("bytes */100"), None);
        assert_eq!(parse_content_range("items 1-2/3"), None);
        // Ranges that can't be.
        assert_eq!(parse_content_range("bytes 99-10/100"), None);
        assert_eq!(parse_content_range("bytes 10-100/100"), None);
        assert_eq!(parse_content_range("bytes 0-18446744073709551615/0"), None);
    }

    #[test]
    fn the_largest_numbers_dont_overflow() {
        let mut headers = HeaderMap::new();
        let max = u64::MAX;
        // Asked from the start, to the end: once `end + 1`, which overflowed.
        headers.insert(CONTENT_RANGE, format!("bytes 0-{max}/0").parse().unwrap());
        assert!(!content_range_matches(&headers, 0, None, None));
        headers.insert(
            CONTENT_RANGE,
            format!("bytes 0-{}/{max}", max - 1).parse().unwrap(),
        );
        assert!(content_range_matches(&headers, 0, None, Some(max)));
    }
}
