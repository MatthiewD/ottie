//! What HTTP and FTP transfers share (docs/decisions/0054): the bytes of an
//! answer (`Body`), where the ranges of a multi-connection run come from
//! (`Source`), naming the file at the first answer, and the steps of a run
//! that don't depend on the protocol: one connection's loop, completing.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::{Client, Response, Url};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio_util::sync::CancellationToken;

use crate::download::{Download, Event, Outcome, RestartReason, Validator};
use crate::error::Error;
use crate::filename::{self, Existing};
use crate::ftp;
use crate::http::{self, HttpConfig};
use crate::playback::Playback;
use crate::throttle::Throttle;
use crate::webmark;

/// What every step of one run shares, whatever the protocol. The `'a` says
/// these are borrowed from the backend's caller and only live as long as
/// that call.
pub(crate) struct Env<'a> {
    pub config: &'a HttpConfig,
    pub throttle: &'a Throttle,
    pub playback: &'a Playback,
    pub cancel: &'a CancellationToken,
}

/// An answer's bytes, as they arrive.
pub(crate) enum Body {
    Http(Response),
    /// Boxed: much bigger than a response.
    Ftp(Box<ftp::Transfer>),
}

impl Body {
    /// The next bytes, or `None` at the end.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, Error> {
        match self {
            Body::Http(response) => Ok(response.chunk().await?),
            Body::Ftp(transfer) => transfer.chunk().await,
        }
    }

    /// After the last byte: FTP's server then says whether it sent the whole
    /// file (a size it didn't give can't tell); HTTP has said all it will.
    pub async fn finish(self, stall: Duration) -> Result<(), Error> {
        match self {
            Body::Http(_) => Ok(()),
            Body::Ftp(transfer) => transfer.finish(stall).await,
        }
    }
}

/// Where the connections of a multi-connection run get their ranges.
pub(crate) enum Source {
    Http { client: Client, url: Url },
    Ftp(Arc<ftp::Link>),
}

/// The file a range is asked of: its size and what proves it's the same.
pub(crate) struct Same<'a> {
    pub total: u64,
    pub validator: &'a Validator,
}

impl Source {
    /// Opens `pos..end` of the file, if it's still `same`. `Ok(Err(_))`: it
    /// changed on the server (or stopped honouring ranges), so the download
    /// starts over. Waiting longer than `stall` for the answer is an error.
    pub async fn range(
        &self,
        pos: u64,
        end: u64,
        same: Same<'_>,
        stall: Duration,
    ) -> Result<Result<Body, RestartReason>, Error> {
        match self {
            Source::Http { client, url } => http::range(client, stall, url, same, pos, end).await,
            Source::Ftp(link) => link.range(pos, same).await,
        }
    }
}

/// The name the file gets in its folder, from the one the server or link
/// suggests: its folder first follows a [`crate::Placement`], then a name
/// already taken there is renamed, replaced once complete, or asked about
/// (docs/decisions/0049). Reserves it and opens the new `.part.ottie`.
pub(crate) async fn reserve(dl: &mut Download, suggested: String) -> Result<(String, File), Error> {
    if let Some(placement) = dl.placement.take()
        && let Some(folder) = placement.folder_for(&suggested)
    {
        dl.dest_dir = folder.to_owned();
    }
    let replace = match dl.existing {
        Existing::Rename => false,
        Existing::Replace => true,
        Existing::Ask if filename::exists(&dl.dest_dir, &suggested).await? => {
            // Nothing written: it asks before any file is made.
            return Err(Error::FileExists { name: suggested });
        }
        Existing::Ask => false,
    };
    let (name, file) = filename::reserve(&dl.dest_dir, &suggested, replace).await?;
    dl.file_name = Some(name.clone());
    Ok((name, file))
}

/// Starts again from zero: forgets what arrived and empties the
/// `.part.ottie` at once, before the next request, so the file always agrees
/// with what `dl` says arrived. If that request fails, the next try finds an
/// empty file and starts from zero; it never finds a full-size file with
/// holes, which a resume at its length would take as complete. Synced, so it
/// holds before the empty map can be saved.
pub(crate) async fn restarting(
    dl: &mut Download,
    reason: RestartReason,
    on_event: &mut impl FnMut(Event),
) -> Result<(), Error> {
    tracing::debug!(url = %redact(&dl.url), ?reason, "restarting download from zero");
    dl.downloaded = 0;
    dl.segments.clear();
    if let Some(part) = dl.part_path() {
        match OpenOptions::new().write(true).open(&part).await {
            Ok(file) => {
                file.set_len(0).await?;
                file.sync_all().await?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    on_event(Event::Restarted(reason));
    Ok(())
}

/// Waits until the database has the job as it is now, when a manager runs
/// it (`Download::persist`); else there's nothing to wait for.
pub(crate) async fn persisted(dl: &Download) -> Result<(), Error> {
    match &dl.persist {
        Some(persist) => persist.saved().await,
        None => Ok(()),
    }
}

pub(crate) fn started(dl: &Download, name: &str, on_event: &mut impl FnMut(Event)) {
    on_event(Event::Started {
        file_name: name.to_owned(),
        dest_dir: dl.dest_dir.clone(),

        total: dl.total,
        validator: dl.validator.clone(),
        resumable: dl.resumable,
        downloaded: dl.downloaded,
        segments: dl.segments.clone(),
    });
}

/// The total size if this fresh answer may be split across connections: the
/// server proved ranges work (`ranges`), we can check the file stays the
/// same, and it's big enough to be worth it.
pub(crate) fn splittable(config: &HttpConfig, dl: &Download, ranges: bool) -> Option<u64> {
    let total = dl.total?;
    let worth_it = config.connections > 1 && total >= 2 * config.min_split.max(1);
    let safe = ranges && dl.resumable;
    (worth_it && safe && dl.downloaded == 0).then_some(total)
}

/// Downloads the rest of the file over one connection, appending to `file`.
pub(crate) async fn single(
    env: &Env<'_>,
    body: Body,
    file: File,
    dl: &mut Download,
    name: &str,
    on_event: &mut impl FnMut(Event),
) -> Result<Outcome, Error> {
    let mut writer = BufWriter::new(file);
    let mut body = body;
    let streamed = stream_body(env, &mut body, &mut writer, dl, on_event).await;
    // Flush even after an error, so the .part.ottie length matches `dl.downloaded`.
    let flushed = flush(&mut writer).await;
    drop(writer); // Windows can't rename a file that's still open.
    if streamed? == Stop::Cancelled {
        flushed?;
        return Ok(Outcome::Paused);
    }
    flushed?;
    body.finish(env.config.stall_timeout).await?;
    complete(dl, name).await
}

#[derive(Debug, PartialEq, Eq)]
enum Stop {
    Finished,
    Cancelled,
}

async fn stream_body(
    env: &Env<'_>,
    body: &mut Body,
    writer: &mut BufWriter<File>,
    dl: &mut Download,
    on_event: &mut impl FnMut(Event),
) -> Result<Stop, Error> {
    let Env {
        throttle,
        cancel,
        config,
        ..
    } = env;
    let stall = config.stall_timeout;
    let mut synced = Instant::now();
    loop {
        // Wait for whichever comes first: the next chunk or a pause request.
        // `biased` checks the pause first, so a fast server can't delay it.
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(Stop::Cancelled),
            chunk = tokio::time::timeout(stall, body.chunk()) => {
                chunk.map_err(|_| Error::Stalled(stall))??
            }
        };
        let Some(bytes) = chunk else {
            return Ok(Stop::Finished);
        };
        writer.write_all(&bytes).await?;
        dl.downloaded += bytes.len() as u64;
        // On the disk before the progress that covers it is saved (every 2 s),
        // as several connections do (`segmented.rs`).
        if synced.elapsed() >= SYNC_EVERY {
            flush(writer).await?;
            synced = Instant::now();
        }
        on_event(Event::Progress {
            downloaded: dl.downloaded,
            total: dl.total,
            segments: Vec::new(),
        });
        // Over the speed limit: wait before reading more (a pause still wins).
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(Stop::Cancelled),
            () = throttle.spend(bytes.len()) => {}
        }
    }
}

/// How often one connection's bytes are synced to disk while it downloads.
const SYNC_EVERY: Duration = Duration::from_secs(1);

/// Writes buffered bytes and asks the OS to put them on disk.
pub(crate) async fn flush(writer: &mut BufWriter<File>) -> io::Result<()> {
    writer.flush().await?;
    writer.get_ref().sync_data().await
}

pub(crate) async fn complete(dl: &mut Download, name: &str) -> Result<Outcome, Error> {
    match dl.total {
        Some(expected) if expected != dl.downloaded => {
            return Err(Error::Incomplete {
                received: dl.downloaded,
                expected,
            });
        }
        _ => dl.total = Some(dl.downloaded),
    }
    let part = dl.dest_dir.join(filename::part_name(name));
    // A file that appeared meanwhile is replaced only if asked to.
    let replace = dl.existing == Existing::Replace;
    let final_name = filename::finalize(&dl.dest_dir, name, &part, replace).await?;
    let path = dl.dest_dir.join(&final_name);
    webmark::mark(&path, &dl.url).await;
    dl.file_name = Some(final_name);
    dl.segments.clear();
    Ok(Outcome::Completed { path })
}

/// URL without credentials, query or fragment, safe to log.
pub(crate) fn redact(url: &Url) -> String {
    let mut url = url.clone();
    url.set_query(None);
    url.set_fragment(None);
    // These only fail for URLs that can't have credentials anyway.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets() {
        let url = Url::parse("https://user:pw@example.com/f.zip?token=abc#x").unwrap();
        assert_eq!(redact(&url), "https://example.com/f.zip");
        let url = Url::parse("ftp://user:pw@nas.local/films/f.mkv").unwrap();
        assert_eq!(redact(&url), "ftp://nas.local/films/f.mkv");
    }
}
