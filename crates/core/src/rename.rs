//! Renaming a file without ever replacing another (docs/decisions/0049): a
//! finished download takes its name only if that name is still free at the
//! very moment of the rename, so a file that appeared meanwhile is never
//! lost. The system's own call where it has one, else a hard link, else
//! check then rename (with a warning: something could slip in between).

use std::io;
use std::path::Path;

/// Renames `from` to `to`, or fails with `AlreadyExists` if `to` exists,
/// leaving both as they were. Blocking: run it off the async threads.
pub(crate) fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    match os::rename_noreplace(from, to) {
        Err(err) if !settled(&err) => {}
        done => return done,
    }
    // Some network shares, Linux before 3.15, a long path on Windows.
    match by_link(from, to) {
        Err(err) if !settled(&err) => {}
        done => return done,
    }
    // No hard links either (FAT, exFAT, some network shares).
    tracing::warn!("this folder can't rename without replacing: checking the name first instead");
    check_then_rename(from, to)
}

/// An answer the next way wouldn't change: the name is taken, or the two
/// are on different drives (the caller copies). Any other failure may only
/// mean this way isn't available here.
fn settled(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::AlreadyExists | io::ErrorKind::CrossesDevices
    )
}

/// A hard link fails if its name exists; then the old name goes.
fn by_link(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::hard_link(from, to)?;
    if let Err(err) = std::fs::remove_file(from) {
        // The file has its name; the old one only takes up a name.
        tracing::warn!(error = %err, "can't remove a finished download's old name");
    }
    Ok(())
}

fn check_then_rename(from: &Path, to: &Path) -> io::Result<()> {
    if to.try_exists()? {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(from, to)
}

#[cfg(windows)]
mod os {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (wide(from), wide(to));
        // SAFETY: both are NUL-terminated UTF-16 strings that outlive the
        // call. No flags: a rename on the same drive that fails if `to`
        // exists (std's rename passes MOVEFILE_REPLACE_EXISTING).
        let done = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
        if done == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod os {
    use std::io;
    use std::path::Path;

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (super::c_path(from)?, super::c_path(to)?);
        // SAFETY: both are NUL-terminated paths that outlive the call;
        // AT_FDCWD reads them as rename does.
        let done = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE as _,
            )
        };
        if done != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::io;
    use std::path::Path;

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (super::c_path(from)?, super::c_path(to)?);
        // SAFETY: both are NUL-terminated paths that outlive the call.
        let done = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        if done != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos"
)))]
mod os {
    use std::io;
    use std::path::Path;

    pub fn rename_noreplace(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder with `part` holding "new" and `taken` holding "old".
    fn folder() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("a.zip.part.ottie");
        let taken = dir.path().join("a.zip");
        std::fs::write(&part, "new").unwrap();
        std::fs::write(&taken, "old").unwrap();
        (dir, part, taken)
    }

    /// Every way refuses a name that's taken and leaves both files as they
    /// were, then takes a free one.
    #[test]
    fn no_way_replaces_a_file() {
        type Rename = fn(&Path, &Path) -> io::Result<()>;
        let ways: [(&str, Rename); 4] = [
            ("the system's", os::rename_noreplace),
            ("a hard link", by_link),
            ("checking first", check_then_rename),
            ("all together", rename_new),
        ];
        for (way, rename) in ways {
            let (dir, part, taken) = folder();
            let err = rename(&part, &taken).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{way}");
            assert_eq!(std::fs::read_to_string(&taken).unwrap(), "old", "{way}");
            assert_eq!(std::fs::read_to_string(&part).unwrap(), "new", "{way}");

            let free = dir.path().join("a (1).zip");
            rename(&part, &free).unwrap();
            assert_eq!(std::fs::read_to_string(&free).unwrap(), "new", "{way}");
            assert!(!part.exists(), "{way}");
        }
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = rename_new(&dir.path().join("gone"), &dir.path().join("a.zip")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
