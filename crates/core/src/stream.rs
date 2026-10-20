//! Reading a download while it arrives ("play while downloading"): a
//! [`Reader`] hands out the bytes at any offset, waiting until they're in
//! the file. See docs/decisions/0029.

use std::io::{self, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};

use crate::download::Segment;
use crate::error::Error;
use crate::filename;
use crate::id::JobId;
use crate::job::{JobEvent, JobSnapshot, JobState};
use crate::manager::Manager;
use crate::playback::Head;

/// Bytes flushed by a multi-connection download aren't announced, so a
/// waiting reader looks again at least this often.
const LOOK_AGAIN: Duration = Duration::from_secs(1);

/// What a reader knows once the download has a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub file_name: String,
    /// Size in bytes, if the server told us (or the download is complete).
    pub total: Option<u64>,
}

/// Reads one download's file, complete or not. Only bytes already in the
/// file are handed out: up to the `.part.ottie` length with one connection,
/// the ranges written with several (as the running download shows them
/// while someone reads, else as saved), the whole file once complete.
pub struct Reader {
    manager: Manager,
    id: JobId,
    events: broadcast::Receiver<JobEvent>,
    /// Where it reads, so a running download goes there first.
    head: Head,
    /// The file being read and where it was when opened. Reopened when the
    /// job's file moves or is renamed on completion.
    file: Option<(PathBuf, File)>,
    /// The job's restarts when it opened: another count means its file is
    /// another version. Its event says so sooner, but a reader that falls
    /// behind (a player paused for minutes) can miss it; the count it can't.
    restarts: u64,
}

impl Reader {
    /// Waits until the job has a file (its first response arrived). Fails if
    /// it's removed, or fails or is cancelled first. Waits while paused or queued.
    pub async fn open(manager: &Manager, id: JobId) -> Result<(Reader, Opened), Error> {
        // Subscribed before the first look, so no change is missed.
        let mut reader = Reader {
            manager: manager.clone(),
            id,
            events: manager.subscribe(),
            head: manager.head(id),
            file: None,
            restarts: 0,
        };
        loop {
            let job = reader.manager.job(id).await?;
            reader.restarts = job.restarts;
            if let Some(file_name) = job.file_name.clone() {
                let total = match job.state {
                    JobState::Completed => Some(job.total.unwrap_or(job.downloaded)),
                    _ => job.total,
                };
                return Ok((reader, Opened { file_name, total }));
            }
            if stopped(job.state) {
                return Err(Error::DownloadStopped(id));
            }
            reader.wait().await?;
        }
    }

    /// Up to `max` bytes from `offset`, waiting until at least one is there.
    /// Empty at the end of the file. Fails if the job is removed, fails or is
    /// cancelled before they arrive, or starts over (its file changed).
    pub async fn read(&mut self, offset: u64, max: usize) -> Result<Vec<u8>, Error> {
        self.check_events()?;
        self.head.set(offset);
        loop {
            let job = self.manager.job(self.id).await?;
            if job.restarts != self.restarts {
                return Err(Error::FileChanged(self.id));
            }
            let complete = job.state == JobState::Completed;
            if job.total.is_some_and(|total| offset >= total) {
                return Ok(Vec::new());
            }
            if let Some(path) = file_path(&job) {
                // With several connections the file has its full size from
                // the start, so only the ranges written count: as the
                // running download shows them, else as last saved.
                let shown = if complete { None } else { self.head.written() };
                let segments = shown.as_deref().unwrap_or(&job.segments);
                let until =
                    (!complete && !segments.is_empty()).then(|| readable_until(segments, offset));
                let max = until.map_or(max, |until| {
                    usize::try_from(until - offset).map_or(max, |left| left.min(max))
                });
                if max > 0 {
                    match self.read_at(path, offset, max).await {
                        Ok(bytes) if !bytes.is_empty() => return Ok(bytes),
                        Ok(_) if complete => return Ok(Vec::new()),
                        Ok(_) => {}
                        // Renamed or moved since we looked: look again.
                        Err(err) if err.kind() == io::ErrorKind::NotFound => self.file = None,
                        Err(err) => return Err(Error::Io(err)),
                    }
                }
            }
            if stopped(job.state) {
                return Err(Error::DownloadStopped(self.id));
            }
            self.wait().await?;
        }
    }

    async fn read_at(&mut self, path: PathBuf, offset: u64, max: usize) -> io::Result<Vec<u8>> {
        let file = match &mut self.file {
            Some((opened, file)) if *opened == path => file,
            _ => {
                let file = File::open(&path).await?;
                &mut self.file.insert((path, file)).1
            }
        };
        file.seek(SeekFrom::Start(offset)).await?;
        let mut bytes = vec![0; max];
        let mut filled = 0;
        while filled < max {
            match file.read(&mut bytes[filled..]).await? {
                0 => break,
                read => filled += read,
            }
        }
        bytes.truncate(filled);
        Ok(bytes)
    }

    /// Fails if the job was removed or started over since the last look.
    /// Events missed by falling behind are no loss: a restart among them
    /// still shows in the job's count (`restarts`).
    fn check_events(&mut self) -> Result<(), Error> {
        loop {
            match self.events.try_recv() {
                Ok(event) => self.check(&event)?,
                Err(TryRecvError::Empty | TryRecvError::Lagged(_)) => return Ok(()),
                Err(TryRecvError::Closed) => return Err(Error::ManagerStopped),
            }
        }
    }

    /// Returns when the job may have more bytes: news about it, or after
    /// `LOOK_AGAIN` anyway.
    async fn wait(&mut self) -> Result<(), Error> {
        let until = tokio::time::Instant::now() + LOOK_AGAIN;
        loop {
            let event = match tokio::time::timeout_at(until, self.events.recv()).await {
                Err(_) | Ok(Err(RecvError::Lagged(_))) => return Ok(()),
                Ok(Err(RecvError::Closed)) => return Err(Error::ManagerStopped),
                Ok(Ok(event)) => event,
            };
            self.check(&event)?;
            if let JobEvent::Changed(JobSnapshot { id, .. }) | JobEvent::Progress { id, .. } = event
                && id == self.id
            {
                return Ok(());
            }
        }
    }

    fn check(&self, event: &JobEvent) -> Result<(), Error> {
        match *event {
            JobEvent::Removed(id) if id == self.id => Err(Error::JobNotFound(id)),
            JobEvent::Restarted { id, .. } if id == self.id => Err(Error::FileChanged(id)),
            _ => Ok(()),
        }
    }
}

/// Failed or cancelled: no more bytes will come by themselves.
fn stopped(state: JobState) -> bool {
    matches!(state, JobState::Failed | JobState::Cancelled)
}

/// Where the job's bytes are now: the finished file, or the `.part.ottie`.
fn file_path(job: &JobSnapshot) -> Option<PathBuf> {
    let name = job.file_name.as_deref()?;
    match job.state {
        JobState::Completed => Some(job.dest_dir.join(name)),
        JobState::Cancelled => None,
        _ => Some(job.dest_dir.join(filename::part_name(name))),
    }
}

/// How far the bytes from `offset` are in the file without a gap: `offset`
/// itself if it isn't. `segments` are sorted and cover the file.
fn readable_until(segments: &[Segment], offset: u64) -> u64 {
    let mut reach = offset;
    for segment in segments {
        let flushed = segment.start + segment.done.min(segment.end - segment.start);
        if segment.start <= reach && reach < flushed {
            reach = flushed;
        }
    }
    reach
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(start: u64, end: u64, done: u64) -> Segment {
        Segment { start, end, done }
    }

    #[test]
    fn reads_on_through_finished_ranges() {
        let segments = [
            segment(0, 100, 100),
            segment(100, 200, 40),
            segment(200, 300, 100),
        ];
        assert_eq!(readable_until(&segments, 0), 140);
        assert_eq!(readable_until(&segments, 120), 140);
        assert_eq!(readable_until(&segments, 210), 300);
    }

    #[test]
    fn nothing_where_no_bytes_arrived_yet() {
        let segments = [segment(0, 100, 10), segment(100, 200, 0)];
        assert_eq!(readable_until(&segments, 10), 10);
        assert_eq!(readable_until(&segments, 50), 50);
        assert_eq!(readable_until(&segments, 100), 100);
    }

    #[test]
    fn a_range_never_counts_past_its_end() {
        let segments = [segment(0, 100, 130), segment(100, 200, 0)];
        assert_eq!(readable_until(&segments, 0), 100);
    }
}
