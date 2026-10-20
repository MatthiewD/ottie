use std::path::PathBuf;
use std::time::SystemTime;

use reqwest::Url;

use crate::category::{Category, CategoryId};
use crate::download::{RequestHeaders, RestartReason, Segment};
use crate::filename::Existing;
use crate::id::JobId;
use crate::problem::Problem;
use crate::settings::Settings;
use crate::slot::{Slot, SlotId};
use crate::view::{View, ViewId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Waiting for a free slot (see the manager's max concurrent downloads).
    Queued,
    /// Running, no bytes received yet.
    Connecting,
    Downloading,
    Paused,
    Completed,
    /// Stopped by an error; can be resumed.
    Failed,
    Cancelled,
    /// Stopped by an error that may go away; waiting to try again. Holds its
    /// download slot until it gives way to a waiting job (docs/decisions/0013).
    Retrying,
    /// Waiting for its start time, then it joins the queue at its place
    /// (docs/decisions/0040).
    Scheduled,
}

impl JobState {
    /// The state machine. Every change of state is checked against this.
    pub fn can_become(self, next: JobState) -> bool {
        use JobState::*;
        matches!(
            (self, next),
            (Queued, Connecting | Paused | Cancelled | Scheduled)
                | (Connecting, Downloading)
                // Stopped for a hold (a start time, a slot) that went away
                // while it stopped: back in the queue.
                | (Connecting | Downloading, Queued)
                | (
                    Connecting | Downloading,
                    Completed | Paused | Failed | Cancelled | Retrying | Scheduled
                )
                | (
                    Retrying,
                    Connecting | Queued | Paused | Cancelled | Scheduled
                )
                | (Paused | Failed, Queued | Cancelled | Scheduled)
                | (Scheduled, Queued | Paused | Cancelled)
        )
    }

    pub fn is_running(self) -> bool {
        matches!(self, JobState::Connecting | JobState::Downloading)
    }

    /// Queued, scheduled or paused: in the queue, or back in it at its place
    /// once resumed or due. These are the jobs [`crate::Manager::reorder`] moves.
    pub fn is_waiting(self) -> bool {
        matches!(
            self,
            JobState::Queued | JobState::Scheduled | JobState::Paused
        )
    }

    /// Completed or cancelled: nothing more will happen to it.
    pub fn is_finished(self) -> bool {
        matches!(self, JobState::Completed | JobState::Cancelled)
    }
}

/// Decides which queued job starts next and how the global speed limit is
/// shared. See docs/decisions/0010.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Priority {
    /// Half a Normal download's share of the global limit.
    Low,
    #[default]
    Normal,
    /// Twice a Normal download's share.
    High,
    /// Served before everything else: others only get what Urgent jobs can't use.
    Urgent,
}

/// Where [`crate::Manager::reorder`] moves jobs: among the waiting jobs of
/// their priority (docs/decisions/0032).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reorder {
    /// One place earlier.
    Up,
    /// One place later.
    Down,
    /// Before all the others.
    Top,
    /// After all the others.
    Bottom,
    /// Just before this job (dragged there). Only moves jobs of its priority.
    Before(JobId),
    /// Just after this job.
    After(JobId),
}

impl Reorder {
    /// Moves the chosen jobs of `order` (one priority's waiting jobs, in
    /// queue order), keeping their order among themselves: a chosen job
    /// never passes another, so a block moves as one. With `Before` or
    /// `After` a job that isn't in `order`, nothing moves.
    pub(crate) fn apply(self, order: &mut Vec<JobId>, chosen: impl Fn(&JobId) -> bool) {
        match self {
            Reorder::Up => step_up(order, chosen),
            Reorder::Down => step_down(order, chosen),
            Reorder::Top => gather(order, chosen, 0),
            Reorder::Bottom => {
                let end = order.len();
                gather(order, chosen, end);
            }
            Reorder::Before(target) | Reorder::After(target) => {
                if let Some(i) = order.iter().position(|&id| id == target) {
                    let at = if matches!(self, Reorder::After(_)) {
                        i + 1
                    } else {
                        i
                    };
                    gather(order, chosen, at);
                }
            }
        }
    }

    /// The job to go next to, when there is one.
    pub(crate) fn target(self) -> Option<JobId> {
        match self {
            Reorder::Before(target) | Reorder::After(target) => Some(target),
            _ => None,
        }
    }
}

/// Moves each chosen item one step earlier, past an item that isn't chosen.
fn step_up<T>(order: &mut [T], chosen: impl Fn(&T) -> bool) {
    for i in 1..order.len() {
        if chosen(&order[i]) && !chosen(&order[i - 1]) {
            order.swap(i - 1, i);
        }
    }
}

/// Moves each chosen item one step later, past an item that isn't chosen.
fn step_down<T>(order: &mut [T], chosen: impl Fn(&T) -> bool) {
    for i in (1..order.len()).rev() {
        if chosen(&order[i - 1]) && !chosen(&order[i]) {
            order.swap(i - 1, i);
        }
    }
}

/// Puts the chosen items together where the gap before `order[at]` was (at
/// the end if `at` is the length); the others keep their order around them.
fn gather<T>(order: &mut Vec<T>, chosen: impl Fn(&T) -> bool, at: usize) {
    let (mut before, mut moving, mut after) = (Vec::new(), Vec::new(), Vec::new());
    for (i, item) in order.drain(..).enumerate() {
        if chosen(&item) {
            moving.push(item);
        } else if i < at {
            before.push(item);
        } else {
            after.push(item);
        }
    }
    order.extend(before);
    order.extend(moving);
    order.extend(after);
}

/// What [`crate::Manager::add`] needs to know about a new download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    pub url: Url,
    /// Where to save it. `None`: the category's folder.
    pub dest_dir: Option<PathBuf>,
    /// `None`: the category whose extensions include the file extension in
    /// the URL, or General if none does.
    pub category: Option<CategoryId>,
    /// Streaming mode (docs/decisions/0029). `None`: on for videos and music
    /// if [`Settings::stream_media`] says so, by the URL's extension and again
    /// by the real name once the server answers.
    pub streaming: Option<bool>,
    /// Waits until then (`Scheduled`), then joins the queue. `None`, or a
    /// time already past: queued at once.
    pub start_at: Option<SystemTime>,
    /// Pauses then if it hasn't finished. Must be after `start_at`.
    pub stop_at: Option<SystemTime>,
    /// What to do when a file with its name is already in its folder.
    /// `None`: [`Settings::existing`], when the time comes.
    pub existing: Option<Existing>,
    /// What the browser sent with it (docs/decisions/0055).
    pub headers: RequestHeaders,
}

impl NewJob {
    pub fn new(url: Url) -> Self {
        Self {
            url,
            dest_dir: None,
            category: None,
            streaming: None,
            start_at: None,
            stop_at: None,
            existing: None,
            headers: RequestHeaders::default(),
        }
    }

    /// Without [`NewJob::in_category`], the folder also decides the category:
    /// the one whose folder it is, else the built-in Other.
    pub fn in_dir(mut self, dest_dir: impl Into<PathBuf>) -> Self {
        self.dest_dir = Some(dest_dir.into());
        self
    }

    pub fn in_category(mut self, category: CategoryId) -> Self {
        self.category = Some(category);
        self
    }

    /// What to do if a file with its name is already in its folder.
    pub fn existing(mut self, existing: Existing) -> Self {
        self.existing = Some(existing);
        self
    }

    /// Streaming mode on or off, whatever the file type.
    pub fn streaming(mut self, on: bool) -> Self {
        self.streaming = Some(on);
        self
    }

    pub fn starting_at(mut self, at: SystemTime) -> Self {
        self.start_at = Some(at);
        self
    }

    pub fn stopping_at(mut self, at: SystemTime) -> Self {
        self.stop_at = Some(at);
        self
    }

    /// Sent with every request of this download; checked when it's added.
    pub fn with_headers(mut self, headers: RequestHeaders) -> Self {
        self.headers = headers;
        self
    }
}

/// Some of a job's own settings to change at once; `None` leaves one as it
/// is. See [`crate::Manager::change_job`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobChange {
    /// Bytes per second; 0 = no limit of its own.
    pub speed_limit: Option<u64>,
    pub priority: Option<Priority>,
    /// Streaming mode: in order, the end of the file first.
    pub streaming: Option<bool>,
    /// `Some(Some(at))`: wait until then (a running job stops and waits; a
    /// paused or failed one waits too). `Some(None)`: no start time (a
    /// scheduled job is queued at once). See [`crate::Manager::change_job`].
    pub start_at: Option<Option<SystemTime>>,
    /// `Some(Some(at))`: pause then; `Some(None)`: no stop time.
    pub stop_at: Option<Option<SystemTime>>,
    /// What to do when a file with its name is already in its folder (at
    /// its first response, or a resume after it stopped to ask).
    pub existing: Option<Existing>,
}

/// A copy of a job's public state, for listing and events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSnapshot {
    pub id: JobId,
    pub url: Url,
    pub dest_dir: PathBuf,
    pub file_name: Option<String>,
    pub state: JobState,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub resumable: bool,
    /// Why the job failed (`Failed`), or the last failure (`Retrying`, or
    /// `Queued` after giving its slot away). `Display` gives English text.
    pub problem: Option<Problem>,
    pub priority: Priority,
    /// Its place in the queue: among the jobs of its priority, lower places
    /// start first. New jobs go last; see [`crate::Manager::reorder`].
    pub place: u64,
    /// This job's own limit in bytes per second; 0 = none.
    pub speed_limit: u64,
    /// Streaming mode (docs/decisions/0029): over several connections, the
    /// file arrives in order, its last 1 MiB first, so it plays sooner.
    pub streaming: bool,
    pub category: CategoryId,
    /// Failed attempts since bytes last arrived. With `Retrying`, the number
    /// of the retry it's waiting for.
    pub retries: u32,
    /// When it will try again, while `Retrying`.
    pub retry_at: Option<SystemTime>,
    /// Multi-connection: the ranges as last saved (flushed to disk). Empty
    /// for one connection, or before the first response.
    pub segments: Vec<Segment>,
    /// Its file is moving to another category's folder. Until it's done,
    /// the job can't be paused, resumed, cancelled, removed or moved again.
    pub moving: bool,
    /// Running, but the speed limits give it no bandwidth for now: other
    /// downloads take it all (Urgent ones first). See docs/decisions/0033.
    pub waiting_for_bandwidth: bool,
    /// While `Scheduled`: when it joins the queue.
    pub start_at: Option<SystemTime>,
    /// When it pauses if it hasn't finished by then.
    pub stop_at: Option<SystemTime>,
    /// While `Scheduled`: the time slot that keeps it from running, if one does.
    pub held_by: Option<SlotId>,
}

/// Broadcast to every subscriber of the manager.
// `Changed` is the biggest and by far the most frequent: boxing it would
// allocate for each one.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    /// A job was added, changed state, or learned its file name and size.
    /// Clients can insert-or-replace by `id`.
    Changed(JobSnapshot),
    /// Bytes arrived. Sent at most a few times per second per job.
    Progress {
        id: JobId,
        downloaded: u64,
        total: Option<u64>,
        /// Multi-connection: each range with what has arrived so far.
        /// Empty for one connection.
        segments: Vec<Segment>,
    },
    /// The job was removed. Clients drop it.
    Removed(JobId),
    /// The job threw away its partial file and started over.
    Restarted { id: JobId, reason: RestartReason },
    /// A category was added or changed. Clients can insert-or-replace by `id`.
    CategoryChanged(Category),
    /// A category was removed; its jobs were moved to General first.
    CategoryRemoved(CategoryId),
    /// The settings changed; here they are, all of them.
    SettingsChanged(Settings),
    /// A saved view was added or changed. Clients can insert-or-replace by `id`.
    ViewChanged(View),
    /// A saved view was removed.
    ViewRemoved(ViewId),
    /// A time slot was added or changed, or started or stopped applying.
    /// Clients can insert-or-replace by `id`.
    SlotChanged(Slot),
    /// A time slot was removed (by hand, with its category, or once over).
    SlotRemoved(SlotId),
}

#[cfg(test)]
mod tests {
    use super::JobState::*;
    use super::*;

    const ALL: [JobState; 9] = [
        Queued,
        Connecting,
        Downloading,
        Paused,
        Completed,
        Failed,
        Cancelled,
        Retrying,
        Scheduled,
    ];

    #[test]
    fn finished_states_are_final() {
        for next in ALL {
            assert!(!Completed.can_become(next));
            assert!(!Cancelled.can_become(next));
        }
    }

    #[test]
    fn no_state_becomes_itself() {
        for state in ALL {
            assert!(!state.can_become(state), "{state:?}");
        }
    }

    #[test]
    fn retrying_starts_again_or_gives_way() {
        assert!(Downloading.can_become(Retrying));
        assert!(Retrying.can_become(Connecting));
        assert!(Retrying.can_become(Queued));
        assert!(Retrying.can_become(Paused));
        assert!(!Retrying.can_become(Downloading));
        assert!(!Queued.can_become(Retrying));
        assert!(Downloading.can_become(Queued));
        assert!(Connecting.can_become(Queued));
    }

    /// Moves the upper-case letters of `order`.
    fn moved(order: &str, step: impl Fn(&mut Vec<char>, &dyn Fn(&char) -> bool)) -> String {
        let mut order: Vec<char> = order.chars().collect();
        step(&mut order, &|c| c.is_uppercase());
        order.into_iter().collect()
    }

    fn up(order: &str) -> String {
        moved(order, |order, chosen| step_up(order, chosen))
    }

    fn down(order: &str) -> String {
        moved(order, |order, chosen| step_down(order, chosen))
    }

    fn gathered(order: &str, at: usize) -> String {
        moved(order, |order, chosen| gather(order, chosen, at))
    }

    #[test]
    fn reorder_moves_one_step_as_a_block() {
        assert_eq!(up("abCd"), "aCbd");
        assert_eq!(up("aBCd"), "BCad");
        assert_eq!(up("AbCd"), "ACbd");
        assert_eq!(down("abCd"), "abdC");
        assert_eq!(down("aBCd"), "adBC");
        assert_eq!(down("aBcD"), "acBD");
    }

    #[test]
    fn reorder_stops_at_the_ends() {
        assert_eq!(up("ABc"), "ABc");
        assert_eq!(down("aBC"), "aBC");
        assert_eq!(up(""), "");
    }

    #[test]
    fn gathering_puts_the_chosen_ones_in_the_gap() {
        assert_eq!(gathered("aBcDe", 0), "BDace");
        assert_eq!(gathered("aBcDe", 5), "aceBD");
        // Before `e`, after `a`, and next to a chosen one: its gap.
        assert_eq!(gathered("aBcDe", 4), "acBDe");
        assert_eq!(gathered("aBcDe", 1), "aBDce");
        assert_eq!(gathered("aBcDe", 3), "acBDe");
    }

    #[test]
    fn dropping_next_to_a_job_of_another_lane_moves_nothing() {
        let id = |seq| JobId::from_parts(1, seq);
        let mut order = vec![id(1), id(2), id(3)];
        Reorder::Before(id(9)).apply(&mut order, |&job| job == id(3));
        assert_eq!(order, [id(1), id(2), id(3)]);
        Reorder::After(id(1)).apply(&mut order, |&job| job == id(3));
        assert_eq!(order, [id(1), id(3), id(2)]);
    }

    #[test]
    fn scheduled_waits_outside_the_queue() {
        for state in [Queued, Connecting, Downloading, Retrying, Paused, Failed] {
            assert!(state.can_become(Scheduled), "{state:?}");
        }
        assert!(Scheduled.can_become(Queued));
        assert!(Scheduled.can_become(Paused));
        assert!(Scheduled.can_become(Cancelled));
        // It joins the queue first, never starts on its own.
        assert!(!Scheduled.can_become(Connecting));
        assert!(!Completed.can_become(Scheduled));
    }

    #[test]
    fn resuming_goes_back_through_the_queue() {
        assert!(Paused.can_become(Queued));
        assert!(Failed.can_become(Queued));
        assert!(!Paused.can_become(Downloading));
        assert!(!Paused.can_become(Connecting));
    }
}
