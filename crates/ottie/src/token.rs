//! The API token: 32 random bytes as hex, in `<config folder>/token`,
//! created the first time it's needed.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use clap::Args;

use crate::config::{DirArgs, Dirs};

const TOKEN_FILE: &str = "token";

pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join(TOKEN_FILE)
}

#[derive(Args, Debug)]
pub struct TokenArgs {
    #[command(flatten)]
    dirs: DirArgs,
    /// Replace it with a new one: browsers and the desktop app signed in with
    /// the old one must sign in again. Devices keep their own tokens.
    #[arg(long)]
    new: bool,
}

/// `ottie token`: prints it (creating it if this is the first run), or
/// replaces it. A running Ottie notices the new one at its next request.
pub fn print(args: &TokenArgs) -> Result<()> {
    let dirs = Dirs::resolve(&args.dirs)?;
    if !args.new {
        println!("{}", for_client(&dirs)?);
        return Ok(());
    }
    let path = path(&dirs.config);
    if dirs.system {
        // Rewritten in place, so it stays the service account's.
        for_client(&dirs)?;
    } else {
        fs::create_dir_all(&dirs.config)
            .with_context(|| format!("can't create {}", dirs.config.display()))?;
    }
    let token = generate()?;
    let contents = format!("{token}\n");
    let written = if dirs.system {
        write_private(&path, &contents, true)
    } else {
        replace_private(&path, &contents)
    };
    written.with_context(|| format!("can't write {}", path.display()))?;
    println!("{token}");
    eprintln!(
        "The old token no longer works: sign in again with `ottie open` or `ottie link`, \
         and restart the desktop app. Devices keep their own tokens."
    );
    Ok(())
}

/// The token for `token`, `link` and `open`. The system-wide service's is
/// only read: one created by root or an administrator here would be theirs,
/// and the service's account couldn't read it.
pub fn for_client(dirs: &Dirs) -> Result<String> {
    if !dirs.system {
        return load_or_create(&dirs.config);
    }
    let path = path(&dirs.config);
    match read(&path) {
        Ok(Some(token)) => Ok(token),
        Ok(None) => bail!(
            "{} doesn't exist yet: the system-wide service creates it when it first starts",
            path.display()
        ),
        Err(err) => Err(err.context(if cfg!(windows) {
            "only administrators can read it"
        } else {
            "only root and the ottie account can read it (sudo?)"
        })),
    }
}

pub fn load_or_create(config_dir: &Path) -> Result<String> {
    let path = path(config_dir);
    if let Some(token) = read(&path)? {
        return Ok(token);
    }
    fs::create_dir_all(config_dir)
        .with_context(|| format!("can't create {}", config_dir.display()))?;
    let token = generate()?;
    match create_private(&path, &format!("{token}\n")) {
        Ok(()) => Ok(token),
        // Another `ottie` created it at the same moment: use that one.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            read(&path)?.ok_or_else(|| anyhow!("{} is empty", path.display()))
        }
        Err(err) => Err(err).with_context(|| format!("can't write {}", path.display())),
    }
}

/// `None` if the file doesn't exist yet.
fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let token = text.trim();
            if token.is_empty() {
                bail!("{} is empty; delete it to get a new token", path.display());
            }
            Ok(Some(token.to_owned()))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("can't read {}", path.display())),
    }
}

fn generate() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| anyhow!("no random numbers available: {err}"))?;
    let mut hex = String::with_capacity(64);
    for byte in bytes {
        // Writing to a String can't fail.
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Creates `path` (owner-only) with all of `contents` at once: written
/// beside it, then linked into place, so a reader never finds it empty.
/// `AlreadyExists` if it's there. Without hard links (some network folders),
/// as `write_private` does.
fn create_private(path: &Path, contents: &str) -> io::Result<()> {
    let beside = beside(path);
    write_private(&beside, contents, true)?;
    let linked = fs::hard_link(&beside, path);
    let _ = fs::remove_file(&beside);
    match linked {
        Err(err) if err.kind() != io::ErrorKind::AlreadyExists => {
            write_private(path, contents, false)
        }
        linked => linked,
    }
}

/// Replaces `path` (owner-only) with `contents` at once: written beside it,
/// then renamed over it, so a reader finds the old or the new, never empty.
fn replace_private(path: &Path, contents: &str) -> io::Result<()> {
    let beside = beside(path);
    write_private(&beside, contents, true)?;
    fs::rename(&beside, path).inspect_err(|_| {
        let _ = fs::remove_file(&beside);
    })
}

/// A file of this process's own beside `path`.
fn beside(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.new", std::process::id()));
    PathBuf::from(name)
}

/// A file only the owner can read on Unix. (On Windows the config folder,
/// %APPDATA%, is already private to the user.) `replace` overwrites an
/// existing file; otherwise one that exists is an `AlreadyExists` error.
pub fn write_private(path: &Path, contents: &str, replace: bool) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true);
    if replace {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_once_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("new folder");
        let first = load_or_create(&config).unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(load_or_create(&config).unwrap(), first);
        assert_ne!(load_or_create(dir.path()).unwrap(), first, "random");
    }

    #[test]
    fn a_token_file_appears_whole() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(dir.path());
        create_private(&file, "first\n").unwrap();
        let again = create_private(&file, "second\n").unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&file).unwrap(), "first\n");
        replace_private(&file, "third\n").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "third\n");
        // Nothing left beside it.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn an_empty_token_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(path(dir.path()), "\n").unwrap();
        assert!(load_or_create(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_owner_can_read_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        load_or_create(dir.path()).unwrap();
        let mode = fs::metadata(path(dir.path())).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
