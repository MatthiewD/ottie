//! The app's own settings files, in its config folder: what belongs to this
//! computer's app rather than to the daemon (the player, the tray).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tauri::{AppHandle, Manager};

/// The settings saved in `file`; `None` if there are none or they can't be
/// read (then the file is kept as `<file>.bad`, and the defaults apply).
pub fn load<T: DeserializeOwned>(app: &AppHandle, file: &str) -> Option<T> {
    load_from(&path(app, file).ok()?)
}

pub fn save<T: Serialize>(app: &AppHandle, file: &str, value: &T) -> anyhow::Result<()> {
    save_to(&path(app, file)?, value)
}

fn path(app: &AppHandle, file: &str) -> anyhow::Result<PathBuf> {
    Ok(app.path().app_config_dir()?.join(file))
}

fn load_from<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            eprintln!("ottie-app: can't read {}: {err}", path.display());
            keep_aside(path);
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(err) => {
            eprintln!("ottie-app: {} is not valid: {err}", path.display());
            keep_aside(path);
            None
        }
    }
}

/// A file that can't be read is kept as `<name>.bad` before the defaults
/// replace it at the next save, so what it held isn't lost.
fn keep_aside(path: &Path) {
    let mut bad = path.as_os_str().to_owned();
    bad.push(".bad");
    // Best effort: without it, the defaults still apply.
    let _ = fs::copy(path, PathBuf::from(bad));
}

/// Written beside it, synced, then renamed over it: a crash or a full disk
/// never leaves it half written.
fn save_to<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder).context("can't create the app's config folder")?;
    }
    let text = serde_json::to_string_pretty(value)?;
    let mut next = path.as_os_str().to_owned();
    next.push(".new");
    let next = PathBuf::from(next);
    let written = (|| {
        let mut file = fs::File::create(&next)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file); // Windows won't rename a file that's still open.
        fs::rename(&next, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&next);
    }
    written.with_context(|| format!("can't save {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_settings_read_back_and_bad_ones_are_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("app.json");
        assert_eq!(load_from::<Vec<u32>>(&file), None, "none yet");

        save_to(&file, &vec![1, 2, 3]).unwrap();
        assert_eq!(load_from::<Vec<u32>>(&file), Some(vec![1, 2, 3]));
        assert!(!dir.path().join("app.json.new").exists());

        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(load_from::<Vec<u32>>(&file), None);
        let bad = dir.path().join("app.json.bad");
        assert_eq!(std::fs::read_to_string(bad).unwrap(), "{ not json");
    }
}
