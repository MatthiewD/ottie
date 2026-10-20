//! The job manager. One task (the "actor") owns every job; everything else
//! sends it commands through a [`Manager`] handle and listens to [`JobEvent`]s.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jiff::Zoned;
use reqwest::Url;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::backend::Backend;
use crate::category::{self, BuiltIn, Category, CategoryId, CategorySettings, Matched};
use crate::certificate::{self, CertificateId, TrustedCertificate};
use crate::download::{Download, Event, Outcome, Placement};
use crate::error::Error;
use crate::filename::{self, Existing};
use crate::id::{IdGenerator, JobId};
use crate::job::{JobChange, JobEvent, JobSnapshot, JobState, NewJob, Priority, Reorder};
use crate::links::{self, Found, LinkPreview};
use crate::playback::{self, Head, Playbacks};
use crate::problem::Problem;
use crate::settings::{RetryPolicy, Settings, SettingsChange};
use crate::slot::{Effects, Slot, SlotId, SlotSettings};
use crate::speed::SpeedHistory;
use crate::store::{JobRecord, SlotRecord, Store, Write};
use crate::throttle::Limiter;
use crate::view::{View, ViewId, ViewSettings};

/// Running jobs report progress to the manager at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// Progress of a running job is saved at most this often (state changes are
/// saved immediately).
const SAVE_INTERVAL: Duration = Duration::from_secs(2);
const COMMAND_BUFFER: usize = 64;
/// Retries wait twice as long each time, up to this (or the policy's first
/// wait, if that's longer)...
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);
/// ...unless the server's `Retry-After` asks for longer, up to this.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);
/// No wait is longer, whatever the policy says (a huge one would overflow).
const LONGEST_RETRY_WAIT: Duration = Duration::from_secs(24 * 3600);
/// Subscribers that fall further behind than this miss events (and should
/// call [`Manager::list`] to catch up).
const EVENT_BUFFER: usize = 1024;
/// Start and stop times are checked against the clock at least this often:
/// the computer may have slept, or its clock changed, while waiting.
const CLOCK_CHECK: Duration = Duration::from_secs(30);

/// Handle to the job manager. Cheap to clone; every clone talks to the same jobs.
#[derive(Clone)]
pub struct Manager {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<JobEvent>,
    playbacks: Playbacks,
}

impl Manager {
    /// Loads saved jobs and settings from `store` and starts the manager
    /// task. Jobs that were running when Ottie last stopped (quit or crash)
    /// are queued again. Must be called from inside a tokio runtime. When
    /// every handle is dropped, the manager shuts down as if
    /// [`Manager::shutdown`] was called.
    pub async fn start(backend: impl Backend, store: Store) -> Result<Manager, Error> {
        let loaded = store.load().await?;
        let mut jobs = BTreeMap::new();
        let mut last_place = 0;
        for record in loaded.jobs {
            last_place = last_place.max(record.place);
            jobs.insert(record.id, Job::restore(record).await);
        }
        let settings = loaded.settings;
        let (internal, internal_rx) = mpsc::unbounded_channel();
        let starved = internal.clone();
        let limiter = Limiter::new(settings.global_limit, move |id, waiting| {
            // Fails only once the manager has stopped.
            let _ = starved.send(Internal::Bandwidth { id, waiting });
        });
        let mut categories = BTreeMap::new();
        // The database always has it (migration 8); General would do otherwise.
        let other = loaded
            .categories
            .iter()
            .find(|c| c.built_in == Some(BuiltIn::Other))
            .map_or(CategoryId::GENERAL, |c| c.id);
        for category in loaded.categories {
            limiter.set_category_limit(category.id, category.settings.speed_limit);
            categories.insert(category.id, category.settings);
        }
        let views = loaded
            .views
            .into_iter()
            .map(|view| (view.id, view.settings))
            .collect();
        // Whether each applies is worked out as the actor starts.
        let slots = loaded
            .slots
            .into_iter()
            .map(|slot| {
                let entry = SlotEntry {
                    place: slot.place,
                    settings: slot.settings,
                    active: false,
                };
                (slot.id, entry)
            })
            .collect();
        let (commands, command_rx) = mpsc::channel(COMMAND_BUFFER);
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let playbacks = Playbacks::default();
        let actor = Actor {
            backend: Arc::new(backend),
            store,
            limiter,
            settings,
            // A removed job's ID may be higher than any left (`None` < `Some`).
            ids: IdGenerator::after(jobs.keys().next_back().copied().max(loaded.last_id)),
            jobs,
            last_place,
            categories,
            other,
            views,
            slots,
            certificates: loaded.certificates,
            effects: Effects::default(),
            ended: Vec::new(),
            yields: 0,
            wake_at: None,
            events: events.clone(),
            speed: SpeedHistory::default(),
            playbacks: playbacks.clone(),
            internal,
            shutdown: None,
        };
        tokio::spawn(actor.run(command_rx, internal_rx));
        Ok(Manager {
            commands,
            events,
            playbacks,
        })
    }

    /// Queues a download. It starts as soon as a slot is free (in total and
    /// in its category).
    pub async fn add(&self, job: NewJob) -> Result<JobId, Error> {
        self.request(|reply| Command::Add { job, reply }).await
    }

    /// Queues several downloads at once, in order (docs/decisions/0036):
    /// each one's ID, or why it was refused, as [`Manager::add`] would. The
    /// ones added are saved together. Refuses them all with
    /// [`Error::TooManyLinks`] if there are more than
    /// [`Settings::max_batch`].
    pub async fn add_many(&self, jobs: Vec<NewJob>) -> Result<Vec<Result<JobId, Error>>, Error> {
        self.request(|reply| Command::AddMany { jobs, reply }).await
    }

    /// The links in `text` (a list, an email, a page), in order and each
    /// once, with those of each sequence (docs/decisions/0037), and what
    /// adding each without a category would do (saved in `dest_dir`, if
    /// given). [`Error::TooManyLinks`] if there are more than
    /// [`Settings::max_batch`].
    pub async fn preview(
        &self,
        text: &str,
        dest_dir: Option<PathBuf>,
    ) -> Result<Vec<LinkPreview>, Error> {
        // Read here, so the manager isn't kept from its jobs meanwhile. The
        // cap is checked here too, before a sequence makes its links.
        let max = self.settings().await?.max_batch;
        // Off the async threads: a big paste takes a while to read.
        let text = text.to_owned();
        let found = tokio::task::spawn_blocking(move || links::find(&text, max))
            .await
            // Only when the runtime is shutting down: nothing in there panics.
            .map_err(|_| Error::ManagerStopped)??;
        let mut previews = self
            .request(|reply| Command::Preview {
                found,
                dest_dir,
                reply,
            })
            .await?;
        // The disk is looked at here too, all in one go off the async threads.
        previews = tokio::task::spawn_blocking(move || {
            for preview in &mut previews {
                if let LinkPreview::Link {
                    name,
                    folder: Some(folder),
                    exists,
                    ..
                } = preview
                {
                    *exists = folder.join(&*name).exists();
                }
            }
            previews
        })
        .await
        // Only when the runtime is shutting down: nothing in there panics.
        .map_err(|_| Error::ManagerStopped)?;
        Ok(previews)
    }

    /// Returns once the job has actually stopped and its `.part.ottie` file is flushed.
    pub async fn pause(&self, id: JobId) -> Result<(), Error> {
        self.request(|reply| Command::Stop {
            id,
            stop: Stop::Pause,
            reply,
        })
        .await
    }

    /// Puts a paused or failed job back in the queue; a scheduled one joins
    /// it now, without waiting for its start time.
    pub async fn resume(&self, id: JobId) -> Result<(), Error> {
        self.request(|reply| Command::Resume { id, reply }).await
    }

    /// Stops the job for good and deletes its `.part.ottie` file.
    pub async fn cancel(&self, id: JobId) -> Result<(), Error> {
        self.request(|reply| Command::Stop {
            id,
            stop: Stop::Cancel,
            reply,
        })
        .await
    }

    /// Takes the job off the list for good. A running job is stopped first
    /// (this answers once it has), and an unfinished one's `.part.ottie` file
    /// is deleted, as with `cancel`. A completed file stays on disk.
    pub async fn remove(&self, id: JobId) -> Result<(), Error> {
        self.request(|reply| Command::Stop {
            id,
            stop: Stop::Remove,
            reply,
        })
        .await
    }

    /// Every job, oldest first.
    pub async fn list(&self) -> Result<Vec<JobSnapshot>, Error> {
        self.request(|reply| Command::List { reply }).await
    }

    /// One job as it is now.
    pub async fn job(&self, id: JobId) -> Result<JobSnapshot, Error> {
        self.request(|reply| Command::Job { id, reply }).await
    }

    pub async fn settings(&self) -> Result<Settings, Error> {
        self.request(|reply| Command::Settings { reply }).await
    }

    /// The total download speed over the last hour, in bytes per second, one
    /// sample per second, oldest first (docs/decisions/0044). Starts empty
    /// when the manager starts.
    pub async fn speed_history(&self) -> Result<Vec<u64>, Error> {
        self.request(|reply| Command::SpeedHistory { reply }).await
    }

    /// Changes the settings given, all at once, and answers with all of them.
    /// Each takes effect immediately and is saved. Lowering `max_concurrent`
    /// doesn't stop running jobs; it only delays new ones. A new retry policy
    /// applies from the next failure.
    pub async fn change_settings(&self, change: SettingsChange) -> Result<Settings, Error> {
        self.request(|reply| Command::ChangeSettings { change, reply })
            .await
    }

    /// How many jobs may download at once (at least 1).
    pub async fn set_max_concurrent(&self, max: usize) -> Result<(), Error> {
        self.change(SettingsChange {
            max_concurrent: Some(max),
            ..SettingsChange::default()
        })
        .await
    }

    /// Bytes per second for all downloads together; 0 = unlimited.
    pub async fn set_global_limit(&self, bytes_per_sec: u64) -> Result<(), Error> {
        self.change(SettingsChange {
            global_limit: Some(bytes_per_sec),
            ..SettingsChange::default()
        })
        .await
    }

    pub async fn set_retry_policy(&self, policy: RetryPolicy) -> Result<(), Error> {
        self.change(SettingsChange {
            max_retries: Some(policy.max_retries),
            retries_before_yield: Some(policy.retries_before_yield),
            first_retry_wait: Some(policy.first_wait),
            ..SettingsChange::default()
        })
        .await
    }

    /// Records whether the first-start setup was done (or skipped).
    pub async fn set_setup_done(&self, done: bool) -> Result<(), Error> {
        self.change(SettingsChange {
            setup_done: Some(done),
            ..SettingsChange::default()
        })
        .await
    }

    async fn change(&self, change: SettingsChange) -> Result<(), Error> {
        self.change_settings(change).await.map(drop)
    }

    /// Changes some of a job's own settings at once, in any state; they take
    /// effect immediately, even while it runs. If anything changed, the job
    /// is saved once and one `Changed` event goes out; otherwise neither.
    /// - `speed_limit`: bytes per second; 0 = none (the global and category
    ///   limits still apply).
    /// - `priority`: its share of the global limit and its place in the queue.
    /// - `streaming`: streaming mode; once set, the file type no longer decides it.
    /// - `start_at` (docs/decisions/0040): a time to come makes the job
    ///   `Scheduled` until then, whether it was queued, paused, failed or
    ///   retrying; a running one stops first (its `Changed` comes once it
    ///   has). No time, or one already past, queues a scheduled job at once.
    /// - `stop_at`: it pauses then, wherever it is (a time already past:
    ///   now). It must come after the start time ([`Error::StopBeforeStart`]).
    ///
    /// Times can't change on a completed or cancelled job
    /// ([`Error::InvalidTransition`]), nor while its file moves.
    pub async fn change_job(&self, id: JobId, change: JobChange) -> Result<(), Error> {
        self.request(|reply| Command::ChangeJob { id, change, reply })
            .await
    }

    /// One job's own limit; see [`Manager::change_job`].
    pub async fn set_speed_limit(&self, id: JobId, bytes_per_sec: u64) -> Result<(), Error> {
        let change = JobChange {
            speed_limit: Some(bytes_per_sec),
            ..JobChange::default()
        };
        self.change_job(id, change).await
    }

    /// One job's priority; see [`Manager::change_job`].
    pub async fn set_priority(&self, id: JobId, priority: Priority) -> Result<(), Error> {
        let change = JobChange {
            priority: Some(priority),
            ..JobChange::default()
        };
        self.change_job(id, change).await
    }

    /// Moves jobs in the queue, among the waiting (queued or paused) jobs of
    /// their priority: priority still comes first, the place only orders jobs
    /// of the same one. Chosen jobs keep their order among themselves, so
    /// neighbours move as a block. Jobs that aren't waiting stay as they are;
    /// with `Before`/`After`, so do those of another priority than the target
    /// (and every job, if the target isn't waiting). Each job that moved is
    /// saved and announced with a `Changed`. An unknown ID moves nothing.
    pub async fn reorder(&self, ids: Vec<JobId>, to: Reorder) -> Result<(), Error> {
        self.request(|reply| Command::Reorder { ids, to, reply })
            .await
    }

    /// Every category, by ID. General (ID 1) is always first; Other is
    /// always there too.
    pub async fn categories(&self) -> Result<Vec<Category>, Error> {
        self.request(|reply| Command::Categories { reply }).await
    }

    /// The category a job for `url` would get if added without one: by the
    /// folder, if one is given (see [`NewJob::in_dir`]), else by the extension
    /// in the URL. The real file name, once the server answers, may still
    /// move a job matched by extension.
    pub async fn match_category(
        &self,
        url: Url,
        dest_dir: Option<PathBuf>,
    ) -> Result<Matched, Error> {
        self.request(|reply| Command::Match {
            url,
            dest_dir,
            reply,
        })
        .await
    }

    pub async fn add_category(&self, settings: CategorySettings) -> Result<CategoryId, Error> {
        self.request(|reply| Command::AddCategory { settings, reply })
            .await
    }

    /// Replaces the category's settings. Limits take effect immediately; a
    /// new folder applies to jobs added from now on.
    pub async fn update_category(
        &self,
        id: CategoryId,
        settings: CategorySettings,
    ) -> Result<(), Error> {
        self.request(|reply| Command::UpdateCategory {
            id,
            settings,
            reply,
        })
        .await
    }

    /// Moves its jobs to General (they stay where they are on disk), then
    /// removes it. General itself can't be removed.
    pub async fn remove_category(&self, id: CategoryId) -> Result<(), Error> {
        self.request(|reply| Command::RemoveCategory { id, reply })
            .await
    }

    /// Moves a job to another category, as the user's choice. If nothing has
    /// been downloaded yet, it will also be saved in that category's folder.
    pub async fn set_category(&self, id: JobId, category: CategoryId) -> Result<(), Error> {
        self.request(|reply| Command::SetCategory {
            id,
            category,
            reply,
        })
        .await
    }

    /// Every saved view, by ID.
    pub async fn views(&self) -> Result<Vec<View>, Error> {
        self.request(|reply| Command::Views { reply }).await
    }

    pub async fn add_view(&self, settings: ViewSettings) -> Result<ViewId, Error> {
        self.request(|reply| Command::AddView { settings, reply })
            .await
    }

    /// Replaces the view's settings (its name too).
    pub async fn update_view(&self, id: ViewId, settings: ViewSettings) -> Result<(), Error> {
        self.request(|reply| Command::UpdateView {
            id,
            settings,
            reply,
        })
        .await
    }

    pub async fn remove_view(&self, id: ViewId) -> Result<(), Error> {
        self.request(|reply| Command::RemoveView { id, reply })
            .await
    }

    /// The certificates trusted by hand, by ID (docs/decisions/0054).
    pub async fn certificates(&self) -> Result<Vec<TrustedCertificate>, Error> {
        self.request(|reply| Command::Certificates { reply }).await
    }

    /// Trusts this certificate (its SHA-256) for `host` from now on: FTPS
    /// downloads from there accept it. Trusting one already trusted gives it back.
    pub async fn trust_certificate(
        &self,
        host: &str,
        fingerprint: &str,
    ) -> Result<TrustedCertificate, Error> {
        let (host, fingerprint) = certificate::normalized(host, fingerprint)?;
        self.request(|reply| Command::TrustCertificate {
            host,
            fingerprint,
            reply,
        })
        .await
    }

    pub async fn distrust_certificate(&self, id: CertificateId) -> Result<(), Error> {
        self.request(|reply| Command::DistrustCertificate { id, reply })
            .await
    }

    /// Every time slot, in the order of the list (the higher one wins where
    /// two apply to the same thing). See docs/decisions/0040.
    pub async fn slots(&self) -> Result<Vec<Slot>, Error> {
        self.request(|reply| Command::Slots { reply }).await
    }

    /// Adds a slot at the end of the list. It applies at once if it covers
    /// now. Its category, if any, must exist.
    pub async fn add_slot(&self, settings: SlotSettings) -> Result<SlotId, Error> {
        self.request(|reply| Command::AddSlot { settings, reply })
            .await
    }

    /// Replaces the slot's settings; they apply at once.
    pub async fn update_slot(&self, id: SlotId, settings: SlotSettings) -> Result<(), Error> {
        self.request(|reply| Command::UpdateSlot {
            id,
            settings,
            reply,
        })
        .await
    }

    pub async fn remove_slot(&self, id: SlotId) -> Result<(), Error> {
        self.request(|reply| Command::RemoveSlot { id, reply })
            .await
    }

    /// Puts the slots in this order: every one, once
    /// ([`Error::InvalidSlotOrder`] otherwise). Each that moved is saved and
    /// announced.
    pub async fn reorder_slots(&self, ids: Vec<SlotId>) -> Result<(), Error> {
        self.request(|reply| Command::ReorderSlots { ids, reply })
            .await
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.events.subscribe()
    }

    /// A player reading the job's file: while it's running, the download
    /// goes where the player reads (docs/decisions/0029).
    pub(crate) fn head(&self, id: JobId) -> Head {
        self.playbacks.head(id)
    }

    /// Stops every running job, saves everything, then stops the manager.
    /// Running jobs keep their state in the database, so the next start
    /// continues them; jobs you paused stay paused.
    pub async fn shutdown(&self) -> Result<(), Error> {
        self.request(|reply| Command::Shutdown { reply }).await
    }

    /// Sends a command with a one-shot reply channel and waits for the answer.
    async fn request<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, Error> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| Error::ManagerStopped)?;
        answer.await.map_err(|_| Error::ManagerStopped)?
    }
}

type Reply<T> = oneshot::Sender<Result<T, Error>>;

enum Command {
    Add {
        job: NewJob,
        reply: Reply<JobId>,
    },
    AddMany {
        jobs: Vec<NewJob>,
        reply: Reply<Vec<Result<JobId, Error>>>,
    },
    Preview {
        found: Vec<Found>,
        dest_dir: Option<PathBuf>,
        reply: Reply<Vec<LinkPreview>>,
    },
    Stop {
        id: JobId,
        stop: Stop,
        reply: Reply<()>,
    },
    Resume {
        id: JobId,
        reply: Reply<()>,
    },
    List {
        reply: Reply<Vec<JobSnapshot>>,
    },
    Job {
        id: JobId,
        reply: Reply<JobSnapshot>,
    },
    Settings {
        reply: Reply<Settings>,
    },
    SpeedHistory {
        reply: Reply<Vec<u64>>,
    },
    ChangeSettings {
        change: SettingsChange,
        reply: Reply<Settings>,
    },
    ChangeJob {
        id: JobId,
        change: JobChange,
        reply: Reply<()>,
    },
    Reorder {
        ids: Vec<JobId>,
        to: Reorder,
        reply: Reply<()>,
    },
    Categories {
        reply: Reply<Vec<Category>>,
    },
    Match {
        url: Url,
        dest_dir: Option<PathBuf>,
        reply: Reply<Matched>,
    },
    AddCategory {
        settings: CategorySettings,
        reply: Reply<CategoryId>,
    },
    UpdateCategory {
        id: CategoryId,
        settings: CategorySettings,
        reply: Reply<()>,
    },
    RemoveCategory {
        id: CategoryId,
        reply: Reply<()>,
    },
    SetCategory {
        id: JobId,
        category: CategoryId,
        reply: Reply<()>,
    },
    Views {
        reply: Reply<Vec<View>>,
    },
    AddView {
        settings: ViewSettings,
        reply: Reply<ViewId>,
    },
    UpdateView {
        id: ViewId,
        settings: ViewSettings,
        reply: Reply<()>,
    },
    RemoveView {
        id: ViewId,
        reply: Reply<()>,
    },
    Certificates {
        reply: Reply<Vec<TrustedCertificate>>,
    },
    TrustCertificate {
        host: String,
        fingerprint: String,
        reply: Reply<TrustedCertificate>,
    },
    DistrustCertificate {
        id: CertificateId,
        reply: Reply<()>,
    },
    Slots {
        reply: Reply<Vec<Slot>>,
    },
    AddSlot {
        settings: SlotSettings,
        reply: Reply<SlotId>,
    },
    UpdateSlot {
        id: SlotId,
        settings: SlotSettings,
        reply: Reply<()>,
    },
    RemoveSlot {
        id: SlotId,
        reply: Reply<()>,
    },
    ReorderSlots {
        ids: Vec<SlotId>,
        reply: Reply<()>,
    },
    Shutdown {
        reply: Reply<()>,
    },
}

/// Messages from running job tasks back to the manager.
enum Internal {
    Event {
        id: JobId,
        event: Event,
    },
    Ended {
        id: JobId,
        download: Box<Download>,
        result: Result<Outcome, Error>,
    },
    /// A job's file has moved (or couldn't): its new folder and name.
    Moved {
        id: JobId,
        result: io::Result<(PathBuf, String)>,
    },
    /// A `Retrying` job's wait is over. `due` tells this wait from a later one.
    RetryDue {
        id: JobId,
        due: Instant,
    },
    /// A running job started or stopped getting no bandwidth from the limits.
    Bandwidth {
        id: JobId,
        waiting: bool,
    },
}

/// Why a running job is being stopped. Later variants win: a user's pause or
/// cancel overrides the shutdown, cancel overrides pause, remove overrides all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stop {
    /// Stop to move the file to another folder, then carry on (see
    /// `set_category`). The state stays as it is meanwhile.
    Move,
    /// Leave the state as it is, so the next start continues the job.
    Shutdown,
    /// Wait (`Scheduled`) for its start time, given while it ran, or for the
    /// end of a time slot that stops downloads.
    Hold,
    Pause,
    Cancel,
    /// Cancel (unless it completed), then forget the job.
    Remove,
}

impl Stop {
    fn target(self) -> Option<JobState> {
        match self {
            Stop::Move | Stop::Shutdown => None,
            Stop::Hold => Some(JobState::Scheduled),
            Stop::Pause => Some(JobState::Paused),
            Stop::Cancel | Stop::Remove => Some(JobState::Cancelled),
        }
    }
}

struct Job {
    state: JobState,
    download: Download,
    /// Why it failed, or the last failure while retrying.
    problem: Option<Problem>,
    priority: Priority,
    /// Its place in the queue among the jobs of its priority: lower starts first.
    place: u64,
    /// Bytes per second; 0 = no limit of its own.
    speed_limit: u64,
    category: CategoryId,
    /// The user picked the category (or a folder, which decided it), rather
    /// than it being matched by extension; it is never re-matched.
    category_chosen: bool,
    /// Streaming mode (docs/decisions/0029).
    streaming: bool,
    /// The user turned streaming mode on or off, rather than the file type
    /// deciding; it is never decided again.
    streaming_chosen: bool,
    /// Failed attempts since bytes last arrived (not saved).
    retries: u32,
    /// While `Retrying`: when it tries again.
    retry_at: Option<(Instant, SystemTime)>,
    /// 0, or the order in which it gave its slot away to a waiting job: such
    /// jobs queue behind every job that didn't (not saved).
    gave_way: u64,
    /// Running, but the speed limits give it no bandwidth (not saved).
    waiting_for_bandwidth: bool,
    /// Times it started over since the daemon started (not saved).
    restarts: u64,
    /// While `Scheduled`: when it joins the queue. Also set on a running job
    /// that is stopping to wait for it (`Stop::Hold`).
    start_at: Option<SystemTime>,
    /// When it pauses if it hasn't finished by then.
    stop_at: Option<SystemTime>,
    /// Started by hand while a time slot held it: slots leave it alone until
    /// it pauses, fails or ends.
    ignores_slots: bool,
    /// While `Scheduled`: the slot holding it, if one is. Saved, so that
    /// when the slot ends, even while Ottie isn't running, it's known
    /// whether to keep the job paused.
    held_by: Option<SlotId>,
    /// What to do when a file with its name is already in its folder;
    /// `None`: the setting's (docs/decisions/0049).
    existing: Option<Existing>,
    /// Present while a task is downloading this job.
    task: Option<Task>,
    /// Present while its category is changing and its file moving there.
    moving: Option<Moving>,
    last_saved: Option<Instant>,
}

/// A move to another category, under way (docs/decisions/0028).
struct Moving {
    category: CategoryId,
    /// The `set_category` call to answer once the file has moved.
    reply: Reply<()>,
}

/// Where a job's file is: its folder, and its name if it has one yet.
type Place = (PathBuf, Option<String>);

impl Job {
    async fn restore(record: JobRecord) -> Job {
        let mut state = record.state;
        let mut download = record.download;
        // Running (or about to retry) when Ottie stopped, whether it quit or
        // crashed: continue it.
        if state.is_running() || state == JobState::Retrying {
            state = JobState::Queued;
        }
        // Stopped by the shutdown while stopping to wait for its start time,
        // or (a database edited by hand) scheduled for nothing. Whether a
        // slot still holds it is worked out again at the first check.
        match (state, record.start_at, record.held_by) {
            (JobState::Queued, Some(_), _) => state = JobState::Scheduled,
            (JobState::Scheduled, None, None) => state = JobState::Queued,
            _ => {}
        }
        let held_by = record.held_by.filter(|_| state == JobState::Scheduled);
        // What's on disk is the truth for how much we have; the saved byte
        // count can lag behind it. With several connections the file has its
        // full size from the start, so the saved ranges tell instead.
        if !download.segments.is_empty() {
            download.downloaded = download.segments.iter().map(|s| s.done).sum();
        } else if !matches!(state, JobState::Completed | JobState::Cancelled)
            && let Some(part) = download.part_path()
        {
            download.downloaded = tokio::fs::metadata(&part).await.map_or(0, |m| m.len());
        }
        Job {
            state,
            download,
            problem: record.problem,
            priority: record.priority,
            place: record.place,
            speed_limit: record.speed_limit,
            category: record.category,
            category_chosen: record.category_chosen,
            streaming: record.streaming,
            streaming_chosen: record.streaming_chosen,
            retries: 0,
            retry_at: None,
            gave_way: 0,
            waiting_for_bandwidth: false,
            restarts: 0,
            start_at: record.start_at,
            stop_at: record.stop_at,
            ignores_slots: record.ignores_slots,
            held_by,
            existing: record.existing,
            task: None,
            moving: None,
            last_saved: None,
        }
    }

    /// Running, or waiting to retry in the slot it ran in, or stopped for a
    /// moment while its file moves.
    fn holds_slot(&self) -> bool {
        self.task.is_some()
            || self.state == JobState::Retrying
            || (self.moving.is_some() && self.state.is_running())
    }

    fn snapshot(&self, id: JobId) -> JobSnapshot {
        let dl = &self.download;
        JobSnapshot {
            id,
            url: dl.url.clone(),
            dest_dir: dl.dest_dir.clone(),
            file_name: dl.file_name.clone(),
            state: self.state,
            downloaded: dl.downloaded,
            total: dl.total,
            resumable: dl.resumable,
            problem: self.problem.clone(),
            priority: self.priority,
            place: self.place,
            speed_limit: self.speed_limit,
            streaming: self.streaming,
            category: self.category,
            retries: self.retries,
            retry_at: self.retry_at.map(|(_, at)| at),
            segments: dl.segments.clone(),
            moving: self.moving.is_some(),
            waiting_for_bandwidth: self.waiting_for_bandwidth,
            start_at: self.start_at,
            stop_at: self.stop_at,
            held_by: self.held_by,
            restarts: self.restarts,
        }
    }

    /// Its start time hasn't come yet.
    fn held(&self, now: SystemTime) -> bool {
        self.start_at.is_some_and(|at| at > now)
    }
}

struct Task {
    cancel: CancellationToken,
    stop: Option<Stop>,
    /// Pause/cancel/remove requests to answer once the task has really stopped.
    waiters: Vec<(Stop, Reply<()>)>,
}

/// A time slot as the actor keeps it.
struct SlotEntry {
    /// Its place in the list: lower comes first, and wins.
    place: u64,
    settings: SlotSettings,
    /// It applies now (worked out in `update_slots`).
    active: bool,
}

/// The task that owns all job state. Only it reads or changes `jobs`.
struct Actor<B> {
    backend: Arc<B>,
    store: Store,
    /// Saved whenever it changes.
    settings: Settings,
    limiter: Limiter,
    /// Sorted by ID, which means by creation time.
    jobs: BTreeMap<JobId, Job>,
    /// The highest place in the queue handed out; new jobs go after it.
    last_place: u64,
    /// Always holds General and Other. Saved whenever one changes.
    categories: BTreeMap<CategoryId, CategorySettings>,
    /// The ID of the built-in Other category.
    other: CategoryId,
    /// Saved views. Saved whenever one changes.
    views: BTreeMap<ViewId, ViewSettings>,
    /// Time slots. Saved whenever one changes.
    slots: BTreeMap<SlotId, SlotEntry>,
    /// Certificates trusted by hand, by ID. Saved whenever one changes.
    certificates: Vec<TrustedCertificate>,
    /// What the active slots change now (see `apply_effects`).
    effects: Effects,
    /// Slots that happened once, have just ended and keep what they held
    /// paused; only until the end of `check_times`, as they're gone.
    ended: Vec<SlotId>,
    /// How many times a retrying job has given way (see `Job::gave_way`).
    yields: u64,
    /// The next start or stop time to come, if any (see `check_times`).
    wake_at: Option<SystemTime>,
    ids: IdGenerator,
    events: broadcast::Sender<JobEvent>,
    /// The total speed, second by second (from the progress reports).
    speed: SpeedHistory,
    /// Where players read; each running job gets its own.
    playbacks: Playbacks,
    /// Cloned into each job task so it can report back.
    internal: mpsc::UnboundedSender<Internal>,
    /// `Some` once shutting down; holds the replies to send when everything has stopped.
    shutdown: Option<Vec<Reply<()>>>,
}

impl<B: Backend> Actor<B> {
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut internal: mpsc::UnboundedReceiver<Internal>,
    ) {
        self.check_times();
        self.schedule();
        let mut handles_alive = true;
        // Saving to the database failing, or working again: clients are told
        // through the settings.
        let mut failing = self.store.failing();
        let mut store_alive = true;
        let mut second = tokio::time::interval(Duration::from_secs(1));
        // After a sleep, the time missed isn't made up with a burst of samples.
        second.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            // Asked of the wall clock each time round, so a clock that
            // changed meanwhile counts.
            let wake = self.wake_at.map(|at| {
                at.duration_since(SystemTime::now())
                    .unwrap_or(Duration::ZERO)
                    .min(CLOCK_CHECK)
            });
            tokio::select! {
                // The actor holds a sender itself, so this channel never closes.
                Some(message) = internal.recv() => self.on_internal(message).await,
                command = commands.recv(), if handles_alive => match command {
                    Some(command) => self.on_command(command).await,
                    None => {
                        handles_alive = false;
                        self.begin_shutdown(None);
                    }
                },
                () = tokio::time::sleep(wake.unwrap_or(CLOCK_CHECK)), if wake.is_some() => {
                    self.check_times();
                    self.schedule();
                }
                _ = second.tick() => self.speed.tick(Instant::now()),
                changed = failing.changed(), if store_alive => match changed {
                    Ok(()) => {
                        let now = failing.borrow_and_update().clone();
                        self.storage_failing(now);
                    }
                    Err(_) => store_alive = false,
                },
            }
            if self.shutdown.is_some() && !self.any_task_running() {
                // Every save so far written, or the first that failed.
                let flushed = self.store.flush().await.map_err(storage_text);
                for reply in self.shutdown.take().unwrap_or_default() {
                    let _ = reply.send(flushed.clone().map_err(Error::StorageFailed));
                }
                return;
            }
        }
    }

    // Replies are sent with `let _ =`: sending only fails if the caller
    // stopped waiting, and then there's nobody to tell.
    async fn on_command(&mut self, command: Command) {
        match command {
            Command::Add { job, reply } => {
                let _ = reply.send(self.add(job).await);
            }
            Command::AddMany { jobs, reply } => {
                let _ = reply.send(self.add_many(jobs).await);
            }
            Command::Preview {
                found,
                dest_dir,
                reply,
            } => {
                let _ = reply.send(self.preview(found, dest_dir.as_deref()));
            }
            Command::Stop { id, stop, reply } => self.stop(id, stop, reply).await,
            Command::Resume { id, reply } => {
                let _ = reply.send(self.resume(id));
            }
            Command::List { reply } => {
                let _ = reply.send(Ok(self.list()));
            }
            Command::Job { id, reply } => {
                let snapshot = self.jobs.get(&id).map(|job| job.snapshot(id));
                let _ = reply.send(snapshot.ok_or(Error::JobNotFound(id)));
            }
            Command::Settings { reply } => {
                let _ = reply.send(Ok(self.settings.clone()));
            }
            Command::SpeedHistory { reply } => {
                let _ = reply.send(Ok(self.speed.samples()));
            }
            Command::ChangeSettings { change, reply } => {
                let _ = reply.send(self.change_settings(change).await);
            }
            Command::ChangeJob { id, change, reply } => {
                let _ = reply.send(self.change_job(id, change));
            }
            Command::Reorder { ids, to, reply } => {
                let _ = reply.send(self.reorder(&ids, to));
            }
            Command::Categories { reply } => {
                let _ = reply.send(Ok(self.categories()));
            }
            Command::Match {
                url,
                dest_dir,
                reply,
            } => {
                let _ = reply.send(Ok(self.matched(&url, dest_dir.as_deref())));
            }
            Command::AddCategory { settings, reply } => {
                let _ = reply.send(self.add_category(settings).await);
            }
            Command::UpdateCategory {
                id,
                settings,
                reply,
            } => {
                let _ = reply.send(self.update_category(id, settings).await);
            }
            Command::RemoveCategory { id, reply } => {
                let _ = reply.send(self.remove_category(id).await);
            }
            Command::SetCategory {
                id,
                category,
                reply,
            } => self.set_category(id, category, reply),
            Command::Views { reply } => {
                let _ = reply.send(Ok(self.views()));
            }
            Command::Certificates { reply } => {
                let _ = reply.send(Ok(self.certificates.clone()));
            }
            Command::TrustCertificate {
                host,
                fingerprint,
                reply,
            } => {
                let _ = reply.send(self.trust_certificate(host, fingerprint).await);
            }
            Command::DistrustCertificate { id, reply } => {
                let _ = reply.send(self.distrust_certificate(id).await);
            }
            Command::AddView { settings, reply } => {
                let _ = reply.send(self.add_view(settings).await);
            }
            Command::UpdateView {
                id,
                settings,
                reply,
            } => {
                let _ = reply.send(self.update_view(id, settings).await);
            }
            Command::RemoveView { id, reply } => {
                let _ = reply.send(self.remove_view(id).await);
            }
            Command::Slots { reply } => {
                let _ = reply.send(Ok(self.slots()));
            }
            Command::AddSlot { settings, reply } => {
                let _ = reply.send(self.add_slot(settings).await);
            }
            Command::UpdateSlot {
                id,
                settings,
                reply,
            } => {
                let _ = reply.send(self.update_slot(id, settings).await);
            }
            Command::RemoveSlot { id, reply } => {
                let _ = reply.send(self.remove_slot(id).await);
            }
            Command::ReorderSlots { ids, reply } => {
                let _ = reply.send(self.reorder_slots(&ids).await);
            }
            Command::Shutdown { reply } => self.begin_shutdown(Some(reply)),
        }
    }

    async fn on_internal(&mut self, message: Internal) {
        match message {
            Internal::Event { id, event } => self.on_event(id, event),
            Internal::Ended {
                id,
                download,
                result,
            } => self.on_ended(id, download, result).await,
            Internal::Moved { id, result } => {
                self.finish_move(id, result.map(|(dir, name)| Some((dir, Some(name)))))
            }
            Internal::RetryDue { id, due } => {
                let waiting = self.jobs.get(&id).is_some_and(|job| {
                    job.state == JobState::Retrying
                        && job.retry_at.map(|(at, _)| at) == Some(due)
                        && job.moving.is_none()
                });
                // Not if it was paused, cancelled or gave way meanwhile.
                if waiting && self.shutdown.is_none() {
                    self.start_job(id);
                }
            }
            Internal::Bandwidth { id, waiting } => {
                // Not saved: it only means something while the job runs.
                if let Some(job) = self.jobs.get_mut(&id)
                    && job.task.is_some()
                    && job.waiting_for_bandwidth != waiting
                {
                    job.waiting_for_bandwidth = waiting;
                    self.emit_changed(id);
                }
            }
        }
    }

    fn on_event(&mut self, id: JobId, event: Event) {
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        match event {
            Event::Started {
                file_name,
                dest_dir,
                total,
                validator,
                resumable,
                downloaded,
                segments,
            } => {
                // Named for the first time: the real name (from the server)
                // may belong to another category than the URL suggested, and
                // be a video when the URL didn't say.
                let extension = job
                    .download
                    .file_name
                    .is_none()
                    .then(|| filename::extension(&file_name))
                    .flatten();
                if let Some(ext) = &extension
                    && !job.category_chosen
                {
                    let category = owner_of(&self.categories, ext).unwrap_or(CategoryId::GENERAL);
                    if category != job.category {
                        job.category = category;
                        self.limiter.set_category(id, category);
                    }
                }
                if let Some(ext) = &extension
                    && !job.streaming_chosen
                {
                    job.streaming = self.settings.stream_media && playback::is_media(ext);
                    self.playbacks.of(id).set_streaming(job.streaming);
                }
                let dl = &mut job.download;
                dl.file_name = Some(file_name);
                dl.dest_dir = dest_dir;
                dl.total = total;
                dl.validator = validator;
                dl.resumable = resumable;
                dl.downloaded = downloaded;
                dl.segments = segments;
                // Save right away: from now on a crash must not lose the
                // file name or the validator needed to resume.
                self.save(id);
                self.emit_changed(id);
            }
            Event::Progress {
                downloaded,
                total,
                segments,
            } => {
                self.speed.progress(id, downloaded, Instant::now());
                job.download.downloaded = downloaded;
                job.download.total = total;
                // It works again: a later failure gets every retry anew.
                job.retries = 0;
                job.gave_way = 0;
                let save_due = job
                    .last_saved
                    .is_none_or(|at| at.elapsed() >= SAVE_INTERVAL);
                if job.state == JobState::Connecting {
                    self.transition_or_log(id, JobState::Downloading, None);
                } else if save_due {
                    self.save(id);
                }
                let _ = self.events.send(JobEvent::Progress {
                    id,
                    downloaded,
                    total,
                    segments,
                });
            }
            // Saved with the next progress save; only flushed ranges are reported.
            Event::Checkpoint { segments } => job.download.segments = segments,
            Event::Restarted(reason) => {
                job.restarts += 1;
                let _ = self.events.send(JobEvent::Restarted { id, reason });
            }
        }
    }

    /// Answers once the job is in the database: if it can't be saved, it
    /// isn't added (docs/decisions/0059).
    async fn add(&mut self, new: NewJob) -> Result<JobId, Error> {
        if self.shutdown.is_some() {
            return Err(Error::ManagerStopped);
        }
        let id = self.insert(new)?;
        if let Some(record) = self.record(id)
            && let Err(err) = self.store.write(Write::Save(Box::new(record))).await
        {
            self.jobs.remove(&id);
            return Err(err);
        }
        self.emit_changed(id);
        self.check_times();
        self.schedule();
        Ok(id)
    }

    /// Answers once they're in the database, all of them, or none if they
    /// can't be saved.
    async fn add_many(&mut self, jobs: Vec<NewJob>) -> Result<Vec<Result<JobId, Error>>, Error> {
        if self.shutdown.is_some() {
            return Err(Error::ManagerStopped);
        }
        self.check_batch(jobs.len())?;
        let results: Vec<_> = jobs.into_iter().map(|new| self.insert(new)).collect();
        let added: Vec<JobId> = results.iter().flatten().copied().collect();
        let records = added.iter().filter_map(|&id| self.record(id)).collect();
        if let Err(err) = self.store.write(Write::SaveJobs(records)).await {
            for id in &added {
                self.jobs.remove(id);
            }
            return Err(err);
        }
        for &id in &added {
            self.emit_changed(id);
        }
        self.check_times();
        self.schedule();
        Ok(results)
    }

    fn check_batch(&self, count: usize) -> Result<(), Error> {
        let max = self.settings.max_batch;
        if count > max {
            return Err(Error::TooManyLinks { count, max });
        }
        Ok(())
    }

    fn preview(
        &self,
        found: Vec<Found>,
        dest_dir: Option<&Path>,
    ) -> Result<Vec<LinkPreview>, Error> {
        self.check_batch(found.len())?;
        let known: HashSet<&str> = self
            .jobs
            .values()
            .map(|job| job.download.url.as_str())
            .collect();
        let previews = found.into_iter().map(|found| match found {
            Found::Link(url) => {
                let matched = self.matched(&url, dest_dir);
                let folder = match dest_dir {
                    Some(dir) => Some(dir.to_owned()),
                    None => self
                        .categories
                        .get(&matched.category)
                        .and_then(|c| c.folder.clone()),
                };
                LinkPreview::Link {
                    name: filename::from_url(&url),
                    matched,
                    known: known.contains(url.as_str()),
                    folder,
                    exists: false, // looked at by `Manager::preview`
                    url,
                }
            }
            Found::Invalid { text, error } => LinkPreview::Invalid { text, error },
        });
        Ok(previews.collect())
    }

    /// Where a job for `url` would go if added without a category.
    fn matched(&self, url: &Url, dest_dir: Option<&Path>) -> Matched {
        let (category, by_folder) = match dest_dir {
            Some(dir) => (self.category_of_folder(url, dir), true),
            None => (self.match_category(url), false),
        };
        Matched {
            category,
            extension: category::url_extension(url),
            by_folder,
        }
    }

    /// Puts a new job in the list (not saved or announced yet).
    fn insert(&mut self, new: NewJob) -> Result<JobId, Error> {
        // Fixed (never re-matched) when the user picked the category, or a
        // folder, which then decides it.
        let (category, fixed) = match (new.category, &new.dest_dir) {
            (Some(category), _) => (category, true),
            (None, Some(dir)) => (self.category_of_folder(&new.url, dir), true),
            (None, None) => (self.match_category(&new.url), false),
        };
        let settings = self.category(category)?;
        let dest_dir = match new.dest_dir {
            Some(dir) => dir,
            None => settings
                .folder
                .clone()
                .ok_or_else(|| Error::NoFolder(settings.name.clone()))?,
        };
        if dest_dir.to_str().is_none() {
            return Err(Error::NonUtf8Path(dest_dir));
        }
        // A start time already past means now.
        let start_at = new.start_at.filter(|&at| at > SystemTime::now());
        if let (Some(start), Some(stop)) = (start_at, new.stop_at)
            && stop <= start
        {
            return Err(Error::StopBeforeStart);
        }
        // A slot that stops its category holds it from the start.
        let held_by = self.effects.stop(category).map(|(slot, _)| slot);
        let state = if start_at.is_some() || held_by.is_some() {
            JobState::Scheduled
        } else {
            JobState::Queued
        };
        let streaming = new.streaming.unwrap_or_else(|| {
            self.settings.stream_media
                && category::url_extension(&new.url).is_some_and(|ext| playback::is_media(&ext))
        });
        new.headers.check()?;
        let mut download = Download::new(new.url, dest_dir)?;
        download.headers = new.headers;
        let id = self.ids.next(unix_now());
        self.last_place += 1;
        self.jobs.insert(
            id,
            Job {
                state,
                download,
                problem: None,
                priority: Priority::default(),
                place: self.last_place,
                speed_limit: 0,
                category,
                category_chosen: fixed,
                streaming,
                streaming_chosen: new.streaming.is_some(),
                retries: 0,
                retry_at: None,
                gave_way: 0,
                waiting_for_bandwidth: false,
                restarts: 0,
                start_at,
                stop_at: new.stop_at,
                ignores_slots: false,
                held_by,
                existing: new.existing,
                task: None,
                moving: None,
                last_saved: None,
            },
        );
        Ok(id)
    }

    /// Changes a setting of one job (any state), saves it and tells
    /// subscribers. A queued job's new priority counts at the next `schedule`.
    /// Applies what differs from the job's settings, then tells the limiter.
    fn change_job(&mut self, id: JobId, change: JobChange) -> Result<(), Error> {
        let job = self.jobs.get_mut(&id).ok_or(Error::JobNotFound(id))?;
        let JobChange {
            speed_limit,
            priority,
            streaming,
            start_at,
            stop_at,
            existing,
        } = change;
        let speed_limit = speed_limit.filter(|&limit| limit != job.speed_limit);
        let existing = existing.filter(|&existing| Some(existing) != job.existing);
        let priority = priority.filter(|&priority| priority != job.priority);
        // Chosen now, even if it stays as it was: the file type no longer decides.
        let streaming = streaming.filter(|&on| on != job.streaming || !job.streaming_chosen);
        // A start time already past means now: none.
        let now = SystemTime::now();
        let start_at = start_at
            .map(|at| at.filter(|&at| at > now))
            .filter(|&at| at != job.start_at);
        let stop_at = stop_at.filter(|&at| at != job.stop_at);
        let times = start_at.is_some() || stop_at.is_some();
        if times {
            if job.state.is_finished() {
                let to = if start_at.is_some() {
                    JobState::Scheduled
                } else {
                    JobState::Paused
                };
                return Err(Error::InvalidTransition {
                    from: job.state,
                    to,
                });
            }
            if job.moving.is_some() {
                return Err(Error::JobMoving(id));
            }
            if let (Some(start), Some(stop)) = (
                start_at.unwrap_or(job.start_at),
                stop_at.unwrap_or(job.stop_at),
            ) && stop <= start
            {
                return Err(Error::StopBeforeStart);
            }
        }
        if speed_limit.is_none()
            && priority.is_none()
            && streaming.is_none()
            && existing.is_none()
            && !times
        {
            return Ok(());
        }
        if existing.is_some() {
            job.existing = existing;
        }
        if let Some(limit) = speed_limit {
            job.speed_limit = limit;
        }
        if let Some(priority) = priority {
            job.priority = priority;
        }
        if let Some(on) = streaming {
            job.streaming = on;
            job.streaming_chosen = true;
        }
        if let Some(at) = stop_at {
            job.stop_at = at;
        }
        // What a new start time does to its state, if anything.
        let mut next = None;
        if let Some(at) = start_at {
            job.start_at = at;
            match (at, job.state) {
                (Some(_), JobState::Paused | JobState::Failed) => {
                    // Started again by hand, later: every retry is available.
                    job.retries = 0;
                    job.gave_way = 0;
                    next = Some(JobState::Scheduled);
                }
                (Some(_), JobState::Queued | JobState::Retrying) => {
                    next = Some(JobState::Scheduled);
                }
                (Some(_), _) => {
                    // Running: it waits once it has stopped (see `on_ended`).
                    if let Some(task) = &mut job.task {
                        task.stop = task.stop.max(Some(Stop::Hold));
                        task.cancel.cancel();
                    }
                }
                (None, JobState::Scheduled) => next = Some(JobState::Queued),
                (None, _) => {}
            }
        }
        // One save and one `Changed`, with the new state if it has one.
        match next {
            Some(state) => self.transition(id, state, None)?,
            None => {
                self.save(id);
                self.emit_changed(id);
            }
        }
        if times {
            self.check_times();
            self.schedule();
        }
        if let Some(on) = streaming {
            // A running download changes its order at once.
            self.playbacks.of(id).set_streaming(on);
        }
        if let Some(limit) = speed_limit {
            self.limiter.set_limit(id, limit);
        }
        if let Some(priority) = priority {
            self.limiter.set_priority(id, priority);
        }
        Ok(())
    }

    /// See [`Manager::reorder`]. Each priority's waiting jobs swap places
    /// among themselves, so every other job keeps its own.
    fn reorder(&mut self, ids: &[JobId], to: Reorder) -> Result<(), Error> {
        let known = |id: &JobId| self.jobs.contains_key(id);
        if let Some(id) = ids.iter().copied().chain(to.target()).find(|id| !known(id)) {
            return Err(Error::JobNotFound(id));
        }
        let chosen: HashSet<JobId> = ids.iter().copied().collect();
        let mut lanes: HashMap<Priority, Vec<(u64, JobId)>> = HashMap::new();
        for (&id, job) in &self.jobs {
            if job.state.is_waiting() {
                lanes.entry(job.priority).or_default().push((job.place, id));
            }
        }
        let mut moved = Vec::new();
        for mut lane in lanes.into_values() {
            if !lane.iter().any(|(_, id)| chosen.contains(id)) {
                continue;
            }
            lane.sort_unstable();
            let (places, mut order): (Vec<u64>, Vec<JobId>) = lane.into_iter().unzip();
            to.apply(&mut order, |id| chosen.contains(id));
            for (place, id) in places.into_iter().zip(order) {
                if let Some(job) = self.jobs.get_mut(&id)
                    && job.place != place
                {
                    job.place = place;
                    moved.push(id);
                }
            }
        }
        // In ID order, so the events always come in the same order.
        moved.sort_unstable();
        for id in moved {
            self.save(id);
            self.emit_changed(id);
        }
        Ok(())
    }

    fn change(&mut self, id: JobId, edit: impl FnOnce(&mut Job)) -> Result<(), Error> {
        let job = self.jobs.get_mut(&id).ok_or(Error::JobNotFound(id))?;
        edit(job);
        self.save(id);
        self.emit_changed(id);
        Ok(())
    }

    /// The category whose extensions include the one in the URL, else General.
    fn match_category(&self, url: &Url) -> CategoryId {
        category::url_extension(url)
            .and_then(|ext| owner_of(&self.categories, &ext))
            .unwrap_or(CategoryId::GENERAL)
    }

    /// The category whose folder `dir` is: the one the extension matches if
    /// its folder is `dir`, else the first with that folder, else Other.
    fn category_of_folder(&self, url: &Url, dir: &Path) -> CategoryId {
        let in_dir = |id: &CategoryId| {
            self.categories
                .get(id)
                .and_then(|c| c.folder.as_deref())
                .is_some_and(|folder| category::same_folder(folder, dir))
        };
        let by_extension = self.match_category(url);
        if in_dir(&by_extension) {
            return by_extension;
        }
        self.categories
            .keys()
            .find(|id| in_dir(id))
            .copied()
            .unwrap_or(self.other)
    }

    fn built_in(&self, id: CategoryId) -> Option<BuiltIn> {
        if id == CategoryId::GENERAL {
            Some(BuiltIn::General)
        } else if id == self.other {
            Some(BuiltIn::Other)
        } else {
            None
        }
    }

    /// For a job's first run, if its category was matched automatically and
    /// it's saved in that category's folder: where to save it instead once
    /// its real name is known. Mirrors the re-match in `on_event`.
    fn placement(&self, job: &Job) -> Option<Placement> {
        let here = &job.download.dest_dir;
        let category_folder = self.categories.get(&job.category)?.folder.as_ref();
        if job.download.file_name.is_some() || job.category_chosen || category_folder != Some(here)
        {
            return None;
        }
        // A category without a folder of its own leaves the job where it is.
        let folder_of = |c: &CategorySettings| c.folder.clone().unwrap_or_else(|| here.clone());
        let by_extension = self
            .categories
            .values()
            .flat_map(|c| {
                c.extensions
                    .iter()
                    .map(move |ext| (ext.clone(), folder_of(c)))
            })
            .collect();
        let otherwise = self
            .categories
            .get(&CategoryId::GENERAL)
            .map_or_else(|| here.clone(), folder_of);
        Some(Placement {
            by_extension,
            otherwise,
        })
    }

    fn category(&self, id: CategoryId) -> Result<&CategorySettings, Error> {
        self.categories.get(&id).ok_or(Error::CategoryNotFound(id))
    }

    fn categories(&self) -> Vec<Category> {
        self.categories
            .iter()
            .map(|(&id, settings)| Category {
                id,
                settings: settings.clone(),
                built_in: self.built_in(id),
            })
            .collect()
    }

    /// Normalizes `settings` and checks that its name and extensions aren't
    /// used by another category (`own` is the one being edited, if any).
    fn check_category(
        &self,
        settings: CategorySettings,
        own: Option<CategoryId>,
    ) -> Result<CategorySettings, Error> {
        let settings = settings.normalized()?;
        let name = settings.name.to_lowercase();
        for (&id, other) in &self.categories {
            if Some(id) == own {
                continue;
            }
            if other.name.to_lowercase() == name {
                return Err(Error::CategoryNameTaken(settings.name));
            }
            if let Some(ext) = settings
                .extensions
                .iter()
                .find(|ext| other.extensions.contains(ext))
            {
                return Err(Error::ExtensionTaken {
                    extension: ext.clone(),
                    category: other.name.clone(),
                });
            }
        }
        Ok(settings)
    }

    async fn add_category(&mut self, settings: CategorySettings) -> Result<CategoryId, Error> {
        let settings = self.check_category(settings, None)?;
        // General always exists, so there is a last ID.
        let last = self.categories.keys().next_back().map_or(0, |id| id.0);
        let id = CategoryId(last.checked_add(1).ok_or(Error::TooManyCategories)?);
        self.put_category(id, settings).await?;
        Ok(id)
    }

    /// Saves it, then keeps it and tells the subscribers.
    async fn put_category(
        &mut self,
        id: CategoryId,
        settings: CategorySettings,
    ) -> Result<(), Error> {
        let category = Category {
            id,
            settings,
            built_in: self.built_in(id),
        };
        self.store
            .write(Write::SaveCategory(category.clone()))
            .await?;
        self.categories.insert(id, category.settings.clone());
        self.apply_limits();
        // Fails only when nobody is subscribed.
        let _ = self.events.send(JobEvent::CategoryChanged(category));
        Ok(())
    }

    async fn update_category(
        &mut self,
        id: CategoryId,
        mut settings: CategorySettings,
    ) -> Result<(), Error> {
        self.category(id)?;
        if id == self.other {
            // Its jobs are the ones saved outside every category's folder.
            settings.folder = None;
            settings.extensions.clear();
        }
        let settings = self.check_category(settings, Some(id))?;
        self.put_category(id, settings).await?;
        // A higher cap on running jobs may let queued ones start.
        self.schedule();
        Ok(())
    }

    /// Its jobs, views and slots are saved first (the database refuses to
    /// drop a category still in use), then its row is deleted, awaited. If
    /// that fails, the category comes back; its jobs stay in General.
    async fn remove_category(&mut self, id: CategoryId) -> Result<(), Error> {
        let settings = self.category(id)?;
        if self.built_in(id).is_some() {
            return Err(Error::CannotRemoveBuiltIn(settings.name.clone()));
        }
        let members: Vec<JobId> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.category == id)
            .map(|(job_id, _)| *job_id)
            .collect();
        // Moved and saved before the category row is deleted, which the
        // database would refuse while jobs still point at it.
        for job_id in members {
            self.change(job_id, |job| {
                job.category = CategoryId::GENERAL;
                job.category_chosen = false;
            })?;
            self.limiter.set_category(job_id, CategoryId::GENERAL);
        }
        // Views that showed it show General instead, where its jobs went.
        // Saved first too: the database would drop it from them otherwise.
        let showing: Vec<ViewId> = self
            .views
            .iter()
            .filter(|(_, view)| {
                view.categories
                    .as_ref()
                    .is_some_and(|ids| ids.contains(&id))
            })
            .map(|(view_id, _)| *view_id)
            .collect();
        for view_id in showing {
            if let Some(ids) = self
                .views
                .get_mut(&view_id)
                .and_then(|view| view.categories.as_mut())
            {
                for each in ids.iter_mut().filter(|each| **each == id) {
                    *each = CategoryId::GENERAL;
                }
                ids.sort();
                ids.dedup();
            }
            self.save_view(view_id);
        }
        // Its time slots go with it, before its row (which they point at).
        let slots: Vec<SlotId> = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.settings.category == Some(id))
            .map(|(slot_id, _)| *slot_id)
            .collect();
        for slot_id in slots {
            self.drop_slot(slot_id);
        }
        let Some(removed) = self.categories.remove(&id) else {
            return Err(Error::CategoryNotFound(id));
        };
        self.limiter.set_category_limit(id, 0);
        if let Err(err) = self.store.write(Write::RemoveCategory(id)).await {
            self.categories.insert(id, removed);
            self.apply_limits();
            self.check_times();
            self.schedule();
            return Err(err);
        }
        // Fails only when nobody is subscribed.
        let _ = self.events.send(JobEvent::CategoryRemoved(id));
        self.check_times();
        self.schedule();
        Ok(())
    }

    /// Moves a job to another category, as the user's choice, and its file
    /// to that category's folder (docs/decisions/0028). A running job stops,
    /// its file moves, and it carries on in the same slot. Answers once the
    /// file has moved; if it can't, the job stays in its old category.
    fn set_category(&mut self, id: JobId, category: CategoryId, reply: Reply<()>) {
        let refusal = if self.shutdown.is_some() {
            Some(Error::ManagerStopped)
        } else {
            self.category(category).err()
        };
        if let Some(err) = refusal {
            let _ = reply.send(Err(err));
            return;
        }
        let folder = self
            .categories
            .get(&category)
            .and_then(|c| c.folder.clone());
        let Some(job) = self.jobs.get_mut(&id) else {
            let _ = reply.send(Err(Error::JobNotFound(id)));
            return;
        };
        if job.moving.is_some() {
            let _ = reply.send(Err(Error::JobMoving(id)));
            return;
        }
        let dl = &job.download;
        let has_file = dl.file_name.is_some() && job.state != JobState::Cancelled;
        // What it has would be lost if it stopped: it would start over.
        let would_restart = has_file && !dl.resumable && dl.downloaded > 0;
        // No folder of its own (it keeps the one it has), or already there.
        let Some(folder) = folder.filter(|folder| !category::same_folder(folder, &dl.dest_dir))
        else {
            let _ = reply.send(self.assign_category(id, category, None));
            self.schedule();
            return;
        };
        if let Some(task) = &mut job.task {
            if would_restart {
                let _ = reply.send(Err(Error::CannotMoveWhileRunning(id)));
                return;
            }
            // Running: stop it first; `on_ended` moves the file.
            task.stop = task.stop.max(Some(Stop::Move));
            task.cancel.cancel();
            job.moving = Some(Moving { category, reply });
            self.emit_changed(id);
            return;
        }
        if !has_file {
            // Nothing on disk yet: it will simply be saved there.
            let _ = reply.send(self.assign_category(id, category, Some((folder, None))));
            self.schedule();
            return;
        }
        job.moving = Some(Moving { category, reply });
        self.emit_changed(id);
        self.move_file(id);
    }

    /// Moves the file of a stopped job whose category is changing, in a task
    /// of its own (between drives it's a copy, which takes a while). With no
    /// file, or no folder to move to, the move is done at once.
    fn move_file(&mut self, id: JobId) {
        let Some(job) = self.jobs.get(&id) else {
            return;
        };
        let Some(moving) = &job.moving else {
            return;
        };
        let folder = self
            .categories
            .get(&moving.category)
            .and_then(|settings| settings.folder.clone());
        let name = job
            .download
            .file_name
            .clone()
            .filter(|_| job.state != JobState::Cancelled);
        let (Some(to), Some(name)) = (folder.clone(), name) else {
            self.finish_move(id, Ok(folder.map(|dir| (dir, None))));
            return;
        };
        let from = job.download.dest_dir.clone();
        let finished = job.state == JobState::Completed;
        let internal = self.internal.clone();
        supervised(
            async move {
                filename::relocate(&from, &name, finished, &to)
                    .await
                    .map(|name| (to, name))
            },
            move |moved| {
                let result = moved.unwrap_or_else(|panic| Err(io::Error::other(panic)));
                // Fails only if the manager has stopped.
                let _ = internal.send(Internal::Moved { id, result });
            },
        );
    }

    /// Ends a move: the job takes its new category and place, or stays as it
    /// was if the file couldn't move. A job that was running carries on.
    fn finish_move(&mut self, id: JobId, result: io::Result<Option<Place>>) {
        let Some(moving) = self.jobs.get_mut(&id).and_then(|job| job.moving.take()) else {
            return;
        };
        let answer = match result {
            Ok(place) => {
                // Removed meanwhile: the file stays where it went, and the job
                // goes to General like the category's other jobs.
                let category = if self.categories.contains_key(&moving.category) {
                    moving.category
                } else {
                    CategoryId::GENERAL
                };
                self.assign_category(id, category, place)
            }
            Err(err) => {
                tracing::warn!(%id, error = %err, "could not move the file");
                self.emit_changed(id);
                Err(Error::MoveFailed(err))
            }
        };
        let _ = moving.reply.send(answer);
        let Some(state) = self.jobs.get(&id).map(|job| job.state) else {
            return;
        };
        if self.shutdown.is_none() && state.is_running() {
            self.spawn_task(id);
        } else if state == JobState::Retrying {
            // Its wait may have ended while it was moving.
            self.retry_when_due(id);
        }
        // Its times were left alone while it moved.
        self.check_times();
        self.schedule();
    }

    /// Puts the job in `category` as the user's choice, and at `place` if
    /// given. The caller schedules (a category cap may have changed).
    fn assign_category(
        &mut self,
        id: JobId,
        category: CategoryId,
        place: Option<Place>,
    ) -> Result<(), Error> {
        self.change(id, |job| {
            job.category = category;
            job.category_chosen = true;
            if let Some((dir, name)) = place {
                job.download.dest_dir = dir;
                if name.is_some() {
                    job.download.file_name = name;
                }
            }
        })?;
        self.limiter.set_category(id, category);
        Ok(())
    }

    async fn trust_certificate(
        &mut self,
        host: String,
        fingerprint: String,
    ) -> Result<TrustedCertificate, Error> {
        if let Some(known) = self
            .certificates
            .iter()
            .find(|c| c.host == host && c.fingerprint == fingerprint)
        {
            return Ok(known.clone());
        }
        let last = self.certificates.iter().map(|c| c.id.0).max().unwrap_or(0);
        let id = CertificateId(last.checked_add(1).ok_or(Error::TooManyCertificates)?);
        let trusted = TrustedCertificate {
            id,
            host,
            fingerprint,
        };
        self.store
            .write(Write::SaveCertificate(trusted.clone()))
            .await?;
        self.certificates.push(trusted.clone());
        Ok(trusted)
    }

    async fn distrust_certificate(&mut self, id: CertificateId) -> Result<(), Error> {
        if !self.certificates.iter().any(|c| c.id == id) {
            return Err(Error::CertificateNotFound(id));
        }
        self.store.write(Write::RemoveCertificate(id)).await?;
        self.certificates.retain(|c| c.id != id);
        Ok(())
    }

    /// The fingerprints trusted by hand for the server of `url`.
    fn trusted_for(&self, url: &Url) -> Vec<String> {
        let host = url.host_str().unwrap_or("").to_ascii_lowercase();
        self.certificates
            .iter()
            .filter(|c| c.host == host)
            .map(|c| c.fingerprint.clone())
            .collect()
    }

    fn views(&self) -> Vec<View> {
        self.views
            .iter()
            .map(|(&id, settings)| View {
                id,
                settings: settings.clone(),
            })
            .collect()
    }

    /// Normalizes `settings` and checks that its name isn't another view's
    /// (`own` is the one being edited, if any) and its categories exist.
    fn check_view(
        &self,
        settings: ViewSettings,
        own: Option<ViewId>,
    ) -> Result<ViewSettings, Error> {
        let settings = settings.normalized()?;
        let name = settings.name.to_lowercase();
        if self
            .views
            .iter()
            .any(|(&id, other)| Some(id) != own && other.name.to_lowercase() == name)
        {
            return Err(Error::ViewNameTaken(settings.name));
        }
        for &category in settings.categories.iter().flatten() {
            self.category(category)?;
        }
        Ok(settings)
    }

    // Views, categories, certificates and slots are saved first: if that
    // fails, nothing changes (docs/decisions/0059).

    async fn add_view(&mut self, settings: ViewSettings) -> Result<ViewId, Error> {
        let settings = self.check_view(settings, None)?;
        let last = self.views.keys().next_back().map_or(0, |id| id.0);
        let id = ViewId(last.checked_add(1).ok_or(Error::TooManyViews)?);
        self.put_view(id, settings).await?;
        Ok(id)
    }

    async fn update_view(&mut self, id: ViewId, settings: ViewSettings) -> Result<(), Error> {
        if !self.views.contains_key(&id) {
            return Err(Error::ViewNotFound(id));
        }
        let settings = self.check_view(settings, Some(id))?;
        self.put_view(id, settings).await
    }

    /// Saves it, then keeps it and tells the subscribers.
    async fn put_view(&mut self, id: ViewId, settings: ViewSettings) -> Result<(), Error> {
        let view = View { id, settings };
        self.store.write(Write::SaveView(view.clone())).await?;
        self.views.insert(id, view.settings.clone());
        // Fails only when nobody is subscribed.
        let _ = self.events.send(JobEvent::ViewChanged(view));
        Ok(())
    }

    async fn remove_view(&mut self, id: ViewId) -> Result<(), Error> {
        if !self.views.contains_key(&id) {
            return Err(Error::ViewNotFound(id));
        }
        self.store.write(Write::RemoveView(id)).await?;
        self.views.remove(&id);
        // Fails only when nobody is subscribed.
        let _ = self.events.send(JobEvent::ViewRemoved(id));
        Ok(())
    }

    fn save_view(&self, id: ViewId) {
        let Some(settings) = self.views.get(&id) else {
            return;
        };
        let view = View {
            id,
            settings: settings.clone(),
        };
        self.store.queue(Write::SaveView(view.clone()));
        // Fails only when nobody is subscribed.
        let _ = self.events.send(JobEvent::ViewChanged(view));
    }

    async fn stop(&mut self, id: JobId, stop: Stop, reply: Reply<()>) {
        let Some(job) = self.jobs.get_mut(&id) else {
            let _ = reply.send(Err(Error::JobNotFound(id)));
            return;
        };
        if job.moving.is_some() && stop != Stop::Shutdown {
            let _ = reply.send(Err(Error::JobMoving(id)));
            return;
        }
        let Some(target) = stop.target() else {
            let _ = reply.send(Ok(()));
            return;
        };
        if let Some(task) = &mut job.task {
            // Running: ask it to stop, and answer once it has (see `on_ended`).
            task.stop = task.stop.max(Some(stop));
            task.cancel.cancel();
            task.waiters.push((stop, reply));
            return;
        }
        let finished = matches!(job.state, JobState::Completed | JobState::Cancelled);
        let mut result = if stop == Stop::Remove && finished {
            Ok(())
        } else {
            self.transition(id, target, None)
        };
        if result.is_ok() {
            if matches!(stop, Stop::Cancel | Stop::Remove) && !finished {
                self.delete_part(id).await;
            }
            if stop == Stop::Remove {
                result = self.forget(id).await;
            }
            // A retrying job held a slot, which is now free.
            self.schedule();
        }
        let _ = reply.send(result);
    }

    fn resume(&mut self, id: JobId) -> Result<(), Error> {
        if self.shutdown.is_some() {
            return Err(Error::ManagerStopped);
        }
        if self.jobs.get(&id).is_some_and(|job| job.moving.is_some()) {
            return Err(Error::JobMoving(id));
        }
        // Started by hand while a slot stops downloads: it runs anyway.
        let held = self.hold_of(id).is_some();
        self.transition(id, JobState::Queued, None)?;
        // Resumed by hand: every retry is available again.
        if let Some(job) = self.jobs.get_mut(&id) {
            job.retries = 0;
            job.gave_way = 0;
            if held {
                job.ignores_slots = true;
                self.save(id);
            }
        }
        self.schedule();
        Ok(())
    }

    /// Applies `change`; if anything changed, saves the settings, puts them
    /// to use and tells the subscribers.
    /// Saving to the database fails (with this text), or works again.
    fn storage_failing(&mut self, now: Option<String>) {
        if self.settings.storage_failing != now {
            self.settings.storage_failing = now;
            let _ = self
                .events
                .send(JobEvent::SettingsChanged(self.settings.clone()));
        }
    }

    /// Saved first: if it can't be, nothing changes.
    async fn change_settings(&mut self, change: SettingsChange) -> Result<Settings, Error> {
        let mut next = self.settings.clone();
        next.apply(change);
        if next != self.settings {
            self.store.write(Write::SaveSettings(next.clone())).await?;
            let before = std::mem::replace(&mut self.settings, next);
            if self.settings.global_limit != before.global_limit {
                self.apply_limits();
            }
            // More slots, or retrying jobs that may now give way.
            self.schedule();
            let _ = self
                .events
                .send(JobEvent::SettingsChanged(self.settings.clone()));
        }
        Ok(self.settings.clone())
    }

    fn list(&self) -> Vec<JobSnapshot> {
        self.jobs
            .iter()
            .map(|(id, job)| job.snapshot(*id))
            .collect()
    }

    /// Starts queued jobs until `max_concurrent` are running, skipping jobs
    /// whose category is at its own cap: the highest priority first, and by
    /// place in the queue within a priority.
    ///
    /// When every slot is taken, a `Retrying` job that has used its in-slot
    /// retries gives its slot to a queued job, and queues behind the others.
    fn schedule(&mut self) {
        if self.shutdown.is_some() {
            return;
        }
        loop {
            let mut busy: HashMap<CategoryId, usize> = HashMap::new();
            for job in self.jobs.values().filter(|j| j.holds_slot()) {
                *busy.entry(job.category).or_default() += 1;
            }
            if busy.values().sum::<usize>() < self.max_concurrent() {
                let Some(id) = self.next_queued(&busy) else {
                    break;
                };
                if !self.start_job(id) {
                    break;
                }
            } else {
                let Some(id) = self.ready_to_give_way(&busy) else {
                    break;
                };
                self.yields += 1;
                let problem = self.jobs.get_mut(&id).and_then(|job| {
                    job.gave_way = self.yields;
                    job.problem.clone()
                });
                if !self.transition_or_log(id, JobState::Queued, problem) {
                    break;
                }
            }
        }
    }

    /// The queued job to start next, among those whose category has room:
    /// jobs that gave their slot away come last (whatever their priority, or
    /// they would take it straight back); then the highest priority, the
    /// lowest place.
    fn next_queued(&self, busy: &HashMap<CategoryId, usize>) -> Option<JobId> {
        self.jobs
            .iter()
            .filter(|(_, job)| {
                job.state == JobState::Queued
                    && job.moving.is_none()
                    && self.has_room(job.category, busy)
            })
            // `Reverse`: the smallest `gave_way` and place win.
            .max_by_key(|(_, job)| {
                let never_gave_way = job.gave_way == 0;
                (
                    never_gave_way,
                    job.priority,
                    Reverse(job.gave_way),
                    Reverse(job.place),
                )
            })
            .map(|(id, _)| *id)
    }

    /// A `Retrying` job past its in-slot retries whose slot a queued job could use:
    /// one `next_queued` would start in its place. Not one it wouldn't (a job
    /// whose file is moving), or the retrying job would take its slot straight
    /// back, without its wait.
    fn ready_to_give_way(&self, busy: &HashMap<CategoryId, usize>) -> Option<JobId> {
        let in_slot = self.settings.retry.retries_before_yield;
        self.jobs
            .iter()
            .filter(|(_, job)| {
                job.state == JobState::Retrying && job.retries > in_slot && job.moving.is_none()
            })
            .find(|(_, job)| {
                let mut after = busy.clone();
                if let Some(count) = after.get_mut(&job.category) {
                    *count = count.saturating_sub(1);
                }
                self.next_queued(&after).is_some()
            })
            .map(|(id, _)| *id)
    }

    /// Follows the clock (docs/decisions/0040): which time slots apply, and
    /// what they change; pauses jobs whose stop time has come; holds or
    /// releases each job as the slots and its start time say. Then notes the
    /// next time to look. Jobs whose file is moving wait until it has. The
    /// caller schedules.
    fn check_times(&mut self) {
        if self.shutdown.is_some() {
            self.wake_at = None;
            return;
        }
        let now = SystemTime::now();
        let zoned = Zoned::now();
        self.update_slots(&zoned);
        let due: Vec<JobId> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.moving.is_none() && job.stop_at.is_some_and(|at| at <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.stop_time_came(id);
        }
        let ids: Vec<JobId> = self.jobs.keys().copied().collect();
        for id in ids {
            self.settle(id, now);
        }
        self.ended.clear();
        let next_job_time = self
            .jobs
            .values()
            .filter(|job| job.moving.is_none())
            .flat_map(|job| {
                let start = job.start_at.filter(|_| job.state == JobState::Scheduled);
                [start, job.stop_at]
            })
            .flatten()
            .min();
        let next_slot_change = self
            .slots
            .values()
            .filter_map(|slot| slot.settings.next_change(&zoned))
            .min()
            .map(SystemTime::from);
        self.wake_at = next_job_time.into_iter().chain(next_slot_change).min();
    }

    /// A job's stop time has come: it pauses, wherever it is.
    fn stop_time_came(&mut self, id: JobId) {
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        job.stop_at = None;
        if let Some(task) = &mut job.task {
            // Saved with the state it stops in (see `on_ended`).
            task.stop = task.stop.max(Some(Stop::Pause));
            task.cancel.cancel();
        } else if matches!(
            job.state,
            JobState::Queued | JobState::Scheduled | JobState::Retrying
        ) {
            self.transition_or_log(id, JobState::Paused, None);
        } else {
            // Already stopped: only the time goes.
            self.save(id);
            self.emit_changed(id);
        }
    }

    /// Puts a job where the slots and its start time say: waiting
    /// (`Scheduled`) or in the queue, or paused if the slot that held it has
    /// ended and says so. A running one that a slot stops is asked to stop
    /// (`Stop::Hold`), unless the slot lets it finish.
    fn settle(&mut self, id: JobId, now: SystemTime) {
        let hold = self.hold_of(id);
        let keep_paused = self
            .jobs
            .get(&id)
            .is_some_and(|job| self.keeps_paused(job.held_by));
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        if job.moving.is_some() {
            return;
        }
        let slot = hold.map(|(slot, _)| slot);
        let stops = hold.is_some_and(|(_, let_finish)| !let_finish);
        match job.state {
            JobState::Scheduled if slot.is_none() && !job.held(now) => {
                let next = if keep_paused {
                    JobState::Paused
                } else {
                    JobState::Queued
                };
                self.transition_or_log(id, next, None);
            }
            JobState::Scheduled if job.held_by != slot => {
                job.held_by = slot;
                self.save(id);
                self.emit_changed(id);
            }
            JobState::Queued if slot.is_some() => {
                job.held_by = slot;
                self.transition_or_log(id, JobState::Scheduled, None);
            }
            JobState::Retrying if stops => {
                job.held_by = slot;
                self.transition_or_log(id, JobState::Scheduled, None);
            }
            _ if stops => {
                if let Some(task) = &mut job.task {
                    task.stop = task.stop.max(Some(Stop::Hold));
                    task.cancel.cancel();
                }
            }
            _ => {}
        }
    }

    /// Whether the slot that held a job has ended and keeps what it held
    /// paused. Not if it was switched off or removed: that lets them go.
    fn keeps_paused(&self, held_by: Option<SlotId>) -> bool {
        held_by.is_some_and(|id| {
            self.ended.contains(&id)
                || self.slots.get(&id).is_some_and(|slot| {
                    slot.settings.enabled && !slot.active && slot.settings.keeps_paused()
                })
        })
    }

    /// The slot stopping this job's downloads, if one does and the job
    /// doesn't ignore slots, and whether it lets running ones finish.
    fn hold_of(&self, id: JobId) -> Option<(SlotId, bool)> {
        let job = self.jobs.get(&id)?;
        if job.ignores_slots {
            return None;
        }
        self.effects.stop(job.category)
    }

    /// Works out which slots apply at `now`, announcing each that starts or
    /// stops applying, removes those that happened once and are over, and
    /// puts what they change to use.
    fn update_slots(&mut self, now: &Zoned) {
        let over: Vec<SlotId> = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.settings.is_over(now))
            .map(|(id, _)| *id)
            .collect();
        for id in over {
            if self
                .slots
                .get(&id)
                .is_some_and(|slot| slot.settings.enabled && slot.settings.keeps_paused())
            {
                self.ended.push(id);
            }
            self.drop_slot(id);
        }
        let mut changed = Vec::new();
        for (&id, slot) in &mut self.slots {
            let active = slot.settings.enabled && slot.settings.covers(now);
            if slot.active != active {
                slot.active = active;
                changed.push(id);
            }
        }
        for id in changed {
            self.announce_slot(id);
        }
        self.apply_effects();
    }

    /// Recomputes what the active slots change, highest in the list first,
    /// and applies the limits if that differs.
    fn apply_effects(&mut self) {
        let mut active: Vec<(u64, SlotId, &SlotSettings)> = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.active)
            .map(|(&id, slot)| (slot.place, id, &slot.settings))
            .collect();
        active.sort_by_key(|&(place, id, _)| (place, id));
        let effects = Effects::of(active.into_iter().map(|(_, id, settings)| (id, settings)));
        if effects != self.effects {
            self.effects = effects;
            self.apply_limits();
        }
    }

    /// The total and category speed limits: the settings', unless a slot
    /// sets one now.
    fn apply_limits(&self) {
        let global = self.effects.queue.speed_limit;
        self.limiter
            .set_global(global.unwrap_or(self.settings.global_limit));
        for (&id, category) in &self.categories {
            let limit = self.effects.of_category(id).speed_limit;
            self.limiter
                .set_category_limit(id, limit.unwrap_or(category.speed_limit));
        }
    }

    /// How many downloads may run at once now: the setting, unless a slot
    /// says otherwise.
    fn max_concurrent(&self) -> usize {
        self.effects
            .queue
            .max_concurrent
            .unwrap_or(self.settings.max_concurrent)
    }

    fn has_room(&self, category: CategoryId, busy: &HashMap<CategoryId, usize>) -> bool {
        let cap = self
            .effects
            .of_category(category)
            .max_concurrent
            .or_else(|| self.categories.get(&category).map(|c| c.max_concurrent))
            .unwrap_or(0);
        cap == 0 || busy.get(&category).copied().unwrap_or(0) < cap
    }

    /// Every slot, in the order of the list.
    fn slots(&self) -> Vec<Slot> {
        let mut slots: Vec<(u64, Slot)> = self
            .slots
            .iter()
            .map(|(&id, slot)| (slot.place, self.slot(id, slot)))
            .collect();
        slots.sort_by_key(|(place, slot)| (*place, slot.id));
        slots.into_iter().map(|(_, slot)| slot).collect()
    }

    fn slot(&self, id: SlotId, entry: &SlotEntry) -> Slot {
        Slot {
            id,
            place: entry.place,
            settings: entry.settings.clone(),
            active: entry.active,
        }
    }

    /// Normalizes `settings` and checks that its category exists.
    fn check_slot(&self, settings: SlotSettings) -> Result<SlotSettings, Error> {
        let settings = settings.normalized()?;
        if let Some(category) = settings.category {
            self.category(category)?;
        }
        Ok(settings)
    }

    async fn add_slot(&mut self, settings: SlotSettings) -> Result<SlotId, Error> {
        let settings = self.check_slot(settings)?;
        let last = self.slots.keys().next_back().map_or(0, |id| id.0);
        let id = SlotId(last.checked_add(1).ok_or(Error::TooManySlots)?);
        let place = self
            .slots
            .values()
            .map(|slot| slot.place + 1)
            .max()
            .unwrap_or(0);
        // Known now, so it's announced once.
        let active = settings.enabled && settings.covers(&Zoned::now());
        let entry = SlotEntry {
            place,
            settings,
            active,
        };
        self.put_slot(id, entry).await?;
        self.check_times();
        self.schedule();
        Ok(id)
    }

    async fn update_slot(&mut self, id: SlotId, settings: SlotSettings) -> Result<(), Error> {
        let Some(place) = self.slots.get(&id).map(|slot| slot.place) else {
            return Err(Error::SlotNotFound(id));
        };
        let settings = self.check_slot(settings)?;
        let active = settings.enabled && settings.covers(&Zoned::now());
        let entry = SlotEntry {
            place,
            settings,
            active,
        };
        self.put_slot(id, entry).await?;
        self.check_times();
        self.schedule();
        Ok(())
    }

    /// Saves it, then keeps it and tells the subscribers.
    async fn put_slot(&mut self, id: SlotId, entry: SlotEntry) -> Result<(), Error> {
        let record = SlotRecord {
            id,
            place: entry.place,
            settings: entry.settings.clone(),
        };
        self.store.write(Write::SaveSlot(record)).await?;
        self.slots.insert(id, entry);
        self.announce_slot(id);
        Ok(())
    }

    async fn remove_slot(&mut self, id: SlotId) -> Result<(), Error> {
        if !self.slots.contains_key(&id) {
            return Err(Error::SlotNotFound(id));
        }
        self.store.write(Write::RemoveSlot(id)).await?;
        if self.slots.remove(&id).is_some() {
            // Fails only when nobody is subscribed.
            let _ = self.events.send(JobEvent::SlotRemoved(id));
        }
        self.check_times();
        self.schedule();
        Ok(())
    }

    /// Each slot that moves is saved, then moved: if one can't be saved,
    /// those before it have moved and the rest haven't.
    async fn reorder_slots(&mut self, ids: &[SlotId]) -> Result<(), Error> {
        let unique: HashSet<&SlotId> = ids.iter().collect();
        if ids.len() != self.slots.len()
            || unique.len() != ids.len()
            || ids.iter().any(|id| !self.slots.contains_key(id))
        {
            return Err(Error::InvalidSlotOrder);
        }
        let mut saved = Ok(());
        for (place, &id) in (0u64..).zip(ids) {
            let Some(slot) = self.slots.get(&id).filter(|slot| slot.place != place) else {
                continue;
            };
            let record = SlotRecord {
                id,
                place,
                settings: slot.settings.clone(),
            };
            if let Err(err) = self.store.write(Write::SaveSlot(record)).await {
                saved = Err(err);
                break;
            }
            if let Some(slot) = self.slots.get_mut(&id) {
                slot.place = place;
            }
            self.announce_slot(id);
        }
        self.check_times();
        self.schedule();
        saved
    }

    fn announce_slot(&self, id: SlotId) {
        if let Some(entry) = self.slots.get(&id) {
            // Fails only when nobody is subscribed.
            let _ = self
                .events
                .send(JobEvent::SlotChanged(self.slot(id, entry)));
        }
    }

    /// Takes the slot out of the list and the database. The caller checks
    /// the times again (what it changed no longer applies).
    fn drop_slot(&mut self, id: SlotId) {
        if self.slots.remove(&id).is_some() {
            self.store.queue(Write::RemoveSlot(id));
            // Fails only when nobody is subscribed.
            let _ = self.events.send(JobEvent::SlotRemoved(id));
        }
    }

    fn start_job(&mut self, id: JobId) -> bool {
        if !self.transition_or_log(id, JobState::Connecting, None) {
            return false;
        }
        self.spawn_task(id)
    }

    /// Starts the task that downloads the job, in the state it's in.
    fn spawn_task(&mut self, id: JobId) -> bool {
        let placement = self.jobs.get(&id).and_then(|job| self.placement(job));
        let trusted = self
            .jobs
            .get(&id)
            .map(|job| self.trusted_for(&job.download.url))
            .unwrap_or_default();
        let Some(job) = self.jobs.get_mut(&id) else {
            return false;
        };
        let cancel = CancellationToken::new();
        job.task = Some(Task {
            cancel: cancel.clone(),
            stop: None,
            waiters: Vec::new(),
        });
        // The task works on its own copy and hands it back when it ends.
        let mut download = job.download.clone();
        download.placement = placement;
        download.existing = job.existing.unwrap_or(self.settings.existing);
        download.trusted = trusted;
        let backend = Arc::clone(&self.backend);
        let internal = self.internal.clone();
        // The job's speed budget lives as long as the task does.
        let throttle = self
            .limiter
            .register(id, job.speed_limit, job.priority, job.category);
        let playback = self.playbacks.of(id);
        playback.set_streaming(job.streaming);

        // What it hands back if it crashes: what it did to the file is
        // unknown, so the next run starts over (no validator, no map: nothing
        // to resume from).
        let mut before = download.clone();
        before.validator = None;
        before.segments.clear();
        before.downloaded = 0;
        let report = internal.clone();
        supervised(
            async move {
                let mut last_progress: Option<Instant> = None;
                // Sends fail only if the manager is gone, and then nobody's listening.
                let mut on_event = |event: Event| {
                    if let Event::Progress { .. } = event {
                        if last_progress.is_some_and(|at| at.elapsed() < PROGRESS_INTERVAL) {
                            return;
                        }
                        last_progress = Some(Instant::now());
                    }
                    let _ = internal.send(Internal::Event { id, event });
                };
                let result = backend
                    .run(&mut download, &throttle, &playback, &cancel, &mut on_event)
                    .await;
                drop(throttle);
                (download, result)
            },
            move |ended| {
                let (download, result) = match ended {
                    Ok(ended) => ended,
                    Err(panic) => (before, Err(Error::Crashed(panic))),
                };
                let _ = report.send(Internal::Ended {
                    id,
                    download: Box::new(download),
                    result,
                });
            },
        );
        true
    }

    async fn on_ended(
        &mut self,
        id: JobId,
        download: Box<Download>,
        result: Result<Outcome, Error>,
    ) {
        let hold = self.hold_of(id);
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        let task = job.task.take();
        job.waiting_for_bandwidth = false;
        job.download = *download;
        job.download.placement = None;
        let stop = task.as_ref().and_then(|t| t.stop);
        // The file couldn't be written or synced as it stopped (disk full, a
        // drive gone): the pause asked for still happens, but says why, and
        // keeps only what's on disk.
        let disk_failure = match (&result, stop) {
            (Err(Error::Io(err)), Some(Stop::Pause | Stop::Shutdown)) => {
                Some((err.kind(), err.to_string()))
            }
            _ => None,
        };
        if disk_failure.is_some()
            && job.download.segments.is_empty()
            && let Some(part) = job.download.part_path()
        {
            // One connection: the file's length is the truth.
            job.download.downloaded = tokio::fs::metadata(&part).await.map_or(0, |m| m.len());
        }

        let next = match (result, stop) {
            // Finished before it noticed the stop request: keep the file.
            (Ok(Outcome::Completed { .. }), _) => Some((JobState::Completed, None)),
            // Its start time was taken away (or came), or the slot holding
            // it ended, while it stopped.
            (_, Some(Stop::Hold)) if !job.held(SystemTime::now()) && hold.is_none() => {
                Some((JobState::Queued, None))
            }
            (Err(err), Some(Stop::Pause | Stop::Shutdown)) if disk_failure.is_some() => {
                tracing::warn!(%id, error = %err, "the file failed as the download stopped");
                Some((JobState::Paused, Some(Problem::from(&err))))
            }
            (_, Some(stop)) => stop.target().map(|state| (state, None)),
            // Only the manager cancels the token, so this means paused.
            (Ok(Outcome::Paused), None) => Some((JobState::Paused, None)),
            (Err(err), None)
                if err.is_retryable() && job.retries < self.settings.retry.max_retries =>
            {
                job.retries += 1;
                let wait = retry_wait(&self.settings.retry, job.retries, err.retry_after());
                tracing::info!(%id, error = %err, retry = job.retries, ?wait, "download failed; retrying");
                job.retry_at = Some((Instant::now() + wait, SystemTime::now() + wait));
                Some((JobState::Retrying, Some(Problem::from(&err))))
            }
            (Err(err), None) => {
                tracing::warn!(%id, error = %err, "download failed");
                Some((JobState::Failed, Some(Problem::from(&err))))
            }
        };
        if let Some((JobState::Scheduled, _)) = next {
            job.held_by = hold.map(|(slot, _)| slot);
        }
        match next {
            Some((state, problem)) => {
                let moved = self.transition_or_log(id, state, problem);
                // A rule missing from `can_become`. Never leave the job
                // running without a task: paused, it can be resumed.
                debug_assert!(moved, "{id}: refused to become {state:?} as its task ended");
                if !moved && self.jobs.get(&id).is_some_and(|job| job.state.is_running()) {
                    self.transition_or_log(id, JobState::Paused, None);
                }
                match state {
                    JobState::Cancelled => self.delete_part(id).await,
                    JobState::Retrying => self.retry_when_due(id),
                    _ => {}
                }
            }
            // Shutting down, or stopped to move its file: keep the running state
            // (the next start continues it, or `finish_move` does).
            None => self.save(id),
        }

        let Some(final_state) = self.jobs.get(&id).map(|job| job.state) else {
            return;
        };
        // Forgotten before answering, so a caller that lists next won't see it.
        let forgotten = match stop {
            Some(Stop::Remove) => Some(self.forget(id).await.map_err(storage_text)),
            _ => None,
        };
        let removed = matches!(forgotten, Some(Ok(())));
        for (asked, reply) in task.map(|t| t.waiters).unwrap_or_default() {
            let answer = match asked.target() {
                _ if removed && asked == Stop::Remove => Ok(()),
                _ if removed => Err(Error::JobNotFound(id)),
                // It couldn't be taken out of the database: it stays, cancelled.
                _ if asked == Stop::Remove && matches!(forgotten, Some(Err(_))) => {
                    Err(Error::StorageFailed(
                        forgotten.clone().and_then(Result::err).unwrap_or_default(),
                    ))
                }
                // Paused, but its file isn't as it should be.
                Some(JobState::Paused) if disk_failure.is_some() => {
                    let (kind, text) = disk_failure
                        .clone()
                        .unwrap_or((io::ErrorKind::Other, String::new()));
                    Err(Error::Io(io::Error::new(kind, text)))
                }
                Some(wanted) if wanted != final_state => Err(Error::InvalidTransition {
                    from: final_state,
                    to: wanted,
                }),
                _ => Ok(()),
            };
            let _ = reply.send(answer);
        }
        // Stopped to move its file (or finished first): move it now, unless
        // Ottie is shutting down.
        if self.shutdown.is_some() {
            if let Some(moving) = self.jobs.get_mut(&id).and_then(|job| job.moving.take()) {
                let _ = moving.reply.send(Err(Error::ManagerStopped));
            }
        } else {
            self.move_file(id);
        }
        self.schedule();
    }

    fn begin_shutdown(&mut self, reply: Option<Reply<()>>) {
        self.shutdown.get_or_insert_with(Vec::new).extend(reply);
        for job in self.jobs.values_mut() {
            if let Some(task) = &mut job.task {
                task.stop = task.stop.max(Some(Stop::Shutdown));
                task.cancel.cancel();
            }
        }
    }

    /// Includes files being moved, which shutdown waits for too.
    fn any_task_running(&self) -> bool {
        self.jobs
            .values()
            .any(|job| job.task.is_some() || job.moving.is_some())
    }

    /// The only place a job changes state. Refuses moves the state machine
    /// doesn't allow; saves and tells subscribers about the others.
    fn transition(
        &mut self,
        id: JobId,
        next: JobState,
        problem: Option<Problem>,
    ) -> Result<(), Error> {
        let job = self.jobs.get_mut(&id).ok_or(Error::JobNotFound(id))?;
        if !job.state.can_become(next) {
            return Err(Error::InvalidTransition {
                from: job.state,
                to: next,
            });
        }
        job.state = next;
        job.problem = problem;
        if next != JobState::Retrying {
            job.retry_at = None;
        }
        // A start time is over once the job is in the queue or stopped (a
        // running or retrying one keeps it while it stops to wait for it).
        if matches!(
            next,
            JobState::Queued
                | JobState::Paused
                | JobState::Failed
                | JobState::Completed
                | JobState::Cancelled
        ) {
            job.start_at = None;
        }
        if next.is_finished() {
            job.stop_at = None;
        }
        if next != JobState::Scheduled {
            job.held_by = None;
        }
        // Started by hand during a slot: only until it stops.
        if matches!(
            next,
            JobState::Paused | JobState::Failed | JobState::Completed | JobState::Cancelled
        ) {
            job.ignores_slots = false;
        }
        self.save(id);
        self.emit_changed(id);
        Ok(())
    }

    /// For transitions the manager itself makes, which should always be valid.
    fn transition_or_log(&mut self, id: JobId, next: JobState, problem: Option<Problem>) -> bool {
        match self.transition(id, next, problem) {
            Ok(()) => true,
            Err(err) => {
                tracing::error!(%id, error = %err, "manager made an invalid transition");
                false
            }
        }
    }

    fn save(&mut self, id: JobId) {
        if let Some(record) = self.record(id) {
            self.store.save(record);
        }
    }

    /// What to save of the job, now.
    fn record(&mut self, id: JobId) -> Option<JobRecord> {
        let job = self.jobs.get_mut(&id)?;
        job.last_saved = Some(Instant::now());
        Some(JobRecord {
            id,
            state: job.state,
            download: job.download.clone(),
            problem: job.problem.clone(),
            priority: job.priority,
            place: job.place,
            speed_limit: job.speed_limit,
            category: job.category,
            category_chosen: job.category_chosen,
            streaming: job.streaming,
            streaming_chosen: job.streaming_chosen,
            start_at: job.start_at,
            stop_at: job.stop_at,
            ignores_slots: job.ignores_slots,
            held_by: job.held_by,
            existing: job.existing,
        })
    }

    /// Takes a stopped job off the list and out of the database.
    /// Out of the database first: if that fails, it stays on the list, as
    /// it would be at the next start (docs/decisions/0059).
    async fn forget(&mut self, id: JobId) -> Result<(), Error> {
        if !self.jobs.contains_key(&id) {
            return Ok(());
        }
        // The generator has handed out at least this job's ID.
        let last_id = self.ids.last().unwrap_or(id);
        self.store.write(Write::RemoveJob { id, last_id }).await?;
        self.jobs.remove(&id);
        // Fails only when nobody is subscribed.
        let _ = self.events.send(JobEvent::Removed(id));
        Ok(())
    }

    fn emit_changed(&self, id: JobId) {
        if let Some(job) = self.jobs.get(&id) {
            let snapshot = job.snapshot(id);
            // Fails only when nobody is subscribed.
            let _ = self.events.send(JobEvent::Changed(snapshot));
        }
    }

    /// Wakes the actor when the job's retry is due.
    fn retry_when_due(&self, id: JobId) {
        let Some((due, _)) = self.jobs.get(&id).and_then(|job| job.retry_at) else {
            return;
        };
        let internal = self.internal.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(due.into()).await;
            // Fails only if the manager has stopped.
            let _ = internal.send(Internal::RetryDue { id, due });
        });
    }

    async fn delete_part(&self, id: JobId) {
        let Some(part) = self.jobs.get(&id).and_then(|job| job.download.part_path()) else {
            return;
        };
        if let Err(err) = tokio::fs::remove_file(&part).await
            && err.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%id, error = %err, "could not delete .part.ottie file");
        }
    }
}

/// How long to wait before retry number `retry` (1 for the first): the
/// policy's first wait, doubled each time up to a minute, or longer if the
/// server asked for that with `Retry-After`.
fn retry_wait(policy: &RetryPolicy, retry: u32, asked: Option<Duration>) -> Duration {
    let doublings = retry.saturating_sub(1).min(16);
    let cap = MAX_RETRY_WAIT.max(policy.first_wait);
    let backoff = policy.first_wait.saturating_mul(1 << doublings).min(cap);
    let wait = asked.map_or(backoff, |asked| asked.min(MAX_RETRY_AFTER).max(backoff));
    wait.min(LONGEST_RETRY_WAIT)
}

/// The category that lists `ext`, if any. Takes the map rather than `&self`
/// so it can be used while a job is borrowed.
fn owner_of(categories: &BTreeMap<CategoryId, CategorySettings>, ext: &str) -> Option<CategoryId> {
    categories
        .iter()
        .find(|(_, settings)| settings.extensions.iter().any(|e| e == ext))
        .map(|(id, _)| *id)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A failed write's text, for answering several with it: the database's
/// own, not wrapped again.
fn storage_text(err: Error) -> String {
    match err {
        Error::StorageFailed(text) => text,
        other => other.to_string(),
    }
}

/// Runs `work` in a task of its own, then gives `ended` what it returned,
/// or the text of its panic (a bug). The actor waits for the end of every
/// download and move (pause, cancel, shutdown), so it must hear of it even
/// when the task crashes; a dropped `JoinHandle` would tell nobody.
fn supervised<T: Send + 'static>(
    work: impl Future<Output = T> + Send + 'static,
    ended: impl FnOnce(Result<T, String>) + Send + 'static,
) {
    let task = tokio::spawn(work);
    tokio::spawn(async move {
        let result = task.await.map_err(|err| {
            let text = match err.try_into_panic() {
                Ok(panic) => panic
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "a panic".to_owned()),
                // The runtime is shutting down: nobody's waiting.
                Err(_) => "cancelled".to_owned(),
            };
            tracing::error!(%text, "a task crashed");
            text
        });
        ended(result);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_waits_double_up_to_a_minute() {
        let policy = RetryPolicy::default(); // first wait 2 s
        let secs = |retry, asked| retry_wait(&policy, retry, asked).as_secs();
        assert_eq!(
            [1, 2, 3, 4, 5, 6, 7].map(|r| secs(r, None)),
            [2, 4, 8, 16, 32, 60, 60]
        );
        assert_eq!(secs(40, None), 60, "no overflow");
        // Retry-After can ask for longer, never shorter, and not forever.
        assert_eq!(secs(1, Some(Duration::from_secs(30))), 30);
        assert_eq!(secs(5, Some(Duration::from_secs(1))), 32);
        assert_eq!(secs(1, Some(Duration::from_secs(86_400))), 600);
    }

    #[test]
    fn a_long_first_wait_is_not_capped() {
        let policy = RetryPolicy {
            first_wait: Duration::from_secs(90),
            ..RetryPolicy::default()
        };
        assert_eq!(retry_wait(&policy, 3, None), Duration::from_secs(90));
        let absurd = RetryPolicy {
            first_wait: Duration::MAX,
            ..RetryPolicy::default()
        };
        assert_eq!(retry_wait(&absurd, 1, None), LONGEST_RETRY_WAIT);
    }
}
