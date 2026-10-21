//! Choosing safe, collision-free file names inside the target folder.

use std::io;
use std::path::Path;
use std::sync::Arc;

use percent_encoding::percent_decode_str;
use reqwest::Url;
use tokio::fs::{self, File, OpenOptions};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio_util::sync::CancellationToken;

use crate::rename;

const FALLBACK_NAME: &str = "download";
/// Leaves room for a " (999)" suffix and `PART_SUFFIX` within common 255-byte limits.
const MAX_NAME_BYTES: usize = 200;
const MAX_EXTENSION_BYTES: usize = 16;
const MAX_COLLISION_ATTEMPTS: u32 = 1000;
const WINDOWS_RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// File name suggested by the last path segment of `url`, sanitized.
pub fn from_url(url: &Url) -> String {
    let last = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("");
    sanitize(&percent_decode_str(last).decode_utf8_lossy())
}

/// The file name a `Content-Disposition` header suggests, sanitized, or
/// `None` if it names none. Prefers `filename*` (RFC 8187, e.g.
/// `UTF-8''caf%C3%A9.txt`) over `filename`. A plain `filename` is read as
/// UTF-8 when it is valid UTF-8 (many servers send that), else as Latin-1.
pub fn from_content_disposition(value: &[u8]) -> Option<String> {
    let text = match std::str::from_utf8(value) {
        Ok(text) => text.to_owned(),
        Err(_) => value.iter().map(|&b| char::from(b)).collect(),
    };
    let mut plain = None;
    let mut extended = None;
    for (key, value) in parameters(&text) {
        if key.eq_ignore_ascii_case("filename*") {
            extended = extended.or_else(|| decode_ext_value(&value));
        } else if key.eq_ignore_ascii_case("filename") {
            plain = plain.or(Some(value));
        }
    }
    let raw = extended.or(plain)?;
    (!raw.trim().is_empty()).then(|| sanitize(&raw))
}

/// The `key=value` pairs after the disposition type (`attachment; …`).
/// Values may be quoted, with `\` escapes.
fn parameters(header: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut rest = match header.split_once(';') {
        Some((_, rest)) => rest,
        None => return pairs,
    };
    loop {
        rest = rest.trim_start_matches([' ', '\t', ';']);
        if rest.is_empty() {
            return pairs;
        }
        let key_end = rest.find(['=', ';']).unwrap_or(rest.len());
        let key = rest[..key_end].trim().to_owned();
        rest = &rest[key_end..];
        let Some(after_eq) = rest.strip_prefix('=') else {
            continue; // A key without a value: skip it.
        };
        let after_eq = after_eq.trim_start();
        let (value, after) = match after_eq.strip_prefix('"') {
            Some(quoted) => unquote(quoted),
            None => {
                let end = after_eq.find(';').unwrap_or(after_eq.len());
                (after_eq[..end].trim().to_owned(), &after_eq[end..])
            }
        };
        pairs.push((key, value));
        rest = after;
    }
}

/// Reads a quoted string whose opening `"` is already consumed. Returns its
/// text and what follows the closing `"` (nothing, if it's missing).
fn unquote(quoted: &str) -> (String, &str) {
    let mut text = String::new();
    let mut chars = quoted.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => text.extend(chars.next().map(|(_, escaped)| escaped)),
            '"' => return (text, &quoted[i + 1..]),
            c => text.push(c),
        }
    }
    (text, "")
}

/// Decodes an RFC 8187 value: `charset'language'percent-encoded`. Only
/// UTF-8 and ISO-8859-1 are required by the standard; others give `None`.
fn decode_ext_value(value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '\'');
    let (charset, _language, encoded) = (parts.next()?, parts.next()?, parts.next()?);
    let bytes: Vec<u8> = percent_decode_str(encoded).collect();
    if charset.eq_ignore_ascii_case("utf-8") {
        String::from_utf8(bytes).ok()
    } else if charset.eq_ignore_ascii_case("iso-8859-1") {
        Some(bytes.iter().map(|&b| char::from(b)).collect())
    } else {
        None
    }
}

/// The extension of a file name, lowercase and without the dot, if it has
/// one (`.hidden` has none).
pub fn extension(name: &str) -> Option<String> {
    match split_extension(name) {
        (_, Some(ext)) if !ext.is_empty() => Some(ext.to_lowercase()),
        _ => None,
    }
}

/// Turns untrusted text into a plain file name: no path separators, no
/// characters Windows rejects, no leading/trailing dots or spaces, no reserved
/// device names. The result can always be joined to a folder without escaping it.
pub fn sanitize(raw: &str) -> String {
    let replaced: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let truncated = truncate(&replaced);
    let trimmed = truncated.trim_matches(|c| c == ' ' || c == '.');
    if trimmed.is_empty() {
        return FALLBACK_NAME.to_owned();
    }
    let stem = trimmed.split('.').next().unwrap_or(trimmed).trim_end();
    if WINDOWS_RESERVED
        .iter()
        .any(|r| r.eq_ignore_ascii_case(stem))
    {
        return format!("_{trimmed}");
    }
    trimmed.to_owned()
}

/// Appended to the final name while downloading. The OS sees `.ottie` as the
/// extension, so every Ottie file shares one extension (see docs/decisions/0007).
pub const PART_SUFFIX: &str = ".part.ottie";

pub fn part_name(name: &str) -> String {
    format!("{name}{PART_SUFFIX}")
}

/// What to do when a file with a download's name is already in its folder
/// (docs/decisions/0049).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Existing {
    /// Save it under the first free name: `name (1)`, `name (2)`…
    #[default]
    Rename,
    /// Download it beside the file, which it replaces once complete.
    Replace,
    /// Stop before downloading anything, so someone decides.
    Ask,
}

impl Existing {
    pub fn as_str(self) -> &'static str {
        match self {
            Existing::Rename => "rename",
            Existing::Replace => "replace",
            Existing::Ask => "ask",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "rename" => Some(Existing::Rename),
            "replace" => Some(Existing::Replace),
            "ask" => Some(Existing::Ask),
            _ => None,
        }
    }
}

/// Whether a file (or folder) named `name` is in `dir`.
pub async fn exists(dir: &Path, name: &str) -> io::Result<bool> {
    fs::try_exists(dir.join(name)).await
}

/// Picks the first free name among `name`, `name (1)`, `name (2)`… and creates
/// its `.part.ottie` file. `create_new` makes the reservation atomic, so two
/// downloads can't grab the same name. With `replace`, a finished file
/// named `name` doesn't count: the download will replace it (but another
/// download's `.part.ottie` still does).
pub async fn reserve(dir: &Path, name: &str, replace: bool) -> io::Result<(String, File)> {
    // A category's folder may not exist yet.
    fs::create_dir_all(dir).await?;
    for n in 0..MAX_COLLISION_ATTEMPTS {
        let candidate = numbered(name, n);
        if !(replace && n == 0) && fs::try_exists(dir.join(&candidate)).await? {
            continue;
        }
        let part = dir.join(part_name(&candidate));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .await
        {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(no_free_name())
}

/// Renames the `.part.ottie` file to the first free name. Normally that's `name`,
/// unless a file with that name appeared while we were downloading. Each
/// name is taken only if it's free at the moment of the rename, so a file
/// appearing at the last moment is never replaced (`rename::rename_new`).
/// With `replace`, it takes `name` whatever is there (the file is replaced).
pub async fn finalize(dir: &Path, name: &str, part: &Path, replace: bool) -> io::Result<String> {
    if replace {
        fs::rename(part, dir.join(name)).await?;
        return Ok(name.to_owned());
    }
    for n in 0..MAX_COLLISION_ATTEMPTS {
        let candidate = numbered(name, n);
        match rename_new(part, &dir.join(&candidate)).await {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(no_free_name())
}

/// [`rename::rename_new`], off the async threads (as `tokio::fs` does).
async fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (from.to_owned(), to.to_owned());
    tokio::task::spawn_blocking(move || rename::rename_new(&from, &to))
        .await
        .map_err(io::Error::other)?
}

/// Moves a download's file from folder `from` to folder `to`, under the first
/// free name there: the finished file (`finished`) or its `.part.ottie`.
/// Returns its name now. A file that's gone (deleted by hand) has nothing to
/// move: the name stays. [`reserve_in`], then [`move_to`]; the manager saves
/// where it's going in between (docs/decisions/0059), so it uses those.
#[cfg(test)]
async fn relocate(from: &Path, name: &str, finished: bool, to: &Path) -> io::Result<String> {
    if !fs::try_exists(source_of(from, name, finished)).await? {
        return Ok(name.to_owned());
    }
    let candidate = reserve_in(to, name).await?;
    move_to(from, name, finished, to, &candidate, &Copies::new(1))
        .await
        .map_err(MoveFailure::into_error)
}

/// Where a download's file is now: the finished file, or its `.part.ottie`.
pub(crate) fn source_of(from: &Path, name: &str, finished: bool) -> std::path::PathBuf {
    if finished {
        from.join(name)
    } else {
        from.join(part_name(name))
    }
}

/// Reserves the first free name in `to` for a file moving there, as a
/// `.part.ottie` the file then replaces. That name.
pub(crate) async fn reserve_in(to: &Path, name: &str) -> io::Result<String> {
    let (candidate, file) = reserve(to, name, false).await?;
    drop(file); // Windows won't replace a file that's still open.
    Ok(candidate)
}

/// How a move failed.
#[derive(Debug)]
pub(crate) enum MoveFailure {
    /// The file is where it was; the reservation is gone.
    Undone(io::Error),
    /// The file couldn't be put back: it's at the destination, under its
    /// reserved name as a `.part.ottie`.
    Stranded(io::Error),
}

impl MoveFailure {
    pub(crate) fn into_error(self) -> io::Error {
        match self {
            MoveFailure::Undone(err) | MoveFailure::Stranded(err) => err,
        }
    }
}

/// Copies between drives (moves there), a few at a time: more at once only
/// share the disks (a seek each, on a hard disk), and every file arrives
/// later. A rename on one drive never waits for them.
#[derive(Clone)]
pub(crate) struct Copies {
    turns: Arc<Semaphore>,
    /// Ottie is stopping: a copy that hasn't begun doesn't.
    stopping: CancellationToken,
}

impl Copies {
    pub(crate) fn new(at_once: usize) -> Self {
        Copies {
            turns: Arc::new(Semaphore::new(at_once)),
            stopping: CancellationToken::new(),
        }
    }

    /// Copies waiting for their turn give up; those under way finish.
    pub(crate) fn stop(&self) {
        self.stopping.cancel();
    }

    /// Waits for a copy's turn. Unless it puts a file back (`may_stop`
    /// false), it's refused once Ottie is stopping.
    async fn turn(&self, may_stop: bool) -> io::Result<SemaphorePermit<'_>> {
        let stopping = async {
            if may_stop {
                self.stopping.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            () = stopping => Err(io::Error::other("Ottie is stopping")),
            // Never closed.
            turn = self.turns.acquire() => turn.map_err(io::Error::other),
        }
    }
}

/// Moves the file from `from` to `to`, under `candidate` (from
/// [`reserve_in`]): its name there. If anything fails, the original is put
/// back where it was, unless even that fails.
pub(crate) async fn move_to(
    from: &Path,
    name: &str,
    finished: bool,
    to: &Path,
    candidate: &str,
    copies: &Copies,
) -> Result<String, MoveFailure> {
    let source = source_of(from, name, finished);
    let part = to.join(part_name(candidate));
    if let Err(err) = move_file(&source, &part, copies, true).await {
        // A copy between drives leaves the original until it's complete.
        let _ = fs::remove_file(&part).await;
        return Err(MoveFailure::Undone(err));
    }
    if !finished {
        return Ok(candidate.to_owned());
    }
    match finalize(to, candidate, &part, false).await {
        Ok(name) => Ok(name),
        // Put it back rather than leave it half-moved.
        Err(err) => match move_file(&part, &source, copies, false).await {
            Ok(()) => Err(MoveFailure::Undone(err)),
            Err(back) => {
                tracing::error!(error = %back, "a moved file couldn't be put back");
                Err(MoveFailure::Stranded(err))
            }
        },
    }
}

/// A move a crash interrupted, settled at the next start: `None` if the file
/// is still where it was (the reservation, or a copy begun, goes), else its
/// name in `to` (a finished file under its `.part.ottie` name gets its own).
pub(crate) async fn settle_move(
    from: &Path,
    name: &str,
    finished: bool,
    to: &Path,
    candidate: &str,
) -> io::Result<Option<String>> {
    let part = to.join(part_name(candidate));
    if fs::try_exists(source_of(from, name, finished)).await? {
        match fs::remove_file(&part).await {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => return Ok(None),
        }
    }
    if finished && fs::try_exists(&part).await? {
        return finalize(to, candidate, &part, false).await.map(Some);
    }
    Ok(Some(candidate.to_owned()))
}

/// Renames `source` to `target`, replacing it. Between drives a rename
/// can't work, so it copies (in its turn, see [`Copies::turn`]), then
/// deletes the original.
async fn move_file(
    source: &Path,
    target: &Path,
    copies: &Copies,
    may_stop: bool,
) -> io::Result<()> {
    match fs::rename(source, target).await {
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            let _turn = copies.turn(may_stop).await?;
            copy_then_delete(source, target).await
        }
        other => other,
    }
}

/// The original goes only once the copy is complete and on the disk
/// (synced, and its folder too on Unix), so a failure, a crash or a power
/// cut halfway leaves it intact.
async fn copy_then_delete(source: &Path, target: &Path) -> io::Result<()> {
    fs::copy(source, target).await?;
    // Write access: Windows syncs only a file open for writing.
    OpenOptions::new()
        .write(true)
        .open(target)
        .await?
        .sync_all()
        .await?;
    #[cfg(unix)]
    if let Some(folder) = target.parent() {
        // Its entry in the folder, so the copy can be found after a crash.
        File::open(folder).await?.sync_all().await?;
    }
    fs::remove_file(source).await
}

fn no_free_name() -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, "no free file name")
}

fn numbered(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    match split_extension(name) {
        (stem, Some(ext)) => format!("{stem} ({n}).{ext}"),
        (stem, None) => format!("{stem} ({n})"),
    }
}

fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], Some(&name[i + 1..])),
        _ => (name, None),
    }
}

/// Shortens long names to `MAX_NAME_BYTES`, keeping a short extension.
fn truncate(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_owned();
    }
    let (stem, ext) = match split_extension(name) {
        (stem, Some(ext)) if ext.len() <= MAX_EXTENSION_BYTES => (stem, Some(ext)),
        _ => (name, None),
    };
    let budget = MAX_NAME_BYTES - ext.map_or(0, |e| e.len() + 1);
    let mut cut = budget.min(stem.len());
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    match ext {
        Some(ext) => format!("{}.{ext}", &stem[..cut]),
        None => stem[..cut].to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn name_comes_from_last_path_segment() {
        let url = Url::parse("https://example.com/a/b/My%20File.zip?token=x").unwrap();
        assert_eq!(from_url(&url), "My File.zip");
    }

    #[test]
    fn empty_path_falls_back() {
        let url = Url::parse("https://example.com/").unwrap();
        assert_eq!(from_url(&url), "download");
    }

    #[test]
    fn encoded_separators_cannot_escape_the_folder() {
        let url = Url::parse("https://example.com/..%2F..%2Fetc%2Fpasswd").unwrap();
        assert_eq!(from_url(&url), "_.._etc_passwd");
        assert_eq!(sanitize(".."), "download");
        assert_eq!(sanitize("..\\..\\x"), "_.._x");
    }

    fn disposition(value: &str) -> Option<String> {
        from_content_disposition(value.as_bytes())
    }

    #[test]
    fn content_disposition_names() {
        assert_eq!(
            disposition("attachment; filename=\"report 2026.pdf\""),
            Some("report 2026.pdf".into())
        );
        assert_eq!(
            disposition("attachment;filename=plain.txt;size=3"),
            Some("plain.txt".into())
        );
        assert_eq!(
            disposition(r#"inline; filename="a \"quoted\" name.txt""#),
            Some("a _quoted_ name.txt".into())
        );
        assert_eq!(
            disposition("attachment; FILENAME=\"Upper.TXT\""),
            Some("Upper.TXT".into())
        );
        assert_eq!(disposition("attachment"), None);
        assert_eq!(disposition("attachment; filename=\"\""), None);
        assert_eq!(disposition("attachment; size=3"), None);
    }

    #[test]
    fn content_disposition_prefers_the_encoded_name() {
        let both = "attachment; filename=\"cafe.txt\"; filename*=UTF-8''caf%C3%A9%20menu.txt";
        assert_eq!(disposition(both), Some("café menu.txt".into()));
        let first = "attachment; filename*=utf-8'en'%E2%82%AC.txt; filename=\"EUR.txt\"";
        assert_eq!(disposition(first), Some("€.txt".into()));
        let latin1 = "attachment; filename*=ISO-8859-1''caf%E9.txt";
        assert_eq!(disposition(latin1), Some("café.txt".into()));
        // An unknown charset or broken UTF-8 falls back to `filename`.
        let unknown = "attachment; filename*=KOI8-R''%C1.txt; filename=\"x.txt\"";
        assert_eq!(disposition(unknown), Some("x.txt".into()));
        let broken = "attachment; filename*=UTF-8''%FF.txt; filename=\"y.txt\"";
        assert_eq!(disposition(broken), Some("y.txt".into()));
    }

    #[test]
    fn content_disposition_raw_bytes() {
        let utf8 = "attachment; filename=\"résumé.pdf\"";
        assert_eq!(disposition(utf8), Some("résumé.pdf".into()));
        let latin1 = b"attachment; filename=\"r\xe9sum\xe9.pdf\"";
        assert_eq!(from_content_disposition(latin1), Some("résumé.pdf".into()));
    }

    #[test]
    fn content_disposition_cannot_escape_the_folder() {
        let evil = "attachment; filename=\"../../etc/passwd\"";
        assert_eq!(disposition(evil), Some("_.._etc_passwd".into()));
        let encoded = "attachment; filename*=UTF-8''..%2F..%5Cboot.ini";
        assert_eq!(disposition(encoded), Some("_.._boot.ini".into()));
    }

    #[test]
    fn extensions() {
        assert_eq!(extension("Film.MKV"), Some("mkv".into()));
        assert_eq!(extension("archive.tar.gz"), Some("gz".into()));
        assert_eq!(extension(".hidden"), None);
        assert_eq!(extension("README"), None);
        assert_eq!(extension("dot."), None);
    }

    #[test]
    fn windows_rules() {
        assert_eq!(sanitize("a<b>c:d\"e|f?g*h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize("name. . "), "name");
        assert_eq!(sanitize("con.txt"), "_con.txt");
        assert_eq!(sanitize("console.txt"), "console.txt");
        assert_eq!(sanitize("tab\there"), "tab_here");
    }

    #[test]
    fn long_names_keep_their_extension() {
        let long = format!("{}.tar", "é".repeat(150)); // 300 bytes of stem
        let out = sanitize(&long);
        assert!(out.len() <= MAX_NAME_BYTES);
        assert!(out.ends_with(".tar"));
    }

    #[test]
    fn partial_files_end_in_ottie() {
        assert_eq!(part_name("video.mp4"), "video.mp4.part.ottie");
        let longest = format!("{} (999){PART_SUFFIX}", "x".repeat(MAX_NAME_BYTES));
        assert!(longest.len() <= 255, "names must fit common file systems");
    }

    #[test]
    fn collision_suffix_goes_before_extension() {
        assert_eq!(numbered("file.zip", 0), "file.zip");
        assert_eq!(numbered("file.zip", 2), "file (2).zip");
        assert_eq!(numbered("README", 1), "README (1)");
        assert_eq!(numbered(".hidden", 1), ".hidden (1)");
    }

    #[tokio::test]
    async fn an_unfinished_download_moves_its_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("film.mkv.part.ottie"), b"half").unwrap();
        let name = relocate(&from, "film.mkv", false, &to).await.unwrap();
        assert_eq!(name, "film.mkv");
        assert_eq!(
            std::fs::read(to.join("film.mkv.part.ottie")).unwrap(),
            b"half"
        );
        assert!(!from.join("film.mkv.part.ottie").exists());
    }

    #[tokio::test]
    async fn a_finished_file_takes_a_free_name() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("song.mp3"), b"new").unwrap();
        std::fs::write(to.join("song.mp3"), b"old").unwrap();
        let name = relocate(&from, "song.mp3", true, &to).await.unwrap();
        assert_eq!(name, "song (1).mp3");
        assert_eq!(std::fs::read(to.join("song (1).mp3")).unwrap(), b"new");
        assert_eq!(std::fs::read(to.join("song.mp3")).unwrap(), b"old");
        assert!(!to.join("song (1).mp3.part.ottie").exists());
        assert!(!from.join("song.mp3").exists());
    }

    #[tokio::test]
    async fn replacing_reserves_the_name_and_takes_it_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("song.mp3"), b"old").unwrap();
        let (name, file) = reserve(dir.path(), "song.mp3", true).await.unwrap();
        drop(file);
        assert_eq!(name, "song.mp3");
        // The old file stays until the new one is complete.
        assert_eq!(std::fs::read(dir.path().join("song.mp3")).unwrap(), b"old");
        let part = dir.path().join("song.mp3.part.ottie");
        std::fs::write(&part, b"new").unwrap();
        let name = finalize(dir.path(), "song.mp3", &part, true).await.unwrap();
        assert_eq!(name, "song.mp3");
        assert_eq!(std::fs::read(dir.path().join("song.mp3")).unwrap(), b"new");
        assert!(!part.exists());
    }

    /// Two folders, `from` holding `song.mp3` (or its `.part.ottie`).
    fn two_folders(finished: bool) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("from"), dir.path().join("to"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(source_of(&from, "song.mp3", finished), b"song").unwrap();
        (dir, from, to)
    }

    #[tokio::test]
    async fn a_move_that_hadnt_happened_is_undone() {
        let (_dir, from, to) = two_folders(true);
        // Reserved, and a copy begun, when it stopped.
        std::fs::write(to.join("song.mp3.part.ottie"), b"so").unwrap();
        let settled = settle_move(&from, "song.mp3", true, &to, "song.mp3")
            .await
            .unwrap();
        assert_eq!(settled, None);
        assert_eq!(std::fs::read(from.join("song.mp3")).unwrap(), b"song");
        assert!(!to.join("song.mp3.part.ottie").exists());
    }

    #[tokio::test]
    async fn a_move_that_happened_is_finished() {
        // A finished file still under its reserved `.part.ottie` name.
        let (_dir, from, to) = two_folders(true);
        std::fs::rename(from.join("song.mp3"), to.join("song.mp3.part.ottie")).unwrap();
        let settled = settle_move(&from, "song.mp3", true, &to, "song.mp3")
            .await
            .unwrap();
        assert_eq!(settled.as_deref(), Some("song.mp3"));
        assert_eq!(std::fs::read(to.join("song.mp3")).unwrap(), b"song");
        assert!(!to.join("song.mp3.part.ottie").exists());

        // Already under its name.
        let (_dir, from, to) = two_folders(true);
        std::fs::rename(from.join("song.mp3"), to.join("song (1).mp3")).unwrap();
        let settled = settle_move(&from, "song.mp3", true, &to, "song (1).mp3")
            .await
            .unwrap();
        assert_eq!(settled.as_deref(), Some("song (1).mp3"));

        // Unfinished: its `.part.ottie` is the file.
        let (_dir, from, to) = two_folders(false);
        std::fs::rename(
            from.join("song.mp3.part.ottie"),
            to.join("song.mp3.part.ottie"),
        )
        .unwrap();
        let settled = settle_move(&from, "song.mp3", false, &to, "song.mp3")
            .await
            .unwrap();
        assert_eq!(settled.as_deref(), Some("song.mp3"));
        assert_eq!(
            std::fs::read(to.join("song.mp3.part.ottie")).unwrap(),
            b"song"
        );
    }

    #[tokio::test]
    async fn names_taken_meanwhile_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (name, file) = reserve(dir.path(), "song.mp3", false).await.unwrap();
        drop(file);
        let part = dir.path().join(part_name(&name));
        std::fs::write(&part, b"new").unwrap();
        // Both appear while it downloads.
        std::fs::write(dir.path().join("song.mp3"), b"first").unwrap();
        std::fs::write(dir.path().join("song (1).mp3"), b"second").unwrap();
        let name = finalize(dir.path(), "song.mp3", &part, false)
            .await
            .unwrap();
        assert_eq!(name, "song (2).mp3");
        assert_eq!(
            std::fs::read(dir.path().join("song.mp3")).unwrap(),
            b"first"
        );
        assert_eq!(
            std::fs::read(dir.path().join("song (1).mp3")).unwrap(),
            b"second"
        );
        assert_eq!(
            std::fs::read(dir.path().join("song (2).mp3")).unwrap(),
            b"new"
        );
        assert!(!part.exists());
    }

    #[tokio::test]
    async fn replacing_still_leaves_another_download_its_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("song.mp3.part.ottie"), b"busy").unwrap();
        let (name, _file) = reserve(dir.path(), "song.mp3", true).await.unwrap();
        assert_eq!(name, "song (1).mp3");
        let (renamed, _file) = reserve(dir.path(), "song.mp3", false).await.unwrap();
        assert_eq!(renamed, "song (2).mp3");
    }

    #[tokio::test]
    async fn a_file_deleted_by_hand_has_nothing_to_move() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("a"), dir.path().join("b"));
        let name = relocate(&from, "gone.iso", true, &to).await.unwrap();
        assert_eq!(name, "gone.iso");
        assert!(!to.exists());
    }

    #[tokio::test]
    async fn copies_take_turns_and_stop_waiting_when_ottie_stops() {
        let copies = Copies::new(2);
        let first = copies.turn(true).await.unwrap();
        let _second = copies.turn(true).await.unwrap();
        // A third waits for a turn.
        let third = tokio::time::timeout(Duration::from_millis(50), copies.turn(true)).await;
        assert!(third.is_err(), "only two at once");
        drop(first);
        let third = copies.turn(true).await.unwrap();
        // Ottie stops: a copy that hasn't begun gives up, but a file
        // going back to where it was still gets its turn.
        let putting_back = tokio::spawn({
            let copies = copies.clone();
            async move { copies.turn(false).await.map(drop) }
        });
        copies.stop();
        assert!(copies.turn(true).await.is_err());
        drop(third);
        putting_back.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn between_drives_the_original_goes_after_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (source, target) = (dir.path().join("x.bin"), dir.path().join("y.bin"));
        std::fs::write(&source, b"bytes").unwrap();
        std::fs::write(&target, b"").unwrap();
        copy_then_delete(&source, &target).await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"bytes");
        assert!(!source.exists());
        // A failed copy leaves the original alone.
        std::fs::write(&source, b"bytes").unwrap();
        let nowhere = dir.path().join("missing").join("z.bin");
        assert!(copy_then_delete(&source, &nowhere).await.is_err());
        assert!(source.exists());
    }
}
