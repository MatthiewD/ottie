//! Multi-connection transfers. A coordinator (the `run` future) owns the list
//! of ranges; each connection is its own task writing at its offset. When a
//! connection finishes, it picks up an unfinished range or takes over the
//! second half of the range with the most left. See docs/decisions/0009.
//!
//! While players read the file (docs/decisions/0029), connections go where
//! they read, and to the end of the file, before anything else. In streaming
//! mode, the same happens as if a player read from the start, and the file
//! comes in order: each connection takes the next piece after the others.
//!
//! A connection that fails with an error that may go away doesn't stop the
//! others: its range waits for another connection, and the run goes on with
//! one connection fewer. The run fails only when no connection is left. A
//! connection the speed limit starves lets go of its request, and the next
//! one waits for bandwidth before asking. See docs/decisions/0033.

use std::io::{self, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::download::{Download, Event, RestartReason, Segment, Validator};
use crate::error::Error;
use crate::http::HttpConfig;
use crate::throttle::Throttle;
use crate::transfer::{Body, Env, Same, Source, flush};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
/// How often each connection flushes to disk, and so how often the saved
/// progress (`Event::Checkpoint`) can move forward.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
/// While a player reads, connections also hand their bytes to the system
/// this often, so the player can read them. Only that: syncing them to disk
/// as often would make the player wait for the disk (seconds on Windows).
const SHOW_INTERVAL: Duration = Duration::from_millis(200);
/// While a player reads, this much of the end of the file comes early: many
/// videos (MP4s especially) keep their index there.
const TAIL: u64 = 1024 * 1024;
/// In streaming mode, pieces are never smaller than this many `min_split`s.
const MIN_PIECE: u64 = 4;
/// In streaming mode, a busy range more than this many pieces long is cut
/// down to one piece. Well above 2, the most a piece can be when it starts,
/// so a piece size that wavers with the speed doesn't cut ranges.
const TRIM: u64 = 4;
/// How often the download's speed, which sizes streaming pieces, is measured.
const SPEED_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) enum Segmented {
    Finished,
    Cancelled,
    /// The file changed on the server (or stopped honouring ranges).
    Restart(RestartReason),
}

/// Splits a new download into equal ranges, one per connection, none smaller
/// than `min_split`.
pub(crate) fn plan(total: u64, config: &HttpConfig) -> Vec<Segment> {
    let by_size = total / config.min_split.max(1);
    let count = u64::try_from(config.connections)
        .unwrap_or(u64::MAX)
        .min(by_size)
        .max(1);
    let size = total / count;
    (0..count)
        .map(|i| Segment {
            start: i * size,
            end: if i + 1 == count {
                total
            } else {
                (i + 1) * size
            },
            done: 0,
        })
        .collect()
}

/// Shared between the coordinator and one connection. Atomics let both sides
/// read and update these numbers at any time without locks or messages.
struct Shared {
    /// Exclusive end. Only the coordinator changes it, by lowering it on a split.
    end: AtomicU64,
    /// Next offset the connection will write.
    pos: AtomicU64,
    /// Everything before this offset is flushed to disk.
    durable: AtomicU64,
    /// Everything before this offset is in the file, if not yet on disk:
    /// players can read it. Never behind `durable`.
    shown: AtomicU64,
}

struct Part {
    start: u64,
    shared: Arc<Shared>,
    busy: bool,
    /// Its connection was told to stop where it is, to go where a player reads.
    stopping: bool,
}

impl Part {
    fn new(segment: Segment) -> Part {
        let reached = segment.start + segment.done;
        Part {
            start: segment.start,
            shared: Arc::new(Shared {
                end: AtomicU64::new(segment.end),
                pos: AtomicU64::new(reached),
                durable: AtomicU64::new(reached),
                shown: AtomicU64::new(reached),
            }),
            busy: false,
            stopping: false,
        }
    }

    fn end(&self) -> u64 {
        self.shared.end.load(SeqCst)
    }

    /// Written so far. Can briefly pass `end` after a split, so it's capped.
    fn pos(&self) -> u64 {
        self.shared.pos.load(SeqCst).min(self.end())
    }

    fn left(&self) -> u64 {
        self.end() - self.pos()
    }

    /// Whether its next bytes lead to one of the `points` players need.
    fn leads_to(&self, points: &[u64]) -> bool {
        points
            .iter()
            .any(|&point| self.pos() <= point && point < self.end())
    }

    /// What has arrived, flushed or not: for showing progress, never for saving.
    fn received(&self) -> Segment {
        Segment {
            start: self.start,
            end: self.end(),
            done: self.pos() - self.start,
        }
    }

    fn segment(&self) -> Segment {
        self.up_to(&self.shared.durable)
    }

    /// What players can read: in the file, maybe not yet on disk.
    fn shown(&self) -> Segment {
        self.up_to(&self.shared.shown)
    }

    fn up_to(&self, reached: &AtomicU64) -> Segment {
        let end = self.end();
        let reached = reached.load(SeqCst).min(end);
        Segment {
            start: self.start,
            end,
            done: reached - self.start,
        }
    }
}

/// What every connection needs, shared read-only (but for `connections`).
struct Context {
    /// Where ranges come from: HTTP or FTP.
    source: Source,
    stall: Duration,
    starved_after: Duration,
    /// How many connections this run may have: one fewer after each that failed.
    connections: AtomicUsize,
    validator: Validator,
    path: PathBuf,
    total: u64,
    throttle: Throttle,
    /// Someone is reading the file: hand bytes to the system more often.
    reading: AtomicBool,
}

enum Ended {
    /// Reached the end of its range.
    Done,
    /// The speed limit held it back too long: it let go of its request.
    Starved,
    Cancelled,
    Changed(RestartReason),
}

/// Downloads every unfinished range of `dl.segments` into the full-size file
/// at `path`, asking `source` for them. `first` is an already-open answer
/// starting at byte 0 (fresh downloads reuse the request that discovered the
/// file).
pub(crate) async fn run(
    env: &Env<'_>,
    source: Source,
    dl: &mut Download,
    path: &Path,
    first: Option<Body>,
    on_event: &mut impl FnMut(Event),
) -> Result<Segmented, Error> {
    let Env { config, cancel, .. } = env;
    let (Some(total), Some(validator)) = (dl.total, dl.validator.clone()) else {
        return Ok(Segmented::Restart(RestartReason::NotResumable));
    };
    let context = Arc::new(Context {
        source,
        stall: config.stall_timeout,
        starved_after: config.starved_after,
        connections: AtomicUsize::new(config.connections.max(1)),
        validator,
        path: path.to_owned(),
        total,
        throttle: env.throttle.clone(),
        reading: AtomicBool::new(false),
    });
    let mut parts: Vec<Part> = dl.segments.iter().copied().map(Part::new).collect();
    compact(&mut parts);
    tracing::debug!(ranges = parts.len(), total, "several connections");
    // Stops every connection, either because the user paused or because
    // one connection hit a problem that ends the whole run.
    let stop = cancel.child_token();
    let mut tasks = JoinSet::new();
    let mut first = first;
    let mut failure: Option<Error> = None;
    let mut restart: Option<RestartReason> = None;
    let mut speed = Speed::new(written(&parts));
    let piece = env.playback.streaming().then(|| piece_size(config, 0));
    let heads = env.playback.offsets();
    let mut order = steer(&mut parts, &heads, piece, total, config.min_split);
    fill(
        &mut parts, &mut tasks, &context, &stop, &mut first, config, &order,
    );

    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    let mut last_progress = written(&parts);
    let mut last_checkpoint = (Instant::now(), segments(&parts));
    while !tasks.is_empty() {
        tokio::select! {
            Some(joined) = tasks.join_next() => {
                let Ok((shared, result)) = joined else {
                    // A connection task panicked. Stop the others; resuming is safe.
                    failure.get_or_insert(Error::Io(io::Error::other("connection task panicked")));
                    stop.cancel();
                    continue;
                };
                // Ranges move as finished ones merge, so it's found by what
                // it shares (always there: a busy range never merges).
                if let Some(part) = parts.iter_mut().find(|p| Arc::ptr_eq(&p.shared, &shared)) {
                    part.busy = false;
                    part.stopping = false;
                }
                compact(&mut parts);
                match result {
                    Ok(Ended::Done | Ended::Starved) if !stop.is_cancelled() => {
                        fill(&mut parts, &mut tasks, &context, &stop, &mut first, config, &order);
                    }
                    Ok(Ended::Done | Ended::Starved | Ended::Cancelled) => {}
                    Ok(Ended::Changed(reason)) => {
                        restart.get_or_insert(reason);
                        stop.cancel();
                    }
                    // Others still work: carry on without this one (the
                    // server may allow fewer connections). Its range waits
                    // for the next connection that's free.
                    Err(err) if err.is_retryable() && parts.iter().any(|p| p.busy) => {
                        let left = context.connections.load(SeqCst).saturating_sub(1).max(1);
                        context.connections.store(left, SeqCst);
                        tracing::debug!(error = %err, connections = left, "a connection failed; going on without it");
                        failure = Some(err);
                    }
                    Err(err) => {
                        // Report this one: an earlier failure was got over.
                        failure = Some(err);
                        stop.cancel();
                    }
                }
            }
            _ = ticker.tick() => {
                let heads = env.playback.offsets();
                let reading = !heads.is_empty();
                context.reading.store(reading, SeqCst);
                speed.update(written(&parts));
                if !stop.is_cancelled() {
                    let piece = env.playback.streaming().then(|| piece_size(config, speed.per_second));
                    order = steer(&mut parts, &heads, piece, total, config.min_split);
                    if !order.wanted.is_empty() || order.pieces.is_some() {
                        fill(&mut parts, &mut tasks, &context, &stop, &mut first, config, &order);
                        make_room(&mut parts, &order.wanted, config.min_split);
                    }
                }
                env.playback.show_written(reading.then(|| shown(&parts)));
                let now = written(&parts);
                if now != last_progress {
                    last_progress = now;
                    on_event(Event::Progress {
                        downloaded: now,
                        total: Some(total),
                        segments: covering(&parts).map(Part::received).collect(),
                    });
                }
                if last_checkpoint.0.elapsed() >= FLUSH_INTERVAL {
                    let current = segments(&parts);
                    if current != last_checkpoint.1 {
                        on_event(Event::Checkpoint { segments: current.clone() });
                    }
                    last_checkpoint = (Instant::now(), current);
                }
            }
        }
    }

    // Every connection has stopped and flushed: record exactly what's on
    // disk. Readers go by that from now on.
    env.playback.show_written(None);
    dl.segments = segments(&parts);
    dl.downloaded = dl.segments.iter().map(|s| s.done).sum();
    on_event(Event::Checkpoint {
        segments: dl.segments.clone(),
    });

    if dl.segments.iter().all(Segment::is_complete) {
        Ok(Segmented::Finished)
    } else if cancel.is_cancelled() {
        Ok(Segmented::Cancelled)
    } else if let Some(reason) = restart {
        Ok(Segmented::Restart(reason))
    } else if let Some(err) = failure {
        Err(err)
    } else {
        Err(Error::Incomplete {
            received: dl.downloaded,
            expected: total,
        })
    }
}

type Task = (Arc<Shared>, Result<Ended, Error>);

/// Where connections go first.
struct Order {
    /// Where players need bytes next, most urgent first (empty if nobody reads).
    wanted: Vec<u64>,
    /// Streaming mode: ranges are taken in order, a piece at a time.
    pieces: Option<Pieces>,
}

#[derive(Clone, Copy)]
struct Pieces {
    /// Where the order starts: the newest player's offset, else the start of the file.
    from: u64,
    /// Bytes a connection takes at a time.
    size: u64,
}

/// Works out where connections go first, and starts ranges there: at each
/// point players need (see `needed`). In streaming mode (`piece` = the size
/// of a piece), as if a player read from the start when none does, and no
/// busy range is left much longer than a piece, so the rest comes in order.
fn steer(parts: &mut Vec<Part>, heads: &[u64], piece: Option<u64>, total: u64, lead: u64) -> Order {
    let heads = match piece {
        Some(_) if heads.is_empty() => &[0][..],
        _ => heads,
    };
    let wanted = needed(parts, heads, total);
    follow(parts, &wanted, lead);
    if let Some(size) = piece {
        trim(parts, size);
    }
    Order {
        wanted,
        pieces: piece.map(|size| Pieces {
            from: heads.first().copied().unwrap_or(0),
            size,
        }),
    }
}

/// Starts connections until as many are busy as this run may have
/// (`config.connections`, fewer after failures): first on ranges
/// that lead to a point players need, then on ranges nobody is working on
/// (in streaming mode, the next one in order, cut to a piece), then by
/// splitting the busy range with the most left.
fn fill(
    parts: &mut Vec<Part>,
    tasks: &mut JoinSet<Task>,
    context: &Arc<Context>,
    stop: &CancellationToken,
    first: &mut Option<Body>,
    config: &HttpConfig,
    order: &Order,
) {
    while parts.iter().filter(|p| p.busy).count() < context.connections.load(SeqCst) {
        let idle = |p: &Part| !p.busy && p.left() > 0;
        let next = parts
            .iter()
            .position(|p| idle(p) && p.leads_to(&order.wanted))
            .or_else(|| match order.pieces {
                Some(pieces) => next_in_order(parts, pieces.from),
                None => parts.iter().position(idle),
            });
        let Some(index) = next.or_else(|| split(parts, config.min_split)) else {
            return;
        };
        if let Some(pieces) = order.pieces
            && parts[index].left() >= 2 * pieces.size
        {
            let at = parts[index].pos() + pieces.size;
            cut(parts, index, at);
        }
        let part = &mut parts[index];
        part.busy = true;
        // The opening response starts at byte 0, so only the range there can use it.
        let response = if part.pos() == 0 { first.take() } else { None };
        let context = Arc::clone(context);
        let shared = Arc::clone(&part.shared);
        let stop = stop.clone();
        tasks.spawn(async move {
            let result = connection(&context, &shared, response, &stop).await;
            (shared, result)
        });
    }
}

/// Gives the second half of the busy range with the most left to a new range.
fn split(parts: &mut Vec<Part>, min_split: u64) -> Option<usize> {
    let (index, left) = parts
        .iter()
        .enumerate()
        .filter(|(_, p)| p.busy)
        .map(|(i, p)| (i, p.left()))
        .max_by_key(|&(_, left)| left)?;
    if left < 2 * min_split.max(1) {
        return None;
    }
    let middle = parts[index].end() - left / 2;
    Some(cut(parts, index, middle))
}

/// Ends the range at `index` at `at`; what came after becomes a new range,
/// for later, whose index this returns. A busy connection notices its lower
/// `end` before its next write.
fn cut(parts: &mut Vec<Part>, index: usize, at: u64) -> usize {
    let old_end = parts[index].end();
    parts[index].shared.end.store(at, SeqCst);
    parts.push(Part::new(Segment {
        start: at,
        end: old_end,
        done: 0,
    }));
    parts.len() - 1
}

/// Merges each run of neighbouring finished ranges into one, and drops the
/// empty ones nobody works on (left by `make_room`). Streaming cuts a file
/// into thousands of pieces; without this, steering would walk each of them
/// every tick, and progress and checkpoints would carry them all. A range is
/// finished only once its connection has ended and it's flushed to disk, so
/// what's saved never covers more than what's on disk.
fn compact(parts: &mut Vec<Part>) {
    let finished = |p: &Part| !p.busy && p.shared.durable.load(SeqCst) >= p.end();
    if parts.iter().filter(|p| finished(p)).count() < 2 {
        return;
    }
    let (mut done, rest): (Vec<Part>, Vec<Part>) = parts.drain(..).partition(finished);
    done.retain(|p| p.end() > p.start);
    done.sort_by_key(|p| p.start);
    parts.extend(rest);
    let mut merged: Vec<Part> = Vec::with_capacity(done.len());
    for part in done {
        match merged.last_mut() {
            Some(last) if last.end() == part.start => {
                let end = part.end();
                for reached in [
                    &last.shared.end,
                    &last.shared.pos,
                    &last.shared.durable,
                    &last.shared.shown,
                ] {
                    reached.store(end, SeqCst);
                }
            }
            _ => merged.push(part),
        }
    }
    parts.extend(merged);
}

/// Streaming mode: cuts each busy range with more than `TRIM` pieces left
/// down to one piece. The rest waits for a connection, which takes ranges in
/// order. After a switch to streaming mode, connections come to the front.
fn trim(parts: &mut Vec<Part>, piece: u64) {
    for index in 0..parts.len() {
        let part = &parts[index];
        if part.busy && !part.stopping && part.left() > TRIM * piece {
            let at = part.pos() + piece;
            tracing::debug!(at, "streaming: a long range is cut to a piece");
            cut(parts, index, at);
        }
    }
}

/// The range nobody is working on that comes next from `from`: the first
/// one that ends after it, else (when they're all before it) the first one.
fn next_in_order(parts: &[Part], from: u64) -> Option<usize> {
    parts
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.busy && p.left() > 0)
        .min_by_key(|(_, p)| (p.end() <= from, p.pos()))
        .map(|(index, _)| index)
}

/// Streaming mode: how much a connection takes at a time, at `speed` bytes
/// per second for the whole download. See [`HttpConfig::piece_time`].
fn piece_size(config: &HttpConfig, speed: u64) -> u64 {
    let connections = u64::try_from(config.connections.max(1)).unwrap_or(u64::MAX);
    let by_speed = (speed as f64 * config.piece_time.as_secs_f64()) as u64 / connections;
    by_speed.max(MIN_PIECE * config.min_split.max(1))
}

/// The download's speed, measured about once a second.
struct Speed {
    since: Instant,
    written: u64,
    per_second: u64,
}

impl Speed {
    fn new(written: u64) -> Self {
        Speed {
            since: Instant::now(),
            written,
            per_second: 0,
        }
    }

    /// `written`: bytes written so far.
    fn update(&mut self, written: u64) {
        let elapsed = self.since.elapsed();
        if elapsed < SPEED_INTERVAL {
            return;
        }
        // A cut can hide a few bytes written past a range's new end.
        let bytes = written.saturating_sub(self.written);
        self.per_second = (bytes as f64 / elapsed.as_secs_f64()) as u64;
        (self.since, self.written) = (Instant::now(), written);
    }
}

/// Where players need bytes next: from each of their `heads`, the first
/// byte that hasn't arrived; then the same from the start of the file's last
/// `TAIL` bytes. Most urgent first (heads come newest first), no duplicates.
/// Empty when nobody reads.
fn needed(parts: &[Part], heads: &[u64], total: u64) -> Vec<u64> {
    let mut points = Vec::new();
    if heads.is_empty() {
        return points;
    }
    let tail = total.saturating_sub(TAIL);
    for from in heads.iter().copied().chain([tail]) {
        if let Some(point) = missing_from(parts, from)
            && !points.contains(&point)
        {
            points.push(point);
        }
    }
    points
}

/// The first byte from `offset` on that hasn't arrived; `None` if they all have.
fn missing_from(parts: &[Part], offset: u64) -> Option<u64> {
    let mut reach = offset;
    loop {
        let part = parts.iter().find(|p| p.start <= reach && reach < p.end())?;
        if part.pos() < part.end() {
            return Some(reach.max(part.pos()));
        }
        reach = part.end();
    }
}

/// Splits ranges so each point players need starts one, unless the range's
/// connection (or its start, if idle) is less than `lead` bytes before it:
/// then it gets there soon enough.
fn follow(parts: &mut Vec<Part>, wanted: &[u64], lead: u64) {
    for &point in wanted {
        let Some(index) = parts
            .iter()
            .position(|p| p.start <= point && point < p.end())
        else {
            continue;
        };
        if point.saturating_sub(parts[index].pos()) < lead {
            continue;
        }
        tracing::debug!(
            point,
            busy = parts[index].busy,
            "a player needs bytes here: a range starts"
        );
        cut(parts, index, point);
    }
}

/// For each point players need that still waits for a connection, stops the
/// busy connection that leads to no such point and has the most left, where
/// it is now: it ends at its next chunk, and `fill` then sends it to the
/// point. The rest of its range becomes a range of its own, for later.
fn make_room(parts: &mut Vec<Part>, wanted: &[u64], lead: u64) {
    let waiting = parts
        .iter()
        .filter(|p| !p.busy && p.left() > 0 && p.leads_to(wanted))
        .count();
    let stopping = parts.iter().filter(|p| p.busy && p.stopping).count();
    for _ in stopping..waiting {
        let index = parts
            .iter()
            .enumerate()
            .filter(|(_, p)| p.busy && !p.stopping && !p.leads_to(wanted) && p.left() >= lead)
            .max_by_key(|(_, p)| p.left())
            .map(|(index, _)| index);
        let Some(index) = index else {
            return;
        };
        let pos = parts[index].pos();
        tracing::debug!(at = pos, "a connection stops to make room for a player");
        parts[index].stopping = true;
        cut(parts, index, pos);
    }
}

/// One connection: request its range (unless given an open response), then
/// write until it reaches its `end`, which may move down while it runs.
/// If a split lands just behind it, it may write a few bytes past the new
/// `end`. That's harmless: `If-Range` guarantees they're the same bytes the
/// other connection writes there.
async fn connection(
    context: &Context,
    shared: &Shared,
    response: Option<Body>,
    stop: &CancellationToken,
) -> Result<Ended, Error> {
    let mut pos = shared.pos.load(SeqCst);
    let mut response = match response {
        Some(response) => response,
        None => {
            // Not before there's bandwidth, so the server never waits on a
            // request we don't read.
            tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(Ended::Cancelled),
                () = context.throttle.spend(0) => {}
            }
            let asked = Instant::now();
            let answer = request(context, pos, shared.end.load(SeqCst), stop).await?;
            let waited = asked.elapsed();
            tracing::debug!(from = pos, ?waited, "the server answered a range request");
            match answer {
                Ok(response) => response,
                Err(ended) => return Ok(ended),
            }
        }
    };

    let mut file = OpenOptions::new().write(true).open(&context.path).await?;
    file.seek(SeekFrom::Start(pos)).await?;
    let mut writer = BufWriter::new(file);
    let mut last_flush = Instant::now();
    let mut last_shown = last_flush;

    let result = async {
        loop {
            if pos >= shared.end.load(SeqCst) {
                return Ok(Ended::Done);
            }
            let chunk = tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(Ended::Cancelled),
                chunk = tokio::time::timeout(context.stall, response.chunk()) => {
                    chunk.map_err(|_| Error::Stalled(context.stall))??
                }
            };
            let end = shared.end.load(SeqCst);
            let Some(bytes) = chunk else {
                // The server closed the range before its end.
                return Err(Error::Incomplete {
                    received: pos,
                    expected: end,
                });
            };
            let wanted = usize::try_from(end.saturating_sub(pos)).unwrap_or(usize::MAX);
            let take = wanted.min(bytes.len());
            writer.write_all(&bytes[..take]).await?;
            pos += take as u64;
            shared.pos.store(pos, SeqCst);
            if last_flush.elapsed() >= FLUSH_INTERVAL {
                flush(&mut writer).await?;
                shared.durable.store(pos, SeqCst);
                shared.shown.store(pos, SeqCst);
                (last_flush, last_shown) = (Instant::now(), Instant::now());
            } else if context.reading.load(SeqCst) && last_shown.elapsed() >= SHOW_INTERVAL {
                // To the system, which players read from; not to the disk.
                writer.flush().await?;
                shared.shown.store(pos, SeqCst);
                last_shown = Instant::now();
            }
            // Every connection spends from the same job budget. Held back
            // too long, it lets go of the request (what was spent stays owed).
            tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(Ended::Cancelled),
                paid = tokio::time::timeout(context.starved_after, context.throttle.spend(bytes.len())) => {
                    if paid.is_err() {
                        tracing::debug!(at = pos, "no bandwidth for a while; letting the request go");
                        return Ok(Ended::Starved);
                    }
                }
            }
        }
    }
    .await;

    // Whatever happened, flush so `durable` covers everything written.
    match flush(&mut writer).await {
        Ok(()) => {
            shared.durable.store(pos, SeqCst);
            shared.shown.store(pos, SeqCst);
        }
        Err(err) if result.is_ok() => return Err(err.into()),
        Err(_) => {} // Report the first problem, not this one.
    }
    result
}

/// Asks for `pos..end`. `Ok(Err(_))` means the connection ended without
/// data: paused, or the file changed on the server.
async fn request(
    context: &Context,
    pos: u64,
    end: u64,
    stop: &CancellationToken,
) -> Result<Result<Body, Ended>, Error> {
    let same = Same {
        total: context.total,
        validator: &context.validator,
    };
    let sent = context.source.range(pos, end, same, context.stall);
    tokio::select! {
        biased;
        _ = stop.cancelled() => Ok(Err(Ended::Cancelled)),
        answer = sent => Ok(answer?.map_err(Ended::Changed)),
    }
}

/// Bytes written so far, flushed or not (for progress display).
fn written(parts: &[Part]) -> u64 {
    parts.iter().map(|p| p.pos() - p.start).sum()
}

/// The ranges that cover part of the file. A connection made to stop before
/// it wrote anything (`make_room`) leaves an empty range, starting where the
/// rest of it now does: shown or saved, that would be two ranges in one place.
fn covering(parts: &[Part]) -> impl Iterator<Item = &Part> {
    parts.iter().filter(|p| p.end() > p.start)
}

/// The ranges as flushed to disk, sorted, ready to save.
fn segments(parts: &[Part]) -> Vec<Segment> {
    sorted(covering(parts).map(Part::segment).collect())
}

/// The ranges as players can read them, sorted.
fn shown(parts: &[Part]) -> Vec<Segment> {
    sorted(covering(parts).map(Part::shown).collect())
}

fn sorted(mut segments: Vec<Segment>) -> Vec<Segment> {
    segments.sort_by_key(|s| s.start);
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(connections: usize, min_split: u64) -> HttpConfig {
        HttpConfig {
            connections,
            min_split,
            ..HttpConfig::default()
        }
    }

    #[test]
    fn plan_covers_the_whole_file_without_gaps() {
        let segments = plan(1003, &config(4, 100));
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0].start, 0);
        assert_eq!(segments.last().unwrap().end, 1003);
        for pair in segments.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
    }

    #[test]
    fn plan_respects_the_minimum_size() {
        assert_eq!(plan(250, &config(8, 100)).len(), 2);
        assert_eq!(plan(50, &config(8, 100)).len(), 1);
    }

    #[test]
    fn split_gives_away_the_second_half_of_the_biggest_busy_range() {
        let mut parts: Vec<Part> = plan(1000, &config(2, 100))
            .into_iter()
            .map(Part::new)
            .collect();
        parts[0].busy = true;
        parts[1].busy = true;
        parts[0].shared.pos.store(400, SeqCst); // 100 left
        parts[1].shared.pos.store(600, SeqCst); // 400 left

        let new = split(&mut parts, 100).unwrap();
        assert_eq!(parts[1].end(), 800);
        assert_eq!((parts[new].start, parts[new].end()), (800, 1000));
        // 200 left at most now; splitting it would make pieces under 100.
        parts[new].busy = true;
        parts[1].shared.pos.store(750, SeqCst);
        parts[new].shared.pos.store(900, SeqCst);
        assert!(split(&mut parts, 100).is_none());
    }

    /// Two busy ranges of 1000 bytes, written up to the given offsets.
    fn two_busy(first_pos: u64, second_pos: u64) -> Vec<Part> {
        let parts: Vec<Part> = plan(2000, &config(2, 100))
            .into_iter()
            .map(Part::new)
            .collect();
        for (part, pos) in parts.iter().zip([first_pos, second_pos]) {
            part.shared.pos.store(pos, SeqCst);
        }
        parts
            .into_iter()
            .map(|p| Part { busy: true, ..p })
            .collect()
    }

    fn ranges(parts: &[Part]) -> Vec<(u64, u64)> {
        let mut ranges: Vec<_> = parts.iter().map(|p| (p.start, p.end())).collect();
        ranges.sort_unstable();
        ranges
    }

    #[test]
    fn players_need_the_first_byte_that_has_not_arrived() {
        let parts = two_busy(1000, 1200); // the first range is complete
        assert_eq!(missing_from(&parts, 0), Some(1200));
        assert_eq!(missing_from(&parts, 1100), Some(1200));
        assert_eq!(missing_from(&parts, 1500), Some(1500));
        assert_eq!(missing_from(&parts, 2000), None);

        // Newest head first, then the tail (the whole file here, as it's small).
        assert_eq!(needed(&parts, &[1500, 300], 2000), [1500, 1200]);
        assert!(needed(&parts, &[], 2000).is_empty(), "nobody reads");
    }

    #[test]
    fn a_player_far_ahead_of_a_connection_gets_a_range_of_its_own() {
        let mut parts = two_busy(100, 1100);
        follow(&mut parts, &[1150, 600], 100);
        // 1150 is close behind the second connection; 600 isn't.
        assert_eq!(ranges(&parts), [(0, 600), (600, 1000), (1000, 2000)]);
    }

    #[test]
    fn a_connection_that_serves_no_player_makes_room() {
        let mut parts = two_busy(100, 1100);
        follow(&mut parts, &[600], 100);
        make_room(&mut parts, &[600], 100);
        // The second connection, with the most left, stops where it is.
        assert!(parts[1].stopping);
        assert_eq!(parts[1].end(), 1100);
        assert_eq!(
            ranges(&parts),
            [(0, 600), (600, 1000), (1000, 1100), (1100, 2000)]
        );

        // Asked once is enough while it stops.
        make_room(&mut parts, &[600], 100);
        assert_eq!(parts.len(), 4);
        assert!(!parts[0].stopping);
    }

    #[test]
    fn a_connection_stopped_before_its_first_byte_leaves_no_empty_range() {
        // The second connection waits for bandwidth: nothing written yet.
        let mut parts = two_busy(100, 1000);
        follow(&mut parts, &[600], 100);
        make_room(&mut parts, &[600], 100);
        assert!(parts[1].stopping);
        assert_eq!((parts[1].start, parts[1].end()), (1000, 1000));
        let starts: Vec<u64> = segments(&parts).iter().map(|s| s.start).collect();
        assert_eq!(starts, [0, 600, 1000]);
        assert_eq!(shown(&parts).len(), 3);
    }

    #[test]
    fn a_connection_that_serves_a_player_keeps_going() {
        let mut parts = two_busy(100, 1100);
        let wanted = [150, 1150, 1600];
        follow(&mut parts, &wanted, 100);
        make_room(&mut parts, &wanted, 100);
        // Each serves a player (at 150 and 1150). 1600 is too far ahead, so
        // it gets a range, but nobody is free to take it.
        assert!(!parts[0].stopping && !parts[1].stopping);
        assert_eq!(ranges(&parts), [(0, 1000), (1000, 1600), (1600, 2000)]);
    }

    const MIB: u64 = 1024 * 1024;
    const KIB: u64 = 1024;

    #[test]
    fn streaming_wants_the_start_and_the_end_and_cuts_long_ranges() {
        // Two ranges of 4 MiB, 1 MiB into each.
        let mut parts: Vec<Part> = plan(8 * MIB, &config(2, 64 * KIB))
            .into_iter()
            .map(Part::new)
            .collect();
        for (part, pos) in parts.iter_mut().zip([MIB, 5 * MIB]) {
            part.shared.pos.store(pos, SeqCst);
            part.busy = true;
        }
        let order = steer(&mut parts, &[], Some(256 * KIB), 8 * MIB, 64 * KIB);
        // As if a player read from the start; and the last MiB.
        assert_eq!(order.wanted, [MIB, 7 * MIB]);
        assert_eq!(order.pieces.map(|p| (p.from, p.size)), Some((0, 256 * KIB)));
        // Each busy range is cut to a piece; the last MiB starts a range.
        let piece_end = |pos| pos + 256 * KIB;
        assert_eq!(
            ranges(&parts),
            [
                (0, piece_end(MIB)),
                (piece_end(MIB), 4 * MIB),
                (4 * MIB, piece_end(5 * MIB)),
                (piece_end(5 * MIB), 7 * MIB),
                (7 * MIB, 8 * MIB)
            ]
        );
    }

    #[test]
    fn without_streaming_nobody_reading_changes_nothing() {
        let mut parts = two_busy(100, 1100);
        let order = steer(&mut parts, &[], None, 2000, 100);
        assert!(order.wanted.is_empty() && order.pieces.is_none());
        assert_eq!(ranges(&parts), [(0, 1000), (1000, 2000)]);
    }

    #[test]
    fn the_next_range_in_order_comes_after_the_player() {
        let mut parts = two_busy(100, 1100);
        let idle = |start, end| {
            Part::new(Segment {
                start,
                end,
                done: 0,
            })
        };
        parts[0].shared.end.store(300, SeqCst);
        parts[1].shared.end.store(1200, SeqCst);
        parts.extend([idle(1200, 2000), idle(300, 1000)]);
        let start = |index: Option<usize>| index.map(|i| parts[i].start);
        assert_eq!(start(next_in_order(&parts, 0)), Some(300));
        // Before the player, only once nothing after it is left.
        assert_eq!(start(next_in_order(&parts, 1500)), Some(1200));
        assert_eq!(start(next_in_order(&parts, 2000)), Some(300));
    }

    #[test]
    fn pieces_last_about_two_seconds_of_one_connection() {
        let config = config(4, MIB);
        assert_eq!(piece_size(&config, 0), 4 * MIB, "never under 4 × min_split");
        assert_eq!(piece_size(&config, 40 * MIB), 20 * MIB);
    }

    #[test]
    fn finished_neighbours_become_one_range() {
        let range = |start, end, done| Part::new(Segment { start, end, done });
        let mut parts = vec![
            range(100, 200, 100),
            range(300, 400, 50),
            range(0, 100, 100),
            range(200, 300, 100),
            // Finished but not yet flushed.
            range(400, 500, 100),
            range(500, 600, 100),
            // Left by a connection made to stop before its first byte.
            range(600, 600, 0),
            range(600, 1000, 0),
        ];
        parts[1].busy = true;
        parts[4].shared.durable.store(450, SeqCst);
        compact(&mut parts);
        assert_eq!(
            ranges(&parts),
            [(0, 300), (300, 400), (400, 500), (500, 600), (600, 1000)]
        );
        let done: Vec<u64> = segments(&parts).iter().map(|s| s.done).collect();
        assert_eq!(done, [300, 50, 50, 100, 0]);
        assert!(parts.iter().any(|p| p.busy && p.start == 300), "still busy");

        // A whole streamed prefix becomes one range.
        let mut parts: Vec<Part> = (0..10_000)
            .map(|i| range(i * 10, (i + 1) * 10, 10))
            .collect();
        compact(&mut parts);
        assert_eq!(ranges(&parts), [(0, 100_000)]);
        assert_eq!(missing_from(&parts, 0), None);
    }

    #[test]
    fn segment_never_counts_bytes_past_a_lowered_end() {
        let part = Part::new(Segment {
            start: 0,
            end: 100,
            done: 0,
        });
        part.shared.pos.store(90, SeqCst);
        part.shared.durable.store(90, SeqCst);
        part.shared.end.store(80, SeqCst);
        assert_eq!(part.segment().done, 80);
        assert_eq!(part.left(), 0);
    }
}
