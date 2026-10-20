//! `ottie serve`: the daemon.

use std::fs::{self, File, OpenOptions};
use std::future::IntoFuture;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use ottie_api::{ACCESS_FILE, Access, AccessConfig, Token, UiSource};
use ottie_core::{CategoryId, Manager, NativeBackend, Store};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::config::{self, CONFIG_FILE, DATABASE_FILE, DaemonArgs, Dirs, FileConfig, LOCK_FILE};
use crate::token;

#[derive(Args, Debug)]
pub struct ServeArgs {
    #[command(flatten)]
    pub daemon: DaemonArgs,
    /// Serve the web UI from this folder instead of the one built into Ottie
    #[arg(long, env = "OTTIE_UI_DIR", value_name = "FOLDER")]
    pub ui_dir: Option<PathBuf>,
    /// Where downloads go until you choose a folder for General [default: your Downloads folder]
    #[arg(long, env = "OTTIE_DOWNLOADS_DIR", value_name = "FOLDER")]
    pub downloads_dir: Option<PathBuf>,
    /// Write the log to this file, not to the terminal (past 10 MB, the old
    /// one moves to FILE.old)
    #[arg(long, value_name = "FILE")]
    pub log_file: Option<PathBuf>,
    /// Windows: let go of the console window at once (`ottie service install`).
    #[arg(long, hide = true)]
    pub detach_console: bool,
    /// Windows: run under the Service Control Manager (`--system`).
    #[arg(long, hide = true)]
    pub windows_service: bool,
    /// For a program that starts Ottie (the desktop app): print `ready` once
    /// listening, and stop when standard input closes.
    #[arg(long, hide = true)]
    pub managed: bool,
}

/// A log file past this moves to `<name>.old` at the next start.
const LOG_LIMIT: u64 = 10 * 1024 * 1024;

/// Runs until Ctrl+C, SIGTERM, `stop` (the Windows service's Stop) or, with
/// `--managed`, standard input closing.
pub async fn run(args: ServeArgs, stop: CancellationToken) -> Result<()> {
    // Watched from the start: if the app quits while we start, we stop too.
    let parent_gone = args.managed.then(stdin_closed);
    // `OTTIE_LOG=debug` (or `ottie_core=debug`…) shows more.
    let filter = EnvFilter::try_from_env("OTTIE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let logs = tracing_subscriber::fmt().with_env_filter(filter);
    match &args.log_file {
        Some(path) => logs
            .with_ansi(false)
            .with_writer(Mutex::new(log_file(path)?))
            .init(),
        // Colours only for a person: not in the journal or `docker logs`.
        None => logs
            .with_ansi(std::io::stderr().is_terminal())
            .with_writer(std::io::stderr)
            .init(),
    }
    let result = serve(args, stop, parent_gone).await;
    if let Err(err) = &result {
        // A service's standard error goes nowhere (or only to the journal).
        tracing::error!("{err:#}");
    }
    result
}

async fn serve(
    args: ServeArgs,
    stop: CancellationToken,
    parent_gone: Option<CancellationToken>,
) -> Result<()> {
    let daemon = &args.daemon;
    let dirs = Dirs::resolve(&daemon.dirs)?;
    let file = FileConfig::load(&dirs.config.join(CONFIG_FILE))?;
    let addr = config::listen_addr(daemon.address, daemon.port, &file);
    let token = token::load_or_create(&dirs.config)?;
    let access = Access::open(AccessConfig {
        token: Token::new(token.clone()),
        token_file: Some(token::path(&dirs.config)),
        file: Some(dirs.config.join(ACCESS_FILE)),
        folders: file.folders,
    })
    .context("can't read the devices' tokens")?;

    // The usual failure: another Ottie already has the port. Checked before
    // the database is opened, or this one would resume its downloads too.
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("can't listen on {addr} (is Ottie already running?)"))?;

    fs::create_dir_all(&dirs.data)
        .with_context(|| format!("can't create {}", dirs.data.display()))?;
    // Held until this function returns, after the downloads are saved.
    let _lock = lock(&dirs.data)?;
    let database = dirs.data.join(DATABASE_FILE);
    let store = Store::open(&database)
        .await
        .with_context(|| format!("can't open the database {}", database.display()))?;
    let manager = Manager::start(NativeBackend::new()?, store).await?;
    let downloads = args
        .downloads_dir
        .clone()
        .or_else(|| dirs.default_downloads());
    default_general_folder(&manager, downloads).await?;

    if !addr.ip().is_loopback() {
        tracing::warn!(%addr, "the API is open to other computers: keep it behind a VPN or reverse proxy");
    }
    tracing::info!(%addr, data = %dirs.data.display(), "Ottie {} is running", env!("CARGO_PKG_VERSION"));
    // Only for a person at a terminal; never into logs (systemd, Docker).
    if std::io::stdout().is_terminal() {
        println!(
            "Ottie is running. Open it, already signed in:\n  {}\n\
             (or run `ottie open`). Ctrl+C stops Ottie; downloads continue at the next start.",
            config::ui_url(addr, &token)
        );
    }

    if args.managed {
        // Connections already wait in the listener's queue: the app can open
        // its window now.
        println!("ready");
    }

    let ui = match args.ui_dir {
        Some(folder) => UiSource::Folder(folder),
        None => UiSource::Embedded,
    };
    let shutdown = CancellationToken::new();
    let app = ottie_api::router(manager.clone(), access, shutdown.clone(), ui);
    let draining = shutdown.clone();
    let server = axum::serve(listener, app)
        // Also closes event streams, or the server would wait for them.
        .with_graceful_shutdown(async move { draining.cancelled().await })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        served = &mut server => {
            // It only ends by itself when it fails.
            served.context("the API server failed")?;
        }
        () = stop_requested(parent_gone, stop) => {}
    }

    // The downloads stop at once, while the API finishes the requests under
    // way, for at most `DRAIN`: a request that never ends (a body that never
    // comes) can't keep the downloads running.
    tracing::info!("stopping: saving downloads");
    shutdown.cancel();
    let drained = async {
        if tokio::time::timeout(DRAIN, &mut server).await.is_err() {
            tracing::warn!("requests still open after {} s: closed", DRAIN.as_secs());
        }
    };
    let (stopped, ()) = tokio::join!(manager.shutdown(), drained);
    stopped?;
    Ok(())
}

/// How long requests under way may take to finish once Ottie stops. The
/// desktop app waits longer for its daemon (`app/src/daemon.rs`).
const DRAIN: Duration = Duration::from_secs(5);

/// One daemon per data folder: another Ottie on another port (another
/// `--port`, config.toml) must not resume the same downloads into the same
/// files. The system releases the lock when the process ends, even in a
/// crash, so the file left behind never stops the next start.
fn lock(data: &Path) -> Result<File> {
    let path = data.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("can't open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => anyhow::bail!(
            "Ottie is already running with the data folder {} (on another port?)",
            data.display()
        ),
        Err(fs::TryLockError::Error(err)) => {
            // Some network folders can't lock: run as before, unguarded.
            tracing::warn!(error = %err, "can't lock the data folder: make sure only one Ottie uses it");
            Ok(file)
        }
    }
}

/// Until the user picks a folder for General, it's the OS's Downloads
/// folder, so a download can be added with just a link.
async fn default_general_folder(manager: &Manager, downloads: Option<PathBuf>) -> Result<()> {
    let categories = manager.categories().await?;
    let Some(general) = categories.into_iter().find(|c| c.id == CategoryId::GENERAL) else {
        return Ok(());
    };
    if general.settings.folder.is_some() {
        return Ok(());
    }
    let Some(downloads) = downloads else {
        tracing::warn!("no Downloads folder found: every download must say where to go");
        return Ok(());
    };
    tracing::info!(folder = %downloads.display(), "downloads go to this folder by default");
    let mut settings = general.settings;
    settings.folder = Some(downloads);
    manager
        .update_category(CategoryId::GENERAL, settings)
        .await?;
    Ok(())
}

/// Appended to, so a service restarted after a crash keeps what led to it.
fn log_file(path: &Path) -> Result<File> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder).with_context(|| format!("can't create {}", folder.display()))?;
    }
    if fs::metadata(path).is_ok_and(|file| file.len() > LOG_LIMIT) {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        // If it can't move, it grows: better than no log.
        let _ = fs::rename(path, old);
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("can't write the log {}", path.display()))
}

/// `--managed`: cancelled once standard input closes, i.e. the program that
/// started us quit (or crashed: the OS closes its end of the pipe then too).
fn stdin_closed() -> CancellationToken {
    let closed = CancellationToken::new();
    let signal = closed.clone();
    // A plain thread, not a tokio task: a read that's still waiting would
    // hold up the runtime's shutdown after Ctrl+C.
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
        signal.cancel();
    });
    closed
}

/// Ctrl+C, (Unix) the SIGTERM that systemd, launchd and Docker send, `stop`,
/// or (`--managed`) standard input closing.
async fn stop_requested(parent_gone: Option<CancellationToken>, stop: CancellationToken) {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            // Without a handler we can't stop gracefully; keep running.
            tracing::warn!(error = %err, "can't listen for Ctrl+C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    let parent_gone = async {
        match parent_gone {
            Some(closed) => closed.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
        () = parent_gone => tracing::info!("the program that started Ottie has quit"),
        () = stop.cancelled() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn general_gets_the_downloads_folder_once() {
        let store = Store::open_in_memory().await.unwrap();
        let manager = Manager::start(NativeBackend::new().unwrap(), store)
            .await
            .unwrap();
        let folder = |m: Vec<ottie_core::Category>| m[0].settings.folder.clone();

        default_general_folder(&manager, Some("/dl".into()))
            .await
            .unwrap();
        assert_eq!(
            folder(manager.categories().await.unwrap()),
            Some("/dl".into())
        );
        // A folder already set (by the user, later) is kept.
        default_general_folder(&manager, Some("/other".into()))
            .await
            .unwrap();
        assert_eq!(
            folder(manager.categories().await.unwrap()),
            Some("/dl".into())
        );
    }
}
