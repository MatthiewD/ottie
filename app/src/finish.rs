//! When everything has finished (docs/decisions/0040): quit Ottie, or put the
//! computer to sleep, hibernate it or shut it down. Chosen for every time in
//! Settings, or for once from the tray menu or the Downloads page; both are
//! this computer's (`app.json`). The app sees downloads finish as it follows
//! the daemon for the tray (`status.rs`). Before the computer goes, a
//! countdown with Cancel: a small window on the daemon's `/finish` page, or a
//! notification (then Cancel is in the tray menu and the Downloads page).

use std::ops::RangeInclusive;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, State, Url, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};

use crate::notify;
use crate::power::{self, Able, Power};
use crate::prefs::{self, Prefs};
use crate::texts::Texts;

/// The countdown's window.
pub const LABEL: &str = "finish";
/// What the pages hear when anything here changes, with a `Finishing`.
const EVENT: &str = "finish";
/// The countdown's length people may choose, in seconds.
const COUNTDOWN: RangeInclusive<u32> = 10..=600;
pub const DEFAULT_COUNTDOWN: u32 = 60;
/// Quitting waits for the last notification's sound, which the app plays.
const QUIT_AFTER: Duration = Duration::from_secs(3);
/// How long what the computer can do is taken as known (the tray asks every
/// second).
const ABLE_FOR: Duration = Duration::from_secs(30);
/// Set (to anything), the countdown ends without the computer going: only
/// a line on stderr. For trying it out.
const DRY_RUN: &str = "OTTIE_POWER_DRY_RUN";
/// The window's width in CSS pixels; the page says how tall it is.
const WIDTH: f64 = 400.0;
const FIRST_HEIGHT: f64 = 200.0;
const HEIGHTS: (f64, f64) = (120.0, 480.0);

/// What happens when everything has finished.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Finish {
    #[default]
    Nothing,
    Quit,
    Sleep,
    Hibernate,
    ShutDown,
}

impl Finish {
    /// In the menus' order.
    pub const ALL: [Finish; 5] = [
        Finish::Nothing,
        Finish::Quit,
        Finish::Sleep,
        Finish::Hibernate,
        Finish::ShutDown,
    ];

    pub fn power(self) -> Option<Power> {
        match self {
            Finish::Nothing | Finish::Quit => None,
            Finish::Sleep => Some(Power::Sleep),
            Finish::Hibernate => Some(Power::Hibernate),
            Finish::ShutDown => Some(Power::ShutDown),
        }
    }

    fn of(power: Power) -> Finish {
        match power {
            Power::Sleep => Finish::Sleep,
            Power::Hibernate => Finish::Hibernate,
            Power::ShutDown => Finish::ShutDown,
        }
    }
}

/// Where the countdown shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CountdownIn {
    #[default]
    Window,
    Notification,
}

/// What's left to run, as the tray counts the downloads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Left {
    /// Queued, connecting, downloading or retrying.
    pub running: usize,
    /// Waiting for their start time, or held by a time slot.
    pub scheduled: usize,
}

/// What the downloads' last change means here.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    /// The last of them has just finished or failed.
    Finished,
    /// Some are left.
    Busy,
    /// None left, but none just ended: paused, cancelled or removed, which
    /// isn't finishing.
    Idle,
}

/// `ended`: a download has just finished or failed (after its last retry).
fn seen(left: Left, ended: bool, waits_for_scheduled: bool) -> Seen {
    if left.running > 0 || (waits_for_scheduled && left.scheduled > 0) {
        Seen::Busy
    } else if ended {
        Seen::Finished
    } else {
        Seen::Idle
    }
}

/// The countdown and the window, as managed state.
pub struct Finisher {
    /// The `/finish` page, signed in.
    page: Url,
    state: Mutex<Counting>,
}

#[derive(Default)]
struct Counting {
    now: Option<Countdown>,
    /// Quitting, once the last notification's sound has played: the ID of
    /// that wait, taken back if a download starts meanwhile.
    quitting: Option<u64>,
    /// Countdowns and quits started so far: a timer acts only if its own is
    /// still on.
    started: u64,
    able: Option<(Instant, Able)>,
    /// The window waits to be shown until the page says how tall it is.
    waiting: bool,
}

impl Counting {
    /// Quitting is chosen: the ID of its wait.
    fn quit_later(&mut self) -> u64 {
        self.started += 1;
        self.quitting = Some(self.started);
        self.started
    }

    /// The wait `id` is over: whether to quit still.
    fn quits(&mut self, id: u64) -> bool {
        self.quitting.take_if(|quitting| *quitting == id).is_some()
    }

    /// A download started again: no quitting, no countdown. Whether one
    /// was counting down.
    fn busy(&mut self) -> bool {
        self.quitting = None;
        self.now.take().is_some()
    }
}

#[derive(Debug, Clone, Copy)]
struct Countdown {
    id: u64,
    power: Power,
    ends_at: SystemTime,
}

impl Finisher {
    /// From the main window's signed-in link (`http://…/#token=…`).
    pub fn new(link: &Url) -> Finisher {
        let mut page = link.clone();
        page.set_path("/finish");
        Finisher {
            page,
            state: Mutex::new(Counting::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Counting> {
        // Nothing can panic while it's held, so a poisoned lock is still consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What this computer can do, asked again after a while.
    pub fn able(&self) -> Able {
        let mut counting = self.lock();
        if let Some((at, able)) = counting.able
            && at.elapsed() < ABLE_FOR
        {
            return able;
        }
        let able = power::able();
        counting.able = Some((Instant::now(), able));
        able
    }

    /// The countdown that runs: what, and the seconds left.
    pub fn counting(&self) -> Option<(Power, u64)> {
        self.lock().now.map(|countdown| {
            let left = countdown
                .ends_at
                .duration_since(SystemTime::now())
                .unwrap_or_default();
            // Rounded up: "1 sec" until the end.
            (countdown.power, left.as_millis().div_ceil(1000) as u64)
        })
    }
}

/// What the pages show and change: Settings, the Downloads page's button,
/// the tray menu and the countdown's window.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finishing {
    /// Every time (Settings).
    always: Finish,
    /// This time only, instead.
    once: Option<Finish>,
    able: Able,
    /// In seconds.
    countdown: u32,
    countdown_in: CountdownIn,
    /// Scheduled downloads are waited for (else: finished once nothing is
    /// left to run now).
    waits_for_scheduled: bool,
    /// The countdown, while one runs.
    running: Option<Running>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct Running {
    action: Finish,
    /// When the computer goes, in milliseconds since 1970 (the page's clock
    /// is this computer's).
    ends_at: u64,
}

fn finishing(app: &AppHandle) -> Finishing {
    let prefs = app.state::<prefs::Saved>().get();
    let finisher = app.state::<Finisher>();
    let running = finisher.lock().now.map(|countdown| Running {
        action: Finish::of(countdown.power),
        ends_at: countdown
            .ends_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    });
    Finishing {
        always: prefs.finish,
        once: prefs.finish_once,
        able: finisher.able(),
        countdown: prefs.countdown,
        countdown_in: prefs.countdown_in,
        waits_for_scheduled: prefs.finish_waits_for_scheduled,
        running,
    }
}

/// Every page hears what's new.
fn changed(app: &AppHandle) {
    // Best effort: a page also asks when it loads.
    let _ = app.emit(EVENT, finishing(app));
}

/// The downloads changed (`status.rs`): `ended` if one has just finished or
/// failed.
pub fn follow(app: &AppHandle, left: Left, ended: bool) {
    let prefs = app.state::<prefs::Saved>().get();
    match seen(left, ended, prefs.finish_waits_for_scheduled) {
        Seen::Finished => finished(app, &prefs),
        // A download started again (a slot, a start time, someone): not
        // finished after all. A choice for once is kept for then.
        Seen::Busy => {
            if app.state::<Finisher>().lock().busy() {
                close_window(app);
                changed(app);
            }
        }
        Seen::Idle => {}
    }
}

fn finished(app: &AppHandle, prefs: &Prefs) {
    let action = prefs.finish_once.unwrap_or(prefs.finish);
    match action.power() {
        Some(power) => start(app, power, prefs),
        None => {
            used_once(app);
            if action == Finish::Quit {
                let id = app.state::<Finisher>().lock().quit_later();
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(QUIT_AFTER).await;
                    // Unless a download started meanwhile (`Seen::Busy`).
                    let quits = app.state::<Finisher>().lock().quits(id);
                    if quits {
                        app.exit(0);
                    }
                });
            }
        }
    }
}

fn start(app: &AppHandle, power: Power, prefs: &Prefs) {
    let finisher = app.state::<Finisher>();
    let texts = Texts::for_language(prefs.language.as_deref());
    if !finisher.able().to(power) {
        used_once(app);
        notify::warn(app, texts.cant(power), None);
        return;
    }
    let seconds = prefs.countdown.clamp(*COUNTDOWN.start(), *COUNTDOWN.end());
    let id = {
        let mut counting = finisher.lock();
        if counting.now.is_some() {
            return;
        }
        counting.started += 1;
        counting.now = Some(Countdown {
            id: counting.started,
            power,
            ends_at: SystemTime::now() + Duration::from_secs(seconds.into()),
        });
        counting.started
    };
    match prefs.countdown_in {
        CountdownIn::Window => open_window(app),
        CountdownIn::Notification => notify::warn(
            app,
            texts.soon(power, seconds.into()),
            Some(texts.cancel_how.clone()),
        ),
    }
    changed(app);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(seconds.into())).await;
        act(&app, Some(id));
    });
}

/// The countdown has ended (`id`: the one that ended), or "now" was chosen
/// (`None`).
fn act(app: &AppHandle, id: Option<u64>) {
    let countdown = {
        let finisher = app.state::<Finisher>();
        let mut counting = finisher.lock();
        match counting.now {
            Some(countdown) if id.is_none_or(|id| id == countdown.id) => counting.now.take(),
            _ => None,
        }
    };
    let Some(Countdown { power, .. }) = countdown else {
        return;
    };
    used_once(app);
    close_window(app);
    changed(app);
    if std::env::var_os(DRY_RUN).is_some() {
        eprintln!("{DRY_RUN}: the computer would {power:?} now");
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Sleep and hibernation return once the computer wakes up.
        let done = tauri::async_runtime::spawn_blocking(move || power::run(power)).await;
        match done {
            Ok(Ok(())) if power == Power::ShutDown => app.exit(0),
            Ok(Ok(())) => {}
            _ => {
                let prefs = app.state::<prefs::Saved>().get();
                let texts = Texts::for_language(prefs.language.as_deref());
                notify::warn(&app, texts.cant(power), None);
            }
        }
    });
}

/// Cancel, in the countdown's window, the tray menu or the Downloads page.
pub fn cancel(app: &AppHandle) {
    if app.state::<Finisher>().lock().now.take().is_some() {
        used_once(app);
        close_window(app);
        changed(app);
    }
}

/// A choice for once is used up when its time comes, done or cancelled.
fn used_once(app: &AppHandle) {
    let saved = app.state::<prefs::Saved>();
    if saved.get().finish_once.is_some() {
        // Best effort: if it can't be saved, it's still used up here.
        let _ = saved.change(app, |prefs| prefs.finish_once = None);
        changed(app);
    }
}

/// What happens this time only, from the tray menu or the Downloads page;
/// the same as every time is no choice for once.
pub fn choose_once(app: &AppHandle, finish: Finish) -> anyhow::Result<()> {
    app.state::<prefs::Saved>().change(app, |prefs| {
        prefs.finish_once = (finish != prefs.finish).then_some(finish);
    })?;
    changed(app);
    Ok(())
}

fn open_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let finisher = app.state::<Finisher>();
    let page = finisher.page.clone();
    finisher.lock().waiting = true;
    let daemon = page.origin();
    let built = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(page))
        .title("Ottie")
        .decorations(false)
        .resizable(false)
        .always_on_top(true)
        // Shown once the page says how tall it is.
        .visible(false)
        .inner_size(WIDTH, FIRST_HEIGHT)
        .center()
        .on_navigation(move |url| url.origin() == daemon)
        .build();
    match built {
        Ok(window) => {
            let handle = app.clone();
            // Closed some other way than its buttons (Alt+F4): cancelled.
            window.on_window_event(move |event| {
                if let WindowEvent::CloseRequested { .. } = event {
                    cancel(&handle);
                }
            });
        }
        Err(_) => {
            // Without its window, the countdown goes on: told by a notification.
            finisher.lock().waiting = false;
            let prefs = app.state::<prefs::Saved>().get();
            let texts = Texts::for_language(prefs.language.as_deref());
            if let Some((power, seconds)) = finisher.counting() {
                notify::warn(
                    app,
                    texts.soon(power, seconds),
                    Some(texts.cancel_how.clone()),
                );
            }
        }
    }
}

fn close_window(app: &AppHandle) {
    app.state::<Finisher>().lock().waiting = false;
    if let Some(window) = app.get_webview_window(LABEL) {
        // Destroyed, not closed: closing it means Cancel.
        let _ = window.destroy();
    }
}

#[tauri::command]
pub fn finish(app: AppHandle) -> Finishing {
    finishing(&app)
}

/// Every time (Settings).
#[tauri::command]
pub fn set_finish(app: AppHandle, finish: Finish) -> Result<Finishing, String> {
    change(&app, |prefs| {
        prefs.finish = finish;
        if prefs.finish_once == Some(finish) {
            prefs.finish_once = None;
        }
    })
}

/// This time only (the tray menu, the Downloads page).
#[tauri::command]
pub fn set_finish_once(app: AppHandle, finish: Finish) -> Result<Finishing, String> {
    choose_once(&app, finish).map_err(|err| format!("{err:#}"))?;
    Ok(finishing(&app))
}

/// The countdown's length, in seconds.
#[tauri::command]
pub fn set_countdown(app: AppHandle, seconds: u32) -> Result<Finishing, String> {
    if !COUNTDOWN.contains(&seconds) {
        return Err("out of range".to_owned());
    }
    change(&app, |prefs| prefs.countdown = seconds)
}

#[tauri::command]
pub fn set_countdown_in(app: AppHandle, countdown_in: CountdownIn) -> Result<Finishing, String> {
    change(&app, |prefs| prefs.countdown_in = countdown_in)
}

#[tauri::command]
pub fn set_finish_waits(app: AppHandle, on: bool) -> Result<Finishing, String> {
    change(&app, |prefs| prefs.finish_waits_for_scheduled = on)
}

#[tauri::command]
pub fn cancel_finish(app: AppHandle) {
    cancel(&app);
}

/// "Shut down now" in the countdown's window: no need to wait.
#[tauri::command]
pub fn finish_now(app: AppHandle) {
    act(&app, None);
}

/// The countdown's page is drawn and says how tall it is.
#[tauri::command]
pub fn countdown_ready(window: WebviewWindow, finisher: State<'_, Finisher>, height: f64) {
    if window.label() != LABEL || !height.is_finite() {
        return;
    }
    let _ = window.set_size(LogicalSize::new(WIDTH, height.clamp(HEIGHTS.0, HEIGHTS.1)));
    let _ = window.center();
    if std::mem::take(&mut finisher.lock().waiting) {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn change(app: &AppHandle, change: impl FnOnce(&mut Prefs)) -> Result<Finishing, String> {
    app.state::<prefs::Saved>()
        .change(app, change)
        .map_err(|err| format!("{err:#}"))?;
    changed(app);
    Ok(finishing(app))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_started_while_waiting_to_quit_keeps_ottie_open() {
        let mut counting = Counting::default();
        let id = counting.quit_later();
        assert!(!counting.busy());
        assert!(!counting.quits(id));
        // Finished again: only the latest wait quits.
        let first = counting.quit_later();
        let second = counting.quit_later();
        assert!(!counting.quits(first));
        assert!(counting.quits(second));
        assert!(!counting.quits(second));
    }

    #[test]
    fn finished_means_the_last_one_has_just_ended() {
        let none = Left::default();
        assert_eq!(seen(none, true, true), Seen::Finished);
        // Paused, cancelled or removed: nothing finished.
        assert_eq!(seen(none, false, true), Seen::Idle);
        let running = Left {
            running: 1,
            scheduled: 0,
        };
        assert_eq!(seen(running, true, true), Seen::Busy);
    }

    #[test]
    fn scheduled_downloads_count_unless_the_setting_says() {
        let scheduled = Left {
            running: 0,
            scheduled: 2,
        };
        assert_eq!(seen(scheduled, true, true), Seen::Busy);
        assert_eq!(seen(scheduled, true, false), Seen::Finished);
    }

    #[test]
    fn choices_read_as_the_page_names_them() {
        let names: Vec<String> = Finish::ALL
            .iter()
            .map(|finish| serde_json::to_string(finish).unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "\"nothing\"",
                "\"quit\"",
                "\"sleep\"",
                "\"hibernate\"",
                "\"shutDown\""
            ]
        );
        assert_eq!(Finish::Quit.power(), None);
        assert_eq!(Finish::of(Power::ShutDown), Finish::ShutDown);
    }
}
