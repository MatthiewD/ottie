//! Ottie's desktop app: a window on the daemon's web UI (docs/decisions/0017).
//! Downloads go through the daemon's HTTP API, never through the app.

// Release builds on Windows: no console window behind the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod client;
mod clipboard;
mod config;
mod daemon;
mod files;
mod finish;
mod identity;
mod notify;
mod player;
mod popup;
mod power;
mod prefs;
mod remote;
mod sound;
mod status;
mod texts;
mod tray;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use anyhow::anyhow;
use tauri::ipc::CapabilityBuilder;
use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, RunEvent, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use crate::daemon::Started;
use crate::remote::Switch;
use crate::texts::Texts;

const WINDOW: &str = "main";
/// The daemon's play links (docs/decisions/0029).
const PLAY_PATH: &str = "/api/v1/stream/";
/// Ottie's own pages on the web (e.g. to help translate it), which the page
/// may open in the system's browser.
const PROJECT: &str = "https://github.com/MatthiewD/ottie/";

/// What the page may do in the app: control its window, since it draws its
/// own title bar (docs/decisions/0019), open the system folder picker, which
/// only hands back the path the user chose, choose how to play
/// (docs/decisions/0031), set what closing does, when to notify
/// (docs/decisions/0035), what happens at sign-in (0039) and the tray's
/// language, in Ottie's tray menu, open the window, quit, and hide the
/// menu (docs/decisions/0034), choose what happens when everything has
/// finished, or cancel it (0040), and watch the clipboard, then offer what
/// it caught (0043), open a download's file or folder (0045), named by
/// its ID, and show another computer's Ottie (0053). Only these, and only
/// for this computer's daemon's pages.
const PAGE_PERMISSIONS: [&str; 47] = [
    "core:window:allow-minimize",
    "core:window:allow-toggle-maximize",
    "core:window:allow-close",
    "core:window:allow-is-maximized",
    // Dragging the title bar, and double-clicking it to maximize.
    "core:window:allow-start-dragging",
    "core:window:allow-internal-toggle-maximize",
    // To hear when the window is resized (maximize or restore button).
    "core:event:allow-listen",
    "core:event:allow-unlisten",
    // "Browse…" next to a category's folder.
    "dialog:allow-open",
    // "Play with" in Settings: this app's own commands (see build.rs).
    "allow-player",
    "allow-set-play-with",
    "allow-choose-player",
    // Settings → Desktop app and Notifications (and the first-start setup's
    // sign-in choice), and the page's language for the tray.
    "allow-desktop",
    "allow-set-startup",
    "allow-set-close-to-tray",
    "allow-set-watch-clipboard",
    "allow-set-copied",
    "allow-set-notify",
    "allow-set-notify-failed",
    "allow-set-sounds",
    "allow-set-sound",
    "allow-play-sound",
    "allow-set-language",
    // Ottie's tray menu (`popup.rs`).
    "core:window:allow-hide",
    "allow-menu-ready",
    "allow-show-main",
    "allow-quit-app",
    // When everything has finished (`finish.rs`): Settings, the Downloads
    // page, the tray menu and the countdown's window.
    "allow-finish",
    "allow-set-finish",
    "allow-set-finish-once",
    "allow-set-countdown",
    "allow-set-countdown-in",
    "allow-set-finish-waits",
    "allow-cancel-finish",
    "allow-finish-now",
    "allow-countdown-ready",
    // The window offering links copied elsewhere (`clipboard.rs`).
    "allow-caught",
    "allow-caught-ready",
    "allow-caught-done",
    "allow-caught-more",
    // Open and Show in folder (`files.rs`).
    "allow-open-file",
    "allow-show-in-folder",
    // Ottie on other computers (`remote.rs`), and Settings → Desktop app.
    "allow-daemons",
    "allow-switch-daemon",
    "allow-add-daemon",
    "allow-remove-daemon",
    "allow-set-local-daemon",
];

/// What another computer's Ottie's pages may do in the app
/// (docs/decisions/0053): its own window, frame and title bar, the tray
/// menu and the window for copied links (they're its pages too), the
/// tray's language, and switching. Nothing that reaches this computer's
/// files, programs, settings or power: such a page is only as trustworthy
/// as that computer.
const REMOTE_PERMISSIONS: [&str; 21] = [
    "core:window:allow-minimize",
    "core:window:allow-toggle-maximize",
    "core:window:allow-close",
    "core:window:allow-is-maximized",
    "core:window:allow-start-dragging",
    "core:window:allow-internal-toggle-maximize",
    "core:event:allow-listen",
    "core:event:allow-unlisten",
    "allow-set-language",
    "core:window:allow-hide",
    "allow-menu-ready",
    "allow-show-main",
    "allow-quit-app",
    "allow-caught",
    "allow-caught-ready",
    "allow-caught-done",
    "allow-caught-more",
    "allow-daemons",
    "allow-switch-daemon",
    "allow-add-daemon",
    "allow-remove-daemon",
];

/// This computer's daemon, if the app started it: stopped when the app quits.
#[derive(Default)]
struct Owned {
    started: Mutex<Option<Started>>,
    /// Set before the daemon is taken to be stopped: a daemon that stops
    /// from then on isn't started again (`remote::watch_over`).
    quitting: AtomicBool,
}

/// The window was asked for before it existed (a second launch while Ottie
/// starts in the tray at sign-in): it shows once made.
#[derive(Default)]
struct ShowRequested(AtomicBool);

fn main() -> tauri::Result<()> {
    let app = tauri::Builder::default()
        // First, so a second launch only brings this window forward (and
        // can't stop the daemon under it when it quits).
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show(app)))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(Owned::default())
        .manage(ShowRequested::default())
        .invoke_handler(tauri::generate_handler![
            player::player,
            player::set_play_with,
            player::choose_player,
            prefs::desktop,
            prefs::set_startup,
            prefs::set_close_to_tray,
            prefs::set_watch_clipboard,
            prefs::set_copied,
            prefs::set_notify,
            prefs::set_notify_failed,
            prefs::set_sounds,
            prefs::set_sound,
            prefs::play_sound,
            prefs::set_language,
            popup::menu_ready,
            popup::show_main,
            popup::quit_app,
            finish::finish,
            finish::set_finish,
            finish::set_finish_once,
            finish::set_countdown,
            finish::set_countdown_in,
            finish::set_finish_waits,
            finish::cancel_finish,
            finish::finish_now,
            finish::countdown_ready,
            clipboard::caught,
            clipboard::caught_ready,
            clipboard::caught_done,
            clipboard::caught_more,
            files::open_file,
            files::show_in_folder,
            remote::daemons,
            remote::switch_daemon,
            remote::add_daemon,
            remote::remove_daemon,
            prefs::set_local_daemon
        ])
        .setup(|app| {
            app.manage(player::Chosen::load(app.handle()));
            let saved = prefs::Saved::load(app.handle());
            let startup = saved.get().startup;
            app.manage(saved);
            autostart::refresh(app.handle());
            let at_sign_in = autostart::at_sign_in();
            let app = app.handle().clone();
            if at_sign_in && startup == autostart::Mode::Daemon {
                tauri::async_runtime::spawn(async move {
                    if let Err(err) = start_alone(&app).await {
                        cant_start(&app, &err);
                    }
                    app.exit(0);
                });
                return Ok(());
            }
            notify::listen(&app);
            tauri::async_runtime::spawn(async move {
                // At sign-in, Ottie starts in the tray.
                if let Err(err) = start(&app, !at_sign_in).await {
                    cant_start(&app, &err);
                    app.exit(1);
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())?;
    app.run(|app, event| {
        if let RunEvent::Exit = event {
            stop(app);
        }
    });
    Ok(())
}

/// Finds or starts this computer's daemon, unless the window shows another
/// computer's and Settings say it starts only when shown (docs/decisions/0053),
/// then opens the window on the one shown last (shown or not) and puts
/// Ottie in the tray.
async fn start(app: &AppHandle, shown: bool) -> anyhow::Result<()> {
    let ottie = daemon::sidecar()?;
    tauri::async_runtime::spawn(daemon::set_up_extension(ottie.clone()));
    let local = daemon::link(&ottie).await?;
    app.manage(status::Feed::new());
    app.manage(popup::Popup::new());
    app.manage(finish::Finisher::new(&local));
    app.manage(clipboard::Catcher::new());
    app.manage(Switch::new(app, ottie, local));
    let switch = app.state::<Switch>();
    let prefs = app.state::<prefs::Saved>().get();
    let texts = Texts::for_language(prefs.language.as_deref());
    let last = switch.last_shown();
    let mut local_failed = None;
    if last.is_none() || prefs.local_daemon == prefs::LocalDaemon::Always {
        match remote::start_local(app).await {
            Ok(_) => {}
            Err(err) if last.is_none() => return Err(err),
            Err(err) => local_failed = Some(err),
        }
    }
    let mut target = None;
    if let Some(id) = last {
        match remote::target(app, Some(id)).await {
            Ok(shown) => target = Some(shown),
            // Asleep, offline, or its token was removed there: this
            // computer's instead.
            Err(_) => {
                let name = switch.name_of(id).unwrap_or_default();
                notify::warn(
                    app,
                    texts.unreachable(&name),
                    Some(texts.showing_local.clone()),
                );
            }
        }
    }
    let target = match target {
        Some(target) => target,
        None => {
            if let Some(err) = local_failed.take() {
                return Err(err);
            }
            remote::start_local(app).await?;
            remote::target(app, None)
                .await
                .map_err(|_| anyhow!("Ottie didn't start on this computer"))?
        }
    };
    if let Some(err) = local_failed {
        notify::warn(app, texts.local_failed.clone(), Some(format!("{err:#}")));
    }
    let link = target.link.clone();
    remote::show(app, target, None).map_err(|_| anyhow!("Ottie on another computer is gone"))?;
    open_window(app, link, shown)?;
    tray::start(app)?;
    clipboard::start(app);
    Ok(())
}

/// At sign-in, set to start only the daemon (docs/decisions/0039): it runs
/// on its own, its log in the app's log folder, and the app quits. Opened
/// later, the app finds it, and quitting the app leaves it running.
async fn start_alone(app: &AppHandle) -> anyhow::Result<()> {
    let log = app.path().app_log_dir()?.join("daemon.log");
    daemon::start_alone(&daemon::sidecar()?, &log).await
}

fn cant_start(app: &AppHandle, err: &anyhow::Error) {
    app.dialog()
        .message(format!("{err:#}"))
        .title("Ottie can't start")
        .kind(MessageDialogKind::Error)
        .blocking_show();
}

/// This computer's daemon's pages get the app's commands
/// (`PAGE_PERMISSIONS`). Its address is only known now, hence a runtime
/// capability.
/// Only once that daemon is known to be this computer's Ottie
/// (`remote::start_local`, docs/decisions/0058).
pub(crate) fn grant_local(app: &AppHandle, link: &Url) -> tauri::Result<()> {
    let page = PAGE_PERMISSIONS.into_iter().fold(
        CapabilityBuilder::new("page")
            .remote(link.origin().ascii_serialization())
            .local(false)
            .windows([WINDOW, popup::LABEL, finish::LABEL, clipboard::LABEL]),
        CapabilityBuilder::permission,
    );
    app.add_capability(page)
}

/// Another computer's Ottie at `origin` gets fewer (`REMOTE_PERMISSIONS`),
/// once it's shown; never the countdown's window, which is this computer's.
fn grant_remote(app: &AppHandle, origin: &str) -> tauri::Result<()> {
    let name: String = origin
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let page = REMOTE_PERMISSIONS.into_iter().fold(
        CapabilityBuilder::new(format!("remote-{name}"))
            .remote(origin.to_owned())
            .local(false)
            .windows([WINDOW, popup::LABEL, clipboard::LABEL]),
        CapabilityBuilder::permission,
    );
    app.add_capability(page)
}

/// Run in every page the app's windows open, before the page's own code:
/// tells it this computer's daemon's address, so another computer's page
/// knows to leave out what only this computer's can do (docs/decisions/0053).
fn page_script(app: &AppHandle) -> String {
    let local = app.state::<Switch>().local_origin();
    let quoted = serde_json::to_string(&local).unwrap_or_else(|_| "null".to_owned());
    format!("window.__OTTIE_LOCAL__ = {quoted};")
}

/// The pages the main window may go to: the daemon it shows (its address
/// changes when another is shown), and this computer's.
fn allowed(app: &AppHandle, url: &Url) -> bool {
    let switch = app.state::<Switch>();
    let origin = url.origin().ascii_serialization();
    origin == switch.local_origin()
        || switch
            .shown()
            .is_some_and(|target| target.link.origin().ascii_serialization() == origin)
}

fn open_window(app: &AppHandle, link: Url, shown: bool) -> tauri::Result<()> {
    let handle = app.clone();
    let navigating = app.clone();
    let window = WebviewWindowBuilder::new(app, WINDOW, WebviewUrl::External(link))
        .title("Ottie")
        // No system title bar: the page draws its own.
        .decorations(false)
        .inner_size(960.0, 680.0)
        // Room for the title bar folded as far as it goes (tabs as icons)
        // beside the window's buttons, and for a page under it.
        .min_inner_size(720.0, 480.0)
        .visible(shown || requested(app))
        .initialization_script(page_script(app))
        // Only the daemons' pages: a link to elsewhere must not turn this
        // window into a browser.
        .on_navigation(move |url| allowed(&navigating, url))
        // "Play" opens the daemon's play link in a new tab: here, in the
        // player chosen in Settings (another computer's too: a play link
        // needs no token). Ottie's own pages on the web open in the
        // system's browser. No other page opens a window.
        .on_new_window(move |url, _| {
            let shown = handle.state::<Switch>().shown();
            let ours = shown.is_some_and(|target| target.link.origin() == url.origin());
            if ours && url.path().starts_with(PLAY_PATH) {
                player::play(&handle, &url);
            } else if url.as_str().starts_with(PROJECT) {
                // Best effort: without a browser, the link does nothing.
                let _ = open::that_detached(url.as_str());
            }
            NewWindowResponse::Deny
        })
        .build()?;
    // Asked for while it was being made.
    if requested(app) {
        show(app);
    }
    let hidden = window.clone();
    let handle = app.clone();
    window.on_window_event(move |event| {
        // Closing hides the window, unless Settings say it quits
        // (docs/decisions/0034). The tray's "Quit Ottie" always quits.
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            if handle.state::<prefs::Saved>().get().close_to_tray {
                // Best effort: if it can't hide, it stays open.
                let _ = hidden.hide();
            } else {
                // Quits even with the tray's menu window still there, hidden.
                handle.exit(0);
            }
        }
    });
    Ok(())
}

/// Brings the window forward: from the tray, a second launch, or the
/// clipboard's More….
fn show(app: &AppHandle) {
    let mut window = app.get_webview_window(WINDOW);
    if window.is_none() {
        app.state::<ShowRequested>().0.store(true, Ordering::SeqCst);
        // Made meanwhile, and maybe before it could see the request: whichever
        // takes the request back shows it.
        window = app.get_webview_window(WINDOW).filter(|_| requested(app));
    }
    if let Some(window) = window {
        // Best effort: the user can still find the window themselves.
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Whether the window was asked for before it existed; takes the request.
fn requested(app: &AppHandle) -> bool {
    app.state::<ShowRequested>().0.swap(false, Ordering::SeqCst)
}

/// The app quits: stop the daemon if we started it. One that was already
/// running (a terminal, a service) keeps running.
fn stop(app: &AppHandle) {
    app.state::<Owned>().quitting.store(true, Ordering::SeqCst);
    if let Some(started) = owned(app).take() {
        tauri::async_runtime::block_on(started.stop());
    }
}

/// From now on this computer's daemon is `started` (`None`: not the app's,
/// as one found running), unless the app quits: then `started` is given
/// back, to be stopped.
pub(crate) fn keep(app: &AppHandle, started: Option<Started>) -> Result<(), Started> {
    let mut owned = owned(app);
    // Checked with the lock held: `stop` sets it before taking the lock.
    if quitting(app) {
        return started.map_or(Ok(()), Err);
    }
    *owned = started;
    Ok(())
}

/// Whether the app quits.
pub(crate) fn quitting(app: &AppHandle) -> bool {
    app.state::<Owned>().quitting.load(Ordering::SeqCst)
}

fn owned(app: &AppHandle) -> std::sync::MutexGuard<'_, Option<Started>> {
    // A panic while holding the lock leaves nothing half-done here.
    app.state::<Owned>()
        .inner()
        .started
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}
