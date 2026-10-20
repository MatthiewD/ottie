//! The daemon this window shows: the one already running on this computer,
//! or `ottie serve --managed` started for the app (docs/decisions/0017).
//! At sign-in it can also be started on its own (docs/decisions/0039).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tauri::Url;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, ChildStdin, Command};

use crate::client;
use crate::identity::{self, Found};

/// A first start creates the database; a large one takes a moment to load.
const STARTUP: Duration = Duration::from_secs(30);
/// Stopping saves every running download first.
const SHUTDOWN: Duration = Duration::from_secs(10);
/// The end of the daemon's log, kept to explain a failed start.
const KEPT_LINES: usize = 20;

/// Windows: start `ottie` without a console window of its own.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct Connection {
    /// The web UI, signed in: `http://127.0.0.1:24068/#token=…`.
    pub link: Url,
    /// `None` when a daemon was already running: not ours to stop.
    pub started: Option<Started>,
}

/// `ottie` sits next to the app: put there by the installer (Tauri's
/// `externalBin`), or built into the same `target/` folder in development.
pub fn sidecar() -> Result<PathBuf> {
    let app = std::env::current_exe().context("can't find where the app is")?;
    let ottie = app.with_file_name(format!("ottie{}", std::env::consts::EXE_SUFFIX));
    if !ottie.is_file() {
        bail!(
            "{} is missing (in development: cargo build -p ottie)",
            ottie.display()
        );
    }
    Ok(ottie)
}

/// Finds or starts the daemon `link` leads to (`link`'s).
pub async fn connect(ottie: &Path, link: Url) -> Result<Connection> {
    // Starting first, not probing first: on Windows a connection to a port
    // nobody listens on takes 2 s to fail. `ottie serve` checks the port
    // before anything else, so a second one gives up at once.
    let failure = match Started::spawn(ottie).await {
        Ok(started) => {
            return Ok(Connection {
                link,
                started: Some(started),
            });
        }
        Err(failure) => failure,
    };
    // Usually: Ottie was already running (from a terminal, or as a service).
    match found(&link).await {
        Found::Ours => Ok(Connection {
            link,
            started: None,
        }),
        Found::Other => Err(not_ours(&link)),
        Found::Nothing => Err(failure),
    }
}

/// Starts `ottie serve` on its own, not tied to the app (which then quits),
/// its log in `log`; done once it answers. Fine if one was running already.
pub async fn start_alone(ottie: &Path, log: &Path) -> Result<()> {
    let link = link(ottie).await?;
    if let Some(folder) = log.parent() {
        std::fs::create_dir_all(folder)
            .with_context(|| format!("can't create {}", folder.display()))?;
    }
    let file =
        std::fs::File::create(log).with_context(|| format!("can't create {}", log.display()))?;
    // Not killed on drop: it outlives the app.
    let mut child = command(ottie)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(file)
        .spawn()
        .with_context(|| format!("can't start {}", ottie.display()))?;
    let waited = tokio::time::timeout(STARTUP, async {
        loop {
            if found(&link).await == Found::Ours {
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                match found(&link).await {
                    // Another Ottie took the port while we looked: that one serves.
                    Found::Ours => return Ok(()),
                    Found::Other => return Err(not_ours(&link)),
                    Found::Nothing => {}
                }
                let text = std::fs::read_to_string(log).unwrap_or_default();
                bail!(reason(text.lines(), status));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    waited
        .await
        .unwrap_or_else(|_| bail!("Ottie didn't start within {} s", STARTUP.as_secs()))
}

/// `ottie link`: the address and token, as `ottie serve` will use them.
pub async fn link(ottie: &Path) -> Result<Url> {
    let output = command(ottie)
        .arg("link")
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("can't run {}", ottie.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(reason(stderr.lines(), output.status));
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    Url::parse(printed.trim()).with_context(|| format!("`ottie link` printed {printed:?}"))
}

/// Tells the browsers found where the extension's native messaging host is
/// (`ottie extension install`, docs/decisions/0055), so the extension signs
/// in by itself. It writes only what changed: nothing, after the first start
/// (security tools frown on an app that rewrites the registry each time).
pub async fn set_up_extension(ottie: PathBuf) {
    let ran = command(&ottie)
        .args(["extension", "install", "--quiet"])
        .stdin(Stdio::null())
        .output()
        .await;
    match ran {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!(
                "browser extension: {}",
                reason(stderr.lines(), output.status)
            );
        }
        Err(err) => eprintln!("browser extension: can't run {}: {err}", ottie.display()),
    }
}

/// What answers at the link's address: this computer's Ottie only if it
/// proves it knows the token, which is never sent to find out
/// (docs/decisions/0058).
async fn found(link: &Url) -> Found {
    identity::check(link, client::token(link)).await
}

/// Something holds Ottie's port but isn't this computer's Ottie.
fn not_ours(link: &Url) -> anyhow::Error {
    let at = link.origin().ascii_serialization();
    anyhow!(
        "what answers at {at} isn't your Ottie: it can't prove it knows this computer's token, so the token wasn't sent.          It may be Ottie started with other folders, an Ottie older than this app (restart it), or another program on that port"
    )
}

fn command(ottie: &Path) -> Command {
    let mut command = Command::new(ottie);
    // Its log lines without colour codes.
    command.env("NO_COLOR", "1");
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// A daemon this app started. Closing its stdin tells it to stop; if the app
/// crashes, the OS closes it, so the daemon never outlives the app.
pub struct Started {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Started {
    async fn spawn(ottie: &Path) -> Result<Started> {
        let mut child = command(ottie)
            .args(["serve", "--managed"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If we give up on it below.
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("can't start {}", ottie.display()))?;
        let stdin = child.stdin.take();
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            bail!("no pipes to ottie serve");
        };
        let log = tauri::async_runtime::spawn(forward_log(stderr));

        let mut stdout = BufReader::new(stdout).lines();
        match tokio::time::timeout(STARTUP, stdout.next_line()).await {
            Ok(Ok(Some(line))) if line == "ready" => {
                // Keep reading: a daemon that writes to a closed pipe fails.
                tauri::async_runtime::spawn(async move {
                    while let Ok(Some(_)) = stdout.next_line().await {}
                });
                Ok(Started { child, stdin })
            }
            Ok(_) => {
                // It stopped: its last log lines say why.
                let status = child.wait().await?;
                let lines = log.await.unwrap_or_default();
                Err(anyhow!(reason(lines.iter().map(String::as_str), status)))
            }
            Err(_) => bail!("Ottie didn't start within {} s", STARTUP.as_secs()),
        }
    }

    /// Asks it to stop, which saves its downloads; kills it if that takes too
    /// long (the next start resumes from the files anyway).
    pub async fn stop(mut self) {
        drop(self.stdin.take());
        if tokio::time::timeout(SHUTDOWN, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

/// The daemon's log goes on to the app's own stderr (seen in development);
/// the last lines are returned once it closes.
async fn forward_log(stderr: impl AsyncRead + Unpin) -> VecDeque<String> {
    let mut kept = VecDeque::with_capacity(KEPT_LINES);
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        eprintln!("{line}");
        if kept.len() == KEPT_LINES {
            kept.pop_front();
        }
        kept.push_back(line);
    }
    kept
}

/// Why `ottie` stopped: the error it printed last (`ottie: …`), else its exit status.
fn reason<'a>(stderr: impl DoubleEndedIterator<Item = &'a str>, status: ExitStatus) -> String {
    stderr
        .rev()
        .find_map(|line| line.strip_prefix("ottie: "))
        .map_or_else(|| format!("Ottie stopped ({status})"), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reason_is_the_last_error_line() {
        let failed = failure_status();
        let log = "INFO starting\nottie: first\nottie: can't listen on 127.0.0.1:24068\nmore";
        assert_eq!(
            reason(log.lines(), failed),
            "can't listen on 127.0.0.1:24068"
        );
        assert!(reason("INFO only logs".lines(), failed).starts_with("Ottie stopped ("));
    }

    /// Exit code 1 on Unix (code × 256), 256 on Windows: a failure either way.
    fn failure_status() -> ExitStatus {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(256)
    }
}
