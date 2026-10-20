//! What the tray shows (docs/decisions/0034): the daemon's downloads,
//! followed through its events stream and summed up once a second. Their
//! changes are also what notifications tell (docs/decisions/0035), and say
//! when everything has finished (0040).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::StreamExt;
use tauri::{AppHandle, Manager};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use crate::client::{Daemon, Event, Job};
use crate::finish::{self, Left};
use crate::notify::{self, News, Notifier};
use crate::prefs;
use crate::remote::{Switch, Target};
use crate::texts::Texts;

/// How often the summary is refreshed.
const EVERY: Duration = Duration::from_secs(1);
/// The wait before following the daemon again after losing it.
const RECONNECT: Duration = Duration::from_secs(2);
/// How much a new measurement moves the speed (the rest is history), as on
/// the page (`ui/src/lib/jobs.ts`).
const SMOOTHING: f64 = 0.3;
/// Measurements closer than this are too noisy to use.
const MIN_INTERVAL: Duration = Duration::from_millis(200);
/// No progress for this long: the download's speed is 0.
const STALE_AFTER: Duration = Duration::from_secs(3);

/// The states of a running download.
const RUNNING: [&str; 3] = ["connecting", "downloading", "retrying"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// The events stream is open; until then, the rest is empty.
    pub connected: bool,
    /// Connecting, downloading or retrying.
    pub running: usize,
    /// Queued.
    pub waiting: usize,
    pub paused: usize,
    /// Every download's speed added up, in bytes per second.
    pub speed: u64,
    /// The total limit; 0 = none.
    pub global_limit: u64,
    /// The token may only look (a read-only device's): the tray offers no
    /// actions on downloads or the limit.
    pub read_only: bool,
    /// How far the running downloads have come; none while none of them
    /// has a known size.
    pub progress: Option<Progress>,
}

/// What the tray's actions may do; a read-only token's are greyed out, as in
/// Ottie's own menu.
impl Summary {
    /// Pause all: something to pause.
    pub fn can_pause(&self) -> bool {
        !self.read_only && self.running + self.waiting > 0
    }

    /// Resume all: something to resume.
    pub fn can_resume(&self) -> bool {
        !self.read_only && self.paused > 0
    }

    /// The total limit: on, off or changed.
    pub fn can_limit(&self) -> bool {
        self.connected && !self.read_only
    }
}

/// The running downloads whose size is known, together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub downloaded: u64,
    pub total: u64,
    /// Their speeds added up, in bytes per second.
    pub speed: u64,
}

/// Where summaries go for the tray, from whichever daemon the window shows
/// (docs/decisions/0053), as managed state.
pub struct Feed(watch::Sender<Summary>);

impl Feed {
    pub fn new() -> Feed {
        Feed(watch::Sender::new(Summary::default()))
    }

    pub fn subscribe(&self) -> watch::Receiver<Summary> {
        self.0.subscribe()
    }
}

/// The window shows another daemon: the tray says it's connecting until
/// that one's follower has its list.
pub fn moved(app: &AppHandle) {
    app.state::<Feed>().0.send_replace(Summary::default());
}

/// The daemon a follower follows.
pub enum Followed {
    /// This computer's, for as long as the app runs.
    Local,
    /// Another computer's, while the window shows it: until it shows
    /// another, even if it comes back to this one later (that's a follower
    /// of its own).
    Remote(Arc<Target>),
}

impl Followed {
    /// Whether the window shows it.
    fn shown(&self, app: &AppHandle) -> bool {
        let switch = app.state::<Switch>();
        match self {
            Followed::Local => switch.is_shown(None),
            Followed::Remote(target) => switch
                .shown()
                .is_some_and(|shown| Arc::ptr_eq(&shown, target)),
        }
    }

    fn local(&self) -> bool {
        matches!(self, Followed::Local)
    }
}

/// Follows a daemon, reconnecting when the stream ends (the daemon
/// restarted or stopped). While the window shows it, it sends the tray a
/// summary every second and notifies; this computer's also says when
/// everything has finished, shown or not.
pub async fn follow(app: AppHandle, daemon: Arc<Daemon>, followed: Followed) {
    // Both outlive a connection: downloads that finish or fail while the
    // daemon can't be reached are told once it's back.
    let mut model = Model::default();
    let mut notifier = Notifier::default();
    let mut moves = app.state::<Switch>().subscribe();
    loop {
        if !followed.local() && !followed.shown(&app) {
            return;
        }
        let ended = session(
            &app,
            &daemon,
            &followed,
            &mut model,
            &mut notifier,
            &mut moves,
        )
        .await;
        if matches!(ended, Ok(Ended::Hidden)) {
            return;
        }
        // Either way it ended, the tray says it's connecting again.
        if followed.shown(&app) {
            moved(&app);
        }
        tokio::time::sleep(RECONNECT).await;
    }
}

/// Why a connection ended without an error.
enum Ended {
    /// The stream closed.
    Closed,
    /// The window shows another daemon: no use following this one.
    Hidden,
}

/// One connection to the events stream, until it ends.
async fn session(
    app: &AppHandle,
    daemon: &Daemon,
    followed: &Followed,
    model: &mut Model,
    notifier: &mut Notifier,
    moves: &mut watch::Receiver<Option<Arc<Target>>>,
) -> Result<Ended> {
    let mut events = daemon.events().await?;
    load(app, daemon, followed, model).await?;
    tell(app, followed, model, notifier);
    let mut tick = tokio::time::interval(EVERY);
    loop {
        tokio::select! {
            message = events.next() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | None => return Ok(Ended::Closed),
                    // Pings are answered by the stream itself.
                    Some(Ok(_)) => continue,
                    Some(Err(err)) => return Err(err.into()),
                };
                // Unreadable: not one of the tray's, or from a newer daemon.
                let Ok(event) = serde_json::from_str::<Event>(&text) else {
                    continue;
                };
                if let Event::SettingsChanged { settings } = &event
                    && followed.shown(app)
                {
                    remember_limit(app, settings.global_limit);
                }
                // Progress changes no state, and comes often.
                let progress = matches!(event, Event::Progress { .. });
                if model.apply(event, Instant::now()) {
                    load(app, daemon, followed, model).await?;
                }
                if !progress {
                    tell(app, followed, model, notifier);
                }
            }
            _ = tick.tick() => {
                if followed.shown(app) {
                    app.state::<Feed>().0.send_replace(model.summary(Instant::now()));
                }
            }
            moved = moves.changed() => {
                // The app is quitting.
                if moved.is_err() {
                    return Ok(Ended::Closed);
                }
                if !followed.local() && !followed.shown(app) {
                    return Ok(Ended::Hidden);
                }
            }
        }
    }
}

/// The whole list and the settings, at the start and when the stream says
/// events were lost (`resync`).
async fn load(
    app: &AppHandle,
    daemon: &Daemon,
    followed: &Followed,
    model: &mut Model,
) -> Result<()> {
    let jobs = daemon.jobs().await?;
    let settings = daemon.settings().await?;
    let access = daemon.access().await?;
    model.reset(jobs, Instant::now());
    model.global_limit = settings.global_limit;
    model.read_only = access.read_only;
    if followed.shown(app) {
        remember_limit(app, settings.global_limit);
    }
    Ok(())
}

/// Notifies what the last changes call for, while the window shows this
/// daemon, then for this computer's, whether everything has finished.
fn tell(app: &AppHandle, followed: &Followed, model: &mut Model, notifier: &mut Notifier) {
    let news = std::mem::take(&mut model.news);
    let ended = !news.is_empty();
    let busy = model.busy();
    // Something new, or finished downloads waiting for the last ones.
    if followed.shown(app) && (ended || (!busy && notifier.waiting())) {
        let prefs = app.state::<prefs::Saved>().get();
        let texts = Texts::for_language(prefs.language.as_deref());
        notify::show(app, notifier.notes(news, busy, &prefs, &texts), &prefs);
    }
    if followed.local() {
        finish::follow(app, model.left(), ended);
    }
}

/// The tray's "Limit speed" switches the last limit back on.
fn remember_limit(app: &AppHandle, limit: u64) {
    if limit > 0 {
        // Best effort: without it, the tray offers the default limit.
        let _ = app
            .state::<prefs::Saved>()
            .change(app, |prefs| prefs.last_limit = Some(limit));
    }
}

/// The downloads as the events tell them.
#[derive(Debug, Default)]
struct Model {
    jobs: HashMap<String, Tracked>,
    global_limit: u64,
    read_only: bool,
    /// Downloads that finished or failed since notifications last looked.
    news: Vec<News>,
}

#[derive(Debug)]
struct Tracked {
    state: String,
    downloaded: u64,
    total: Option<u64>,
    rate: Rate,
}

impl Model {
    /// A fresh list; downloads still there keep their measured speed, and
    /// those that finished or failed in between are news. The first list
    /// isn't: what had finished before the app looked is old.
    fn reset(&mut self, jobs: Vec<Job>, now: Instant) {
        let mut old = std::mem::take(&mut self.jobs);
        for job in jobs {
            let tracked = old.remove(&job.id);
            self.track(job, tracked, now);
        }
    }

    /// A download's new snapshot, `tracked` as it was.
    fn track(&mut self, job: Job, tracked: Option<Tracked>, now: Instant) {
        let rate = match tracked {
            Some(tracked) => {
                self.news.extend(News::of(&tracked.state, &job));
                tracked.rate.next(job.downloaded, now)
            }
            None => Rate::first(job.downloaded, now),
        };
        self.jobs.insert(
            job.id,
            Tracked {
                state: job.state,
                downloaded: job.downloaded,
                total: job.total,
                rate,
            },
        );
    }

    /// Applies an event; true if the whole list must be fetched again.
    fn apply(&mut self, event: Event, now: Instant) -> bool {
        match event {
            Event::Changed { job } => {
                let tracked = self.jobs.remove(&job.id);
                self.track(job, tracked, now);
            }
            Event::Progress {
                id,
                downloaded,
                total,
            } => {
                if let Some(tracked) = self.jobs.get_mut(&id) {
                    tracked.downloaded = downloaded;
                    tracked.total = total;
                    tracked.rate = tracked.rate.next(downloaded, now);
                }
            }
            Event::Removed { id } => {
                self.jobs.remove(&id);
            }
            Event::SettingsChanged { settings } => self.global_limit = settings.global_limit,
            Event::Resync => return true,
            Event::Other => {}
        }
        false
    }

    /// Whether downloads are running or waiting to.
    fn busy(&self) -> bool {
        self.jobs
            .values()
            .any(|tracked| RUNNING.contains(&tracked.state.as_str()) || tracked.state == "queued")
    }

    /// What's left to run, for "when everything has finished".
    fn left(&self) -> Left {
        let mut left = Left::default();
        for tracked in self.jobs.values() {
            match tracked.state.as_str() {
                state if RUNNING.contains(&state) || state == "queued" => left.running += 1,
                "scheduled" => left.scheduled += 1,
                _ => {}
            }
        }
        left
    }

    fn summary(&self, now: Instant) -> Summary {
        let mut summary = Summary {
            connected: true,
            global_limit: self.global_limit,
            read_only: self.read_only,
            ..Summary::default()
        };
        let mut speed = 0.0;
        let mut sized = Progress::default();
        let mut sized_speed = 0.0;
        for tracked in self.jobs.values() {
            match tracked.state.as_str() {
                state if RUNNING.contains(&state) => {
                    summary.running += 1;
                    let current = tracked.rate.current(now);
                    speed += current;
                    if let Some(total) = tracked.total.filter(|&total| total > 0) {
                        sized.downloaded += tracked.downloaded.min(total);
                        sized.total += total;
                        sized_speed += current;
                    }
                }
                "queued" => summary.waiting += 1,
                "paused" => summary.paused += 1,
                _ => {}
            }
        }
        // Rounded down to whole bytes: only shown, never computed with.
        summary.speed = speed as u64;
        sized.speed = sized_speed as u64;
        summary.progress = (sized.total > 0).then_some(sized);
        summary
    }
}

/// One download's speed, smoothed as on the page (`nextRate`).
#[derive(Debug, Clone, Copy)]
struct Rate {
    at: Instant,
    downloaded: u64,
    per_second: f64,
}

impl Rate {
    fn first(downloaded: u64, now: Instant) -> Rate {
        Rate {
            at: now,
            downloaded,
            per_second: 0.0,
        }
    }

    fn next(self, downloaded: u64, now: Instant) -> Rate {
        // The download restarted from zero.
        if downloaded < self.downloaded {
            return Rate::first(downloaded, now);
        }
        let elapsed = now.saturating_duration_since(self.at);
        if elapsed < MIN_INTERVAL {
            return self;
        }
        let measured = (downloaded - self.downloaded) as f64 / elapsed.as_secs_f64();
        let per_second = if self.per_second == 0.0 {
            measured
        } else {
            self.per_second + SMOOTHING * (measured - self.per_second)
        };
        Rate {
            at: now,
            downloaded,
            per_second,
        }
    }

    fn current(&self, now: Instant) -> f64 {
        if now.saturating_duration_since(self.at) > STALE_AFTER {
            0.0
        } else {
            self.per_second
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Settings;

    fn job(id: &str, state: &str, downloaded: u64) -> Job {
        Job {
            id: id.to_owned(),
            url: format!("https://example.org/{id}"),
            file_name: Some(format!("{id}.zip")),
            state: state.to_owned(),
            downloaded,
            total: None,
            moving: false,
            problem: None,
            error: None,
        }
    }

    #[test]
    fn a_read_only_token_gets_no_actions() {
        let busy = Summary {
            connected: true,
            running: 1,
            paused: 1,
            ..Summary::default()
        };
        assert!(busy.can_pause() && busy.can_resume() && busy.can_limit());
        let looking = Summary {
            read_only: true,
            ..busy
        };
        assert!(!looking.can_pause() && !looking.can_resume() && !looking.can_limit());
        assert!(!Summary::default().can_limit());
    }

    #[test]
    fn finishing_and_failing_are_news_except_in_the_first_list() {
        let now = Instant::now();
        let mut model = Model::default();
        model.reset(
            vec![job("a", "completed", 0), job("b", "downloading", 0)],
            now,
        );
        assert!(model.news.is_empty());
        assert!(model.busy());
        let done = Event::Changed {
            job: job("b", "completed", 0),
        };
        model.apply(done, now);
        assert_eq!(
            model.news,
            [News::Finished {
                name: "b.zip".to_owned()
            }]
        );
        assert!(!model.busy());
        assert_eq!(model.left(), Left::default());
        // Missed while the daemon was away, seen in the next list.
        model.news.clear();
        model.reset(vec![job("c", "queued", 0)], now);
        model.reset(vec![job("c", "failed", 0)], now);
        assert!(matches!(model.news.as_slice(), [News::Failed { name, .. }] if name == "c.zip"));
    }

    #[test]
    fn the_summary_counts_downloads_by_state() {
        let now = Instant::now();
        let mut model = Model::default();
        model.reset(
            vec![
                job("a", "downloading", 0),
                job("b", "retrying", 0),
                job("c", "queued", 0),
                job("d", "paused", 0),
                job("e", "completed", 0),
                job("f", "scheduled", 0),
            ],
            now,
        );
        assert_eq!(
            model.left(),
            Left {
                running: 3,
                scheduled: 1
            }
        );
        model.apply(
            Event::SettingsChanged {
                settings: Settings { global_limit: 512 },
            },
            now,
        );
        let summary = model.summary(now);
        assert_eq!(
            summary,
            Summary {
                connected: true,
                running: 2,
                waiting: 1,
                paused: 1,
                speed: 0,
                global_limit: 512,
                read_only: false,
                progress: None,
            }
        );
        model.apply(Event::Removed { id: "a".to_owned() }, now);
        model.apply(
            Event::Changed {
                job: job("c", "connecting", 0),
            },
            now,
        );
        let summary = model.summary(now);
        assert_eq!((summary.running, summary.waiting), (2, 0));
    }

    #[test]
    fn the_speed_adds_up_progress_and_goes_stale() {
        let start = Instant::now();
        let mut model = Model::default();
        model.reset(
            vec![job("a", "downloading", 0), job("b", "downloading", 0)],
            start,
        );
        let second = start + Duration::from_secs(1);
        for (id, downloaded) in [("a", 1000), ("b", 3000)] {
            let progress = Event::Progress {
                id: id.to_owned(),
                downloaded,
                total: None,
            };
            model.apply(progress, second);
        }
        assert_eq!(model.summary(second).speed, 4000);
        // Smoothed: a faster second only moves it part of the way.
        let third = second + Duration::from_secs(1);
        let progress = Event::Progress {
            id: "a".to_owned(),
            downloaded: 3000,
            total: None,
        };
        model.apply(progress, third);
        assert_eq!(model.summary(third).speed, 1300 + 3000);
        // "b" has had nothing for 3.5 s, "a" for 2.5 s.
        let later = third + Duration::from_millis(2500);
        assert_eq!(model.summary(later).speed, 1300);
    }

    #[test]
    fn the_progress_adds_up_running_downloads_of_known_size() {
        let start = Instant::now();
        let mut model = Model::default();
        let sized = |id, state, downloaded, total| Job {
            total: Some(total),
            ..job(id, state, downloaded)
        };
        model.reset(
            vec![
                sized("a", "downloading", 0, 4000),
                job("b", "downloading", 0),
                sized("c", "paused", 500, 1000),
            ],
            start,
        );
        assert_eq!(
            model.summary(start).progress,
            Some(Progress {
                downloaded: 0,
                total: 4000,
                speed: 0,
            })
        );
        let second = start + Duration::from_secs(1);
        for (id, downloaded, total) in [("a", 1000, Some(4000)), ("b", 5000, None)] {
            let progress = Event::Progress {
                id: id.to_owned(),
                downloaded,
                total,
            };
            model.apply(progress, second);
        }
        // "b"'s speed counts in the total, not in the progress.
        let summary = model.summary(second);
        assert_eq!(summary.speed, 6000);
        assert_eq!(
            summary.progress,
            Some(Progress {
                downloaded: 1000,
                total: 4000,
                speed: 1000,
            })
        );
        model.apply(Event::Removed { id: "a".to_owned() }, second);
        assert_eq!(model.summary(second).progress, None);
    }

    #[test]
    fn a_restart_or_a_close_measurement_doesnt_count() {
        let start = Instant::now();
        let rate = Rate::first(5000, start);
        let soon = rate.next(9000, start + Duration::from_millis(50));
        assert_eq!(soon.downloaded, 5000);
        let restarted = rate.next(100, start + Duration::from_secs(1));
        assert_eq!(restarted.per_second, 0.0);
        assert_eq!(restarted.downloaded, 100);
    }
}
