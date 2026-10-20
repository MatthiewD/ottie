//! The tray icon (docs/decisions/0034). A right click opens Ottie's own menu
//! (`popup.rs`), which has the limit's field. The system's menu below (open
//! the window, back to this computer's Ottie while another's is shown (0053), see what runs and how far it has come, pause or resume everything, switch the total
//! limit or pick one, choose what happens when everything has finished or cancel it
//! (docs/decisions/0040), watch the clipboard or not (0043), quit) is there while the daemon can't be reached, since
//! Ottie's menu is its page, and always on Linux, whose trays report no
//! clicks. What it shows comes from the daemon's events (`status.rs`); what
//! it does goes through the daemon's API, like any other front end. Both are
//! the daemon the window shows, this computer's or another's (0053).

use std::time::Duration;

use tauri::image::Image;
use tauri::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};
use tokio::sync::watch;

use crate::client::Daemon;
use crate::finish::{self, Finish, Finisher};
use crate::remote::{self, Switch};
use crate::status::{Feed, Summary};
use crate::texts::Texts;
use crate::{popup, prefs};

const OPEN: &str = "open";
const SHOW_LOCAL: &str = "show-local";
const STATUS: &str = "status";
const PROGRESS: &str = "progress";
const PAUSE_ALL: &str = "pause-all";
const RESUME_ALL: &str = "resume-all";
const LIMIT: &str = "limit";
const LIMITS: &str = "limits";
/// A preset limit's item: `limit-<bytes per second>`.
const PRESET: &str = "limit-";
const OTHER_LIMIT: &str = "limit-other";
const FINISH: &str = "finish";
/// A choice's item: `finish-<its place in Finish::ALL>`.
const FINISH_CHOICE: &str = "finish-";
const CANCEL_FINISH: &str = "finish-cancel";
const WATCH_CLIPBOARD: &str = "watch-clipboard";
const QUIT: &str = "quit";

/// The limits offered in the system's menu, in bytes per second, as on the
/// page (`LIMIT_PRESETS` in `ui/src/lib/settings.ts`). "Other…" opens Ottie's
/// menu, which has a field.
const PRESETS: [u64; 8] = [
    256 * 1024,
    512 * 1024,
    1024 * 1024,
    2 * 1024 * 1024,
    5 * 1024 * 1024,
    10 * 1024 * 1024,
    20 * 1024 * 1024,
    50 * 1024 * 1024,
];
/// Linux trays report no clicks: the system's menu is always there.
const ALWAYS_MENU: bool = cfg!(target_os = "linux");

/// Where the progress goes in the system's menu while there's one: under
/// the status (after "Open Ottie" and a separator).
const PROGRESS_AT: usize = 3;
/// Where Cancel goes during a countdown: after the "When everything has
/// finished" submenu (one further down with the progress).
const CANCEL_AT: usize = 8;
/// How often the tray's texts, menu and icon are brought up to date.
const REFRESH: Duration = Duration::from_secs(1);
/// Switching the total limit on for the first time, as on the page
/// (`DEFAULT_LIMIT` in `ui/src/lib/settings.ts`).
const DEFAULT_LIMIT: u64 = 10 * 1024 * 1024;
/// The size of a tray icon at 100 %, in pixels.
const BASE_SIZE: f64 = 16.0;

/// The drawn tray icons (`res/icons/png/tray`), by size: white for dark
/// taskbars, black for light ones.
const WHITE: [(u32, &[u8]); 5] = [
    (
        16,
        include_bytes!("../../res/icons/png/tray/ottie-tray-white-16.png"),
    ),
    (
        24,
        include_bytes!("../../res/icons/png/tray/ottie-tray-white-24.png"),
    ),
    (
        32,
        include_bytes!("../../res/icons/png/tray/ottie-tray-white-32.png"),
    ),
    (
        48,
        include_bytes!("../../res/icons/png/tray/ottie-tray-white-48.png"),
    ),
    (
        64,
        include_bytes!("../../res/icons/png/tray/ottie-tray-white-64.png"),
    ),
];
const BLACK: [(u32, &[u8]); 5] = [
    (
        16,
        include_bytes!("../../res/icons/png/tray/ottie-tray-black-16.png"),
    ),
    (
        24,
        include_bytes!("../../res/icons/png/tray/ottie-tray-black-24.png"),
    ),
    (
        32,
        include_bytes!("../../res/icons/png/tray/ottie-tray-black-32.png"),
    ),
    (
        48,
        include_bytes!("../../res/icons/png/tray/ottie-tray-black-48.png"),
    ),
    (
        64,
        include_bytes!("../../res/icons/png/tray/ottie-tray-black-64.png"),
    ),
];

/// What the menu's actions need, as managed state.
struct Actions {
    summaries: watch::Receiver<Summary>,
}

/// The menu's items whose text or state changes.
struct Items {
    open: MenuItem<Wry>,
    /// In the menu only while another computer's Ottie is shown.
    show_local: MenuItem<Wry>,
    status: MenuItem<Wry>,
    /// In the menu only while running downloads have a known size.
    progress: MenuItem<Wry>,
    pause_all: MenuItem<Wry>,
    resume_all: MenuItem<Wry>,
    limit: CheckMenuItem<Wry>,
    limits: Submenu<Wry>,
    presets: Vec<(u64, CheckMenuItem<Wry>)>,
    other_limit: MenuItem<Wry>,
    finish: Submenu<Wry>,
    finish_choices: Vec<(Finish, CheckMenuItem<Wry>)>,
    /// In the menu only during a countdown.
    cancel_finish: MenuItem<Wry>,
    watch_clipboard: CheckMenuItem<Wry>,
    quit: MenuItem<Wry>,
}

/// Puts Ottie in the tray, and keeps it up to date with the daemon shown.
pub fn start(app: &AppHandle) -> tauri::Result<()> {
    let summaries = app.state::<Feed>().subscribe();
    let texts = Texts::for_language(app.state::<prefs::Saved>().get().language.as_deref());
    let items = Items {
        open: MenuItem::with_id(app, OPEN, &texts.open, true, None::<&str>)?,
        show_local: MenuItem::with_id(app, SHOW_LOCAL, &texts.show_local, true, None::<&str>)?,
        status: MenuItem::with_id(
            app,
            STATUS,
            texts.status(&Summary::default()),
            false,
            None::<&str>,
        )?,
        progress: MenuItem::with_id(app, PROGRESS, "", false, None::<&str>)?,
        pause_all: MenuItem::with_id(app, PAUSE_ALL, &texts.pause_all, false, None::<&str>)?,
        resume_all: MenuItem::with_id(app, RESUME_ALL, &texts.resume_all, false, None::<&str>)?,
        limit: CheckMenuItem::with_id(
            app,
            LIMIT,
            texts.limit(DEFAULT_LIMIT),
            false,
            false,
            None::<&str>,
        )?,
        limits: Submenu::with_id(app, LIMITS, &texts.change_limit, false)?,
        presets: PRESETS
            .into_iter()
            .map(|limit| {
                let id = format!("{PRESET}{limit}");
                let item =
                    CheckMenuItem::with_id(app, id, texts.rate(limit), true, false, None::<&str>);
                item.map(|item| (limit, item))
            })
            .collect::<tauri::Result<_>>()?,
        other_limit: MenuItem::with_id(app, OTHER_LIMIT, &texts.other_limit, true, None::<&str>)?,
        finish: Submenu::with_id(app, FINISH, &texts.when_finished, false)?,
        finish_choices: Finish::ALL
            .into_iter()
            .enumerate()
            .map(|(place, finish)| {
                let id = format!("{FINISH_CHOICE}{place}");
                let text = texts.choice(finish);
                let item = CheckMenuItem::with_id(app, id, text, true, false, None::<&str>);
                item.map(|item| (finish, item))
            })
            .collect::<tauri::Result<_>>()?,
        cancel_finish: MenuItem::with_id(app, CANCEL_FINISH, "", true, None::<&str>)?,
        watch_clipboard: CheckMenuItem::with_id(
            app,
            WATCH_CLIPBOARD,
            &texts.watch_clipboard,
            true,
            false,
            None::<&str>,
        )?,
        quit: MenuItem::with_id(app, QUIT, &texts.quit, true, None::<&str>)?,
    };
    let presets: Vec<&dyn IsMenuItem<Wry>> = items
        .presets
        .iter()
        .map(|(_, item)| item as &dyn IsMenuItem<Wry>)
        .collect();
    items.limits.append_items(&presets)?;
    items.limits.append(&PredefinedMenuItem::separator(app)?)?;
    items.limits.append(&items.other_limit)?;
    for (_, item) in &items.finish_choices {
        items.finish.append(item)?;
    }
    let menu = Menu::with_items(
        app,
        &[
            &items.open,
            &PredefinedMenuItem::separator(app)?,
            &items.status,
            &items.pause_all,
            &items.resume_all,
            &items.limit,
            &items.limits,
            &items.finish,
            &items.watch_clipboard,
            &PredefinedMenuItem::separator(app)?,
            &items.quit,
        ],
    )?;
    let look = Look::now(app);
    let tray = TrayIconBuilder::with_id("main")
        .icon(look.icon())
        // macOS draws it in the menu bar's colour.
        .icon_as_template(true)
        .tooltip("Ottie")
        .menu(&menu)
        // A click opens the window, a right click a menu.
        .show_menu_on_left_click(false)
        .on_tray_icon_event(clicked)
        .on_menu_event(chosen)
        .build(app)?;
    app.manage(Actions {
        summaries: summaries.clone(),
    });
    tauri::async_runtime::spawn(refresh(app.clone(), tray, menu, items, look, summaries));
    Ok(())
}

fn clicked(tray: &TrayIcon, event: TrayIconEvent) {
    let TrayIconEvent::Click {
        button,
        button_state: MouseButtonState::Up,
        position,
        ..
    } = event
    else {
        return;
    };
    let app = tray.app_handle();
    match button {
        MouseButton::Left => crate::show(app),
        // Without the daemon, the system's menu opens instead.
        MouseButton::Right if app.state::<Actions>().summaries.borrow().connected => {
            popup::toggle(app, position);
        }
        _ => {}
    }
}

fn chosen(app: &AppHandle, event: MenuEvent) {
    let actions = app.state::<Actions>();
    let summary = *actions.summaries.borrow();
    let id = event.id().as_ref();
    let shown = app.state::<Switch>().shown();
    // The daemon's actions, while there's one to act on.
    let daemon = || shown.as_ref().map(|target| target.daemon.clone());
    match id {
        OPEN => crate::show(app),
        SHOW_LOCAL => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                // Failed (this computer's didn't start): the menu stays as it is.
                if remote::show_local(&app).await.is_ok() {
                    crate::show(&app);
                }
            });
        }
        OTHER_LIMIT => {
            if let Ok(at) = app.cursor_position() {
                popup::toggle(app, at);
            }
        }
        QUIT => app.exit(0),
        CANCEL_FINISH => finish::cancel(app),
        WATCH_CLIPBOARD => {
            let saved = app.state::<prefs::Saved>();
            let on = !saved.get().watch_clipboard;
            // Failed: the next refresh shows what's saved.
            let _ = prefs::change_desktop(app, &saved, |prefs| prefs.watch_clipboard = on);
        }
        PAUSE_ALL => {
            if let Some(daemon) = daemon() {
                tauri::async_runtime::spawn(async move { pause_all(&daemon).await });
            }
        }
        RESUME_ALL => {
            if let Some(daemon) = daemon() {
                tauri::async_runtime::spawn(async move { resume_all(&daemon).await });
            }
        }
        LIMIT => {
            let limit = if summary.global_limit > 0 {
                0
            } else {
                last_limit(app)
            };
            let Some(daemon) = daemon() else {
                return;
            };
            tauri::async_runtime::spawn(async move {
                // Failed: the next refresh puts the check mark back.
                let _ = daemon.set_global_limit(limit).await;
            });
        }
        _ => {
            let choice = id
                .strip_prefix(FINISH_CHOICE)
                .and_then(|place| place.parse::<usize>().ok())
                .and_then(|place| Finish::ALL.get(place));
            if let Some(&finish) = choice {
                // Failed: the next refresh shows what's saved.
                let _ = finish::choose_once(app, finish);
                return;
            }
            let preset = id.strip_prefix(PRESET).and_then(|limit| limit.parse().ok());
            if let (Some(limit), Some(daemon)) = (preset, daemon()) {
                tauri::async_runtime::spawn(async move {
                    let _ = daemon.set_global_limit(limit).await;
                });
            }
        }
    }
}

/// Every running or waiting download. The waiting ones first: otherwise
/// each running one paused would start one of them. Best effort: one that
/// finished or failed meanwhile is refused, and the rest go on.
async fn pause_all(daemon: &Daemon) {
    let Ok(jobs) = daemon.jobs().await else {
        return;
    };
    let waiting = |state: &str| state == "queued";
    let running = |state: &str| matches!(state, "connecting" | "downloading" | "retrying");
    for which in [waiting as fn(&str) -> bool, running] {
        for job in jobs.iter().filter(|job| !job.moving && which(&job.state)) {
            let _ = daemon.pause(&job.id).await;
        }
    }
}

/// Every paused download, in the order they were added. Failed ones stay as
/// they are: they need a look first.
async fn resume_all(daemon: &Daemon) {
    let Ok(jobs) = daemon.jobs().await else {
        return;
    };
    for job in jobs
        .iter()
        .filter(|job| !job.moving && job.state == "paused")
    {
        let _ = daemon.resume(&job.id).await;
    }
}

fn last_limit(app: &AppHandle) -> u64 {
    app.state::<prefs::Saved>()
        .get()
        .last_limit
        .filter(|&limit| limit > 0)
        .unwrap_or(DEFAULT_LIMIT)
}

/// Brings the tray up to date every second: the status and progress (in the
/// menu and the tooltip), which actions apply, the language, and the icon
/// for the taskbar's colours.
async fn refresh(
    app: AppHandle,
    tray: TrayIcon,
    menu: Menu<Wry>,
    items: Items,
    mut look: Look,
    summaries: watch::Receiver<Summary>,
) {
    let mut language = None;
    let mut texts = Texts::default();
    let mut shown = Shown::default();
    // Whether the progress item is in the menu, and Cancel.
    let mut progress_listed = false;
    let mut cancel_listed = false;
    // Whether "Show this computer's Ottie" is in the menu (after "Open Ottie").
    let mut local_listed = false;
    let mut tick = tokio::time::interval(REFRESH);
    loop {
        tick.tick().await;
        let prefs = app.state::<prefs::Saved>().get();
        if language.as_ref() != Some(&prefs.language) {
            texts = Texts::for_language(prefs.language.as_deref());
            language = Some(prefs.language);
            // Every text again, in the new language.
            shown = Shown::default();
            let _ = items.open.set_text(&texts.open);
            let _ = items.show_local.set_text(&texts.show_local);
            let _ = items.pause_all.set_text(&texts.pause_all);
            let _ = items.resume_all.set_text(&texts.resume_all);
            let _ = items.quit.set_text(&texts.quit);
            let _ = items.limits.set_text(&texts.change_limit);
            let _ = items.other_limit.set_text(&texts.other_limit);
            for (limit, item) in &items.presets {
                let _ = item.set_text(texts.rate(*limit));
            }
            let _ = items.finish.set_text(&texts.when_finished);
            for (finish, item) in &items.finish_choices {
                let _ = item.set_text(texts.choice(*finish));
            }
            let _ = items.watch_clipboard.set_text(&texts.watch_clipboard);
        }
        let summary = *summaries.borrow();
        let limit = if summary.global_limit > 0 {
            summary.global_limit
        } else {
            last_limit(&app)
        };
        let finisher = app.state::<Finisher>();
        let counting = finisher.counting();
        let now = Shown {
            cancel: counting.map(|(power, seconds)| texts.cancel_soon(power, seconds)),
            status: Some(texts.status(&summary)),
            progress: summary.progress.map(|progress| texts.progress(&progress)),
            can_pause: summary.can_pause(),
            can_resume: summary.can_resume(),
            limit: Some(texts.limit(limit)),
            can_limit: summary.can_limit(),
            connected: summary.connected,
            name: app
                .state::<Switch>()
                .shown()
                .and_then(|target| target.name.clone()),
        };
        // Best effort below: a tray that misses one update gets the next.
        if now.status != shown.status
            && let Some(status) = &now.status
        {
            let _ = items.status.set_text(status);
        }
        // Another computer's Ottie is shown: a way back to this one's, even
        // when that page can't load (docs/decisions/0053).
        let remote = now.name.is_some();
        if remote != local_listed {
            local_listed = if remote {
                menu.insert(&items.show_local, 1).is_ok()
            } else {
                menu.remove(&items.show_local).is_err()
            };
        }
        match &now.progress {
            Some(progress) => {
                if now.progress != shown.progress {
                    let _ = items.progress.set_text(progress);
                }
                if !progress_listed {
                    let at = PROGRESS_AT + usize::from(local_listed);
                    progress_listed = menu.insert(&items.progress, at).is_ok();
                }
            }
            None => {
                if progress_listed && menu.remove(&items.progress).is_ok() {
                    progress_listed = false;
                }
            }
        }
        match &now.cancel {
            Some(cancel) => {
                if now.cancel != shown.cancel {
                    let _ = items.cancel_finish.set_text(cancel);
                }
                if !cancel_listed {
                    let at = CANCEL_AT + usize::from(progress_listed) + usize::from(local_listed);
                    cancel_listed = menu.insert(&items.cancel_finish, at).is_ok();
                }
            }
            None => {
                if cancel_listed && menu.remove(&items.cancel_finish).is_ok() {
                    cancel_listed = false;
                }
            }
        }
        if (&now.status, &now.progress, &now.name) != (&shown.status, &shown.progress, &shown.name)
        {
            // "Ottie" (and which computer's, if not this one's), what runs, how
            // far it has come: below the 127 characters Windows shows.
            let title = match &now.name {
                Some(name) => format!("Ottie – {}", name.chars().take(40).collect::<String>()),
                None => "Ottie".to_owned(),
            };
            let lines = [title.as_str()]
                .into_iter()
                .chain(now.status.as_deref())
                .chain(now.progress.as_deref());
            let _ = tray.set_tooltip(Some(lines.collect::<Vec<_>>().join("\n")));
        }
        if now.can_pause != shown.can_pause {
            let _ = items.pause_all.set_enabled(now.can_pause);
        }
        if now.can_resume != shown.can_resume {
            let _ = items.resume_all.set_enabled(now.can_resume);
        }
        if now.limit != shown.limit
            && let Some(limit) = &now.limit
        {
            let _ = items.limit.set_text(limit);
        }
        if now.can_limit != shown.can_limit {
            let _ = items.limit.set_enabled(now.can_limit);
            let _ = items.limits.set_enabled(now.can_limit);
        }
        if now.connected != shown.connected {
            let _ = items.finish.set_enabled(now.connected);
            let attached = (ALWAYS_MENU || !now.connected).then(|| menu.clone());
            let _ = tray.set_menu(attached);
        }
        // Checked by the items themselves: a click ticks one before the
        // daemon has answered, and nothing else would untick it if that failed.
        tick_if(&items.limit, summary.global_limit > 0);
        tick_if(&items.watch_clipboard, prefs.watch_clipboard);
        for (limit, item) in &items.presets {
            tick_if(item, summary.global_limit == *limit);
        }
        // What happens next time: for once, else every time. One the
        // computer can't do stays listed, greyed out.
        let next = prefs.finish_once.unwrap_or(prefs.finish);
        let able = finisher.able();
        for (finish, item) in &items.finish_choices {
            tick_if(item, *finish == next);
            let can = finish.power().is_none_or(|power| able.to(power));
            if item.is_enabled().is_ok_and(|enabled| enabled != can) {
                let _ = item.set_enabled(can);
            }
        }
        shown = now;
        let next = Look::now(&app);
        if next != look {
            look = next;
            let _ = tray.set_icon(Some(look.icon()));
        }
    }
}

fn tick_if(item: &CheckMenuItem<Wry>, on: bool) {
    if item.is_checked().is_ok_and(|checked| checked != on) {
        let _ = item.set_checked(on);
    }
}

/// What the menu shows now, to change only what differs.
#[derive(Default)]
struct Shown {
    /// Cancel, during a countdown.
    cancel: Option<String>,
    status: Option<String>,
    progress: Option<String>,
    can_pause: bool,
    can_resume: bool,
    limit: Option<String>,
    can_limit: bool,
    connected: bool,
    /// Another computer's Ottie shown in the window, by its name.
    name: Option<String>,
}

/// Which icon suits the tray: its colour and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Look {
    /// Black, for a light taskbar; else white.
    black: bool,
    size: u32,
}

impl Look {
    fn now(app: &AppHandle) -> Look {
        let scale = app
            .primary_monitor()
            .ok()
            .flatten()
            .map_or(1.0, |monitor| monitor.scale_factor());
        Look {
            black: light_taskbar(),
            size: size_for(scale),
        }
    }

    fn icon(self) -> Image<'static> {
        let icons = if self.black { BLACK } else { WHITE };
        let (_, png) = icons
            .into_iter()
            .find(|&(size, _)| size == self.size)
            .unwrap_or(icons[0]);
        // The PNGs are built in and tested, so this never fails; if it did,
        // an empty icon beats no tray at all.
        Image::from_bytes(png).unwrap_or_else(|_| Image::new_owned(vec![0; 4], 1, 1))
    }
}

/// The drawn size for a display scale: the smallest one at least as big as
/// the icon's place (16 px at 100 %), else the biggest.
fn size_for(scale: f64) -> u32 {
    let wanted = BASE_SIZE * scale;
    WHITE
        .iter()
        .map(|&(size, _)| size)
        .find(|&size| f64::from(size) >= wanted - 0.01)
        .unwrap_or(64)
}

/// Windows: the taskbar's own setting ("Choose your default Windows mode"),
/// which can differ from the apps' one. Changes show at the next refresh.
#[cfg(windows)]
fn light_taskbar() -> bool {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|key| key.get_u32("SystemUsesLightTheme"))
        .is_ok_and(|light| light == 1)
}

/// macOS draws template icons in the menu bar's colour, from a black one.
#[cfg(target_os = "macos")]
fn light_taskbar() -> bool {
    true
}

/// Linux: most panels are dark, and there's no common way to ask.
#[cfg(not(any(windows, target_os = "macos")))]
fn light_taskbar() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_reads() {
        for (size, png) in WHITE.into_iter().chain(BLACK) {
            let image = Image::from_bytes(png).unwrap();
            assert_eq!((image.width(), image.height()), (size, size));
        }
    }

    #[test]
    fn the_size_follows_the_display_scale() {
        assert_eq!(size_for(1.0), 16);
        assert_eq!(size_for(1.25), 24);
        assert_eq!(size_for(1.5), 24);
        assert_eq!(size_for(2.0), 32);
        assert_eq!(size_for(3.5), 64);
        assert_eq!(size_for(5.0), 64);
    }
}
