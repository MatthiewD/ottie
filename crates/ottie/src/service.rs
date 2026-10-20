//! `ottie service`: Ottie as a service of the system, without the app
//! (docs/decisions/0051). For you: a systemd user unit, a LaunchAgent, a task
//! at sign-in. With `--system`, for the whole computer under an account of its
//! own: a systemd system unit, a Windows service.
//!
//! Each system's file (unit, plist, task) is plain text built here on every
//! system, so its tests run anywhere; only installing it is per system.

mod launchd;
mod systemd;
pub mod windows;

use std::ffi::OsStr;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::config::{self, CONFIG_FILE, DirArgs, Dirs, FileConfig};

#[cfg(target_os = "macos")]
use launchd as platform;
#[cfg(target_os = "linux")]
use systemd as platform;
#[cfg(windows)]
use windows as platform;

#[derive(Args, Debug)]
pub struct ServiceArgs {
    #[command(subcommand)]
    command: ServiceCommand,
}

#[derive(Subcommand, Debug)]
enum ServiceCommand {
    /// Set Ottie up to start on its own, and start it. Run it again after
    /// updating Ottie: it takes the new version.
    Install(InstallArgs),
    /// Stop the service and take it off. Downloads, settings and the database stay.
    Uninstall(ScopeArgs),
    /// Whether Ottie is set up as a service, for you and system-wide, and runs.
    Status,
}

#[derive(Args, Debug)]
struct ScopeArgs {
    /// The system-wide service (needs administrator rights)
    #[arg(long)]
    system: bool,
}

#[derive(Args, Debug)]
pub struct InstallArgs {
    /// Without `--system`: for you, when you sign in.
    #[command(flatten)]
    dirs: DirArgs,
    /// Where downloads go until you choose a folder for General
    #[arg(long, env = "OTTIE_DOWNLOADS_DIR", value_name = "FOLDER")]
    downloads_dir: Option<PathBuf>,
    /// Linux: also start at boot and keep running after you log out
    /// (systemd's "lingering")
    #[arg(long, conflicts_with = "system")]
    at_boot: bool,
}

/// What `install` hands each system's code.
pub struct Install {
    pub system: bool,
    /// Only systemd has lingering; elsewhere `prepare` refuses the flag.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub at_boot: bool,
    /// The log file, where the system keeps none (Linux: the journal does).
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub log: PathBuf,
    /// This `ottie`. The system-wide service runs a copy where its account
    /// can read it.
    pub exe: PathBuf,
    /// `serve` and its options, without the log file (each system decides).
    pub args: Vec<String>,
    /// Where it will listen (config.toml). Each system's code stops its own
    /// service first: if another Ottie still answers there, it's set up but
    /// not started.
    pub addr: SocketAddr,
}

pub fn run(args: ServiceArgs) -> Result<()> {
    match args.command {
        ServiceCommand::Install(args) => platform::install(&prepare(args)?),
        ServiceCommand::Uninstall(scope) => platform::uninstall(scope.system),
        ServiceCommand::Status => {
            for line in platform::status()? {
                println!("{line}");
            }
            // Yours, or the one the desktop app started.
            let addr = listen_addr(&Dirs::resolve(&DirArgs::default())?)?;
            if answers(addr) {
                println!("Ottie answers at {addr}.");
            } else {
                println!("Nothing answers at {addr}.");
            }
            Ok(())
        }
    }
}

fn prepare(args: InstallArgs) -> Result<Install> {
    if args.at_boot && !cfg!(target_os = "linux") {
        bail!("--at-boot is for Linux; elsewhere, --system starts Ottie at boot");
    }
    let dirs = Dirs::resolve(&args.dirs)?;
    let mut serve = vec!["serve".to_owned()];
    if dirs.system {
        serve.push("--system".to_owned());
    } else {
        for (flag, folder) in [
            ("--config-dir", &args.dirs.config_dir),
            ("--data-dir", &args.dirs.data_dir),
        ] {
            if let Some(folder) = folder {
                serve.extend([flag.to_owned(), text(&absolute(folder)?)?]);
            }
        }
    }
    if let Some(folder) = &args.downloads_dir {
        serve.extend(["--downloads-dir".to_owned(), text(&absolute(folder)?)?]);
    }
    let addr = listen_addr(&dirs)?;
    Ok(Install {
        system: dirs.system,
        at_boot: args.at_boot,
        // As absolute as `--data-dir`: the service may start in another folder.
        log: absolute(&dirs.data)?.join("ottie.log"),
        exe: std::env::current_exe().context("can't find where ottie is")?,
        args: serve,
        addr,
    })
}

/// Where Ottie with these folders listens: config.toml, else 127.0.0.1:24068.
/// A service doesn't get the `OTTIE_*` variables of the terminal it was
/// installed from, so only the file counts.
pub fn listen_addr(dirs: &Dirs) -> Result<SocketAddr> {
    let file = FileConfig::load(&dirs.config.join(CONFIG_FILE))?;
    Ok(config::listen_addr(None, None, &file))
}

/// Whether an Ottie (or anything) listens at `addr` on this computer.
pub fn answers(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&config::local(addr), Duration::from_secs(2)).is_ok()
}

/// Whether something still answers at `addr` after waiting up to `wait` for
/// it to stop.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn still_answers(addr: SocketAddr, wait: Duration) -> bool {
    let start = Instant::now();
    while answers(addr) {
        if start.elapsed() >= wait {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

/// A service starts in another folder than this terminal's.
fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("can't find {}", path.display()))
}

/// Units, plists and tasks are text.
fn text(path: &Path) -> Result<String> {
    match path.to_str() {
        Some(text) => Ok(text.to_owned()),
        None => bail!("{} isn't valid Unicode", path.display()),
    }
}

/// Runs a system tool and returns what it printed; its error output becomes
/// the error.
pub fn tool(program: &str, args: &[impl AsRef<OsStr>]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("can't run {program}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.trim();
        let said = if said.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        } else {
            said.to_owned()
        };
        bail!("{program} failed: {said}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The system-wide service runs its own copy of `ottie`, in a folder its
/// account can read (not someone's home). Done again by every install, so
/// an update reaches it.
pub fn copy_exe(exe: &Path, to: &Path) -> Result<()> {
    if same_file(exe, to) {
        return Ok(());
    }
    if let Some(folder) = to.parent() {
        std::fs::create_dir_all(folder)
            .with_context(|| format!("can't create {}", folder.display()))?;
    }
    // Copied beside it, then renamed over it: a running copy is never half
    // written.
    let mut next = to.as_os_str().to_owned();
    next.push(".new");
    let next = PathBuf::from(next);
    std::fs::copy(exe, &next).with_context(|| format!("can't copy ottie to {}", next.display()))?;
    std::fs::rename(&next, to).with_context(|| format!("can't replace {}", to.display()))?;
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `status`: one line per scope.
pub fn describe(scope: &str, installed: Option<&str>, running: Option<bool>) -> String {
    match (installed, running) {
        (None, _) => format!("{scope}: not installed"),
        (Some(what), None) => format!("{scope}: installed ({what})"),
        (Some(what), Some(true)) => format!("{scope}: installed ({what}), running"),
        (Some(what), Some(false)) => format!("{scope}: installed ({what}), not running"),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::*;

    pub fn install(_: &Install) -> Result<()> {
        unsupported()
    }

    pub fn uninstall(_: bool) -> Result<()> {
        unsupported()
    }

    pub fn status() -> Result<Vec<String>> {
        unsupported()
    }

    fn unsupported<T>() -> Result<T> {
        bail!(
            "`ottie service` isn't available on this system: run `ottie serve` from its own service manager"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines_say_what_is_known() {
        assert_eq!(describe("For you", None, None), "For you: not installed");
        assert_eq!(
            describe("System-wide", Some("Windows service"), Some(true)),
            "System-wide: installed (Windows service), running"
        );
        assert_eq!(
            describe("For you", Some("task at sign-in"), None),
            "For you: installed (task at sign-in)"
        );
    }

    #[test]
    fn a_relative_data_folder_gives_an_absolute_log() {
        let install = prepare(InstallArgs {
            dirs: DirArgs {
                config_dir: Some("state".into()),
                data_dir: Some("state".into()),
                system: false,
            },
            downloads_dir: None,
            at_boot: false,
        })
        .unwrap();
        assert!(install.log.is_absolute(), "{}", install.log.display());
        assert_eq!(
            install.log,
            absolute(Path::new("state")).unwrap().join("ottie.log")
        );
    }

    #[test]
    fn a_copy_lands_whole() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("ottie");
        std::fs::write(&exe, "new").unwrap();
        let to = dir.path().join("bin").join("ottie");
        copy_exe(&exe, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "new");
        // Itself: nothing to do.
        copy_exe(&to, &to).unwrap();
        assert!(!dir.path().join("bin").join("ottie.new").exists());
    }
}
