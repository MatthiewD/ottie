use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use reqwest::Url;
use reqwest::header::{COOKIE, HeaderMap, HeaderName, HeaderValue, REFERER, USER_AGENT};

use crate::error::Error;
use crate::filename::{self, Existing};
use crate::ftp;

/// Everything needed to start or resume one download. This is the state that
/// will be persisted, so a download can resume after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    /// The URL the user gave. Resuming requests it again (rather than the URL
    /// it redirected to) because redirect targets are often short-lived signed links.
    pub url: Url,
    pub dest_dir: PathBuf,
    /// Final file name, without `.part.ottie`. Chosen when the first response arrives.
    pub file_name: Option<String>,
    /// Size in bytes, if the server told us.
    pub total: Option<u64>,
    /// Bytes written to the `.part.ottie` file.
    pub downloaded: u64,
    /// Identifies the exact version of the remote file we've been downloading.
    pub validator: Option<Validator>,
    /// Whether a resume can continue where it stopped. If not, it restarts from zero.
    pub resumable: bool,
    /// Byte ranges fetched over separate connections, sorted by `start`.
    /// Empty for a single-connection download, whose `.part.ottie` length is
    /// then the truth for how much we have. When not empty, the `.part.ottie`
    /// file has its full size from the start and these ranges are the truth.
    pub segments: Vec<Segment>,
    /// Moves a download that has no file yet to another folder once its real
    /// name is known. Used once, then cleared; not saved.
    pub placement: Option<Placement>,
    /// What to do when a file with its name is already in the folder: at the
    /// first response, and when the file is complete. Set for each run (the
    /// job's choice, else the setting's); not saved here.
    pub existing: Existing,
    /// FTPS: the SHA-256 fingerprints (lowercase hex) of certificates trusted
    /// by hand for this server, besides those the system trusts
    /// (docs/decisions/0054). Set for each run; not saved here.
    pub trusted: Vec<String>,
    /// What the browser sent with it, when the extension handed it over
    /// (docs/decisions/0055). Saved.
    pub headers: RequestHeaders,
    /// Set by the manager for each run: saves the job as it is now, and
    /// waits for the commit. Not saved.
    pub(crate) persist: Option<Persist>,
}

/// Saves the job now and waits until the database has it: a run asks before
/// it preallocates a file for a new plan, so the database never holds an
/// old map beside a file reset under it (docs/decisions/0059). Not part of
/// what a download is: any two are equal.
#[derive(Clone)]
pub(crate) struct Persist(Arc<dyn Fn() -> Saving + Send + Sync>);

/// A save under way, until it's committed.
type Saving = Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>;

impl Persist {
    pub(crate) fn new<F>(save: impl Fn() -> F + Send + Sync + 'static) -> Persist
    where
        F: Future<Output = Result<(), Error>> + Send + 'static,
    {
        Persist(Arc::new(move || Box::pin(save())))
    }

    pub(crate) async fn saved(&self) -> Result<(), Error> {
        (self.0)().await
    }
}

impl PartialEq for Persist {
    fn eq(&self, _: &Persist) -> bool {
        true
    }
}

impl Eq for Persist {}

impl fmt::Debug for Persist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Persist")
    }
}

/// What a browser sent with a download it handed over (docs/decisions/0055):
/// sent again with every HTTP request of that download, and of no other.
/// Sites behind a sign-in need the cookies; some check the referrer, or that
/// the user agent is the one the cookies were given to. FTP ignores them.
///
/// Saved with the job; never shown through the API nor logged: `Debug` only
/// says which are set.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RequestHeaders {
    /// The `Cookie` header's value.
    pub cookie: Option<String>,
    /// The page the download came from (`Referer`).
    pub referrer: Option<String>,
    /// Instead of Ottie's own.
    pub user_agent: Option<String>,
}

impl RequestHeaders {
    /// The most bytes each value may have. Browsers keep a site's cookies
    /// under about 4 KiB each, a few dozen at most.
    pub const MAX_LEN: usize = 32 * 1024;

    pub fn is_empty(&self) -> bool {
        self.cookie.is_none() && self.referrer.is_none() && self.user_agent.is_none()
    }

    /// Fails with the header's name if a value can't be sent as one (line
    /// breaks, other control characters) or is longer than [`Self::MAX_LEN`].
    pub fn check(&self) -> Result<(), Error> {
        for (name, _, value) in self.values() {
            if value.len() > Self::MAX_LEN || HeaderValue::from_str(value).is_err() {
                return Err(Error::InvalidHeader(name));
            }
        }
        Ok(())
    }

    /// The headers every request of the download carries.
    pub(crate) fn header_map(&self) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (_, header, value) in self.values() {
            // `check` refused a value that can't be a header when the job
            // was added; one that still can't is left out.
            if let Ok(mut value) = HeaderValue::from_str(value) {
                value.set_sensitive(true);
                map.insert(header, value);
            }
        }
        map
    }

    /// Those set: the field's name in the API, its header, its value.
    fn values(&self) -> impl Iterator<Item = (&'static str, HeaderName, &str)> {
        [
            ("cookie", COOKIE, &self.cookie),
            ("referrer", REFERER, &self.referrer),
            ("userAgent", USER_AGENT, &self.user_agent),
        ]
        .into_iter()
        .filter_map(|(name, header, value)| Some((name, header, value.as_deref()?)))
    }
}

impl fmt::Debug for RequestHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |value: &Option<String>| if value.is_some() { "…" } else { "-" };
        f.debug_struct("RequestHeaders")
            .field("cookie", &set(&self.cookie))
            .field("referrer", &set(&self.referrer))
            .field("user_agent", &set(&self.user_agent))
            .finish()
    }
}

/// Where to save a download once its real file name is known (from
/// `Content-Disposition` or the URL after redirects), by the name's
/// extension. The manager sets it so a job follows the category its real
/// name belongs to (see docs/decisions/0012).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Lowercase extension without the dot → folder.
    pub by_extension: BTreeMap<String, PathBuf>,
    /// For a name whose extension no entry lists.
    pub otherwise: PathBuf,
}

impl Placement {
    /// `None` for a name without an extension: it stays where it is.
    pub fn folder_for(&self, name: &str) -> Option<&Path> {
        let ext = filename::extension(name)?;
        Some(self.by_extension.get(&ext).unwrap_or(&self.otherwise))
    }
}

/// One range of a multi-connection download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub start: u64,
    /// Exclusive. Can move down while downloading, when an idle connection
    /// takes over the second half of this range.
    pub end: u64,
    /// Bytes from `start` that are flushed to disk.
    pub done: u64,
}

impl Segment {
    pub fn is_complete(&self) -> bool {
        self.start + self.done >= self.end
    }
}

impl Download {
    pub fn new(url: Url, dest_dir: impl Into<PathBuf>) -> Result<Self, Error> {
        check_scheme(&url)?;
        Ok(Self {
            url,
            dest_dir: dest_dir.into(),
            file_name: None,
            total: None,
            downloaded: 0,
            validator: None,
            resumable: false,
            segments: Vec::new(),
            placement: None,
            existing: Existing::default(),
            trusted: Vec::new(),
            headers: RequestHeaders::default(),
            persist: None,
        })
    }

    pub fn part_path(&self) -> Option<PathBuf> {
        self.file_name
            .as_deref()
            .map(|name| self.dest_dir.join(filename::part_name(name)))
    }
}

/// Proves the remote file hasn't changed. HTTP: a strong `ETag`, or failing
/// that `Last-Modified`, sent back in `If-Range` when resuming. FTP: its size
/// and modification time as the server gives them (`SIZE`, `MDTM`), asked
/// again and compared before resuming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validator {
    ETag(String),
    LastModified(String),
    /// `<size> <MDTM time>`. A server without `MDTM` gives none: the size
    /// alone can't tell two versions apart. (Ottie 0.1.0 saved `<size> -`
    /// then; such a job starts over, as its file seems changed.)
    Ftp(String),
}

impl Validator {
    pub fn header_value(&self) -> &str {
        match self {
            Validator::ETag(v) | Validator::LastModified(v) | Validator::Ftp(v) => v,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The server answered and the `.part.ottie` file is ready. Sent once per
    /// run, before any `Progress`, so the caller can save everything needed to
    /// resume even if the process dies before the first byte arrives.
    Started {
        file_name: String,
        /// Where the file is; a [`Placement`] may have changed it.
        dest_dir: PathBuf,

        total: Option<u64>,
        validator: Option<Validator>,
        resumable: bool,
        /// Where this run continues from (0 unless resuming).
        downloaded: u64,
        /// The ranges this run will fetch (empty for a single connection).
        segments: Vec<Segment>,
    },
    Progress {
        downloaded: u64,
        total: Option<u64>,
        /// Multi-connection only: each range with what has arrived so far
        /// (maybe not flushed yet, so never saved). Empty for one connection.
        segments: Vec<Segment>,
    },
    /// Multi-connection only: the ranges as now flushed to disk. Safe to save;
    /// a resume from these never trusts bytes that aren't really there.
    Checkpoint { segments: Vec<Segment> },
    /// The bytes already on disk were thrown away and the download started over.
    Restarted(RestartReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartReason {
    /// The server gave no way to check the file is unchanged, or refuses ranges.
    NotResumable,
    /// The remote file is a different version from the one we started.
    RemoteChanged,
    /// The server ignored our `Range` header and sent the whole file.
    RangesIgnored,
    /// The `.part.ottie` file was deleted.
    PartFileMissing,
    /// The `.part.ottie` file no longer has the size it was created with.
    PartFileDamaged,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed {
        path: PathBuf,
    },
    /// Stopped by the cancellation token. Running again resumes.
    Paused,
}

/// Whether a link can be downloaded: HTTP(S), or FTP in its three forms
/// (`ftp`, `ftps`, `ftpes`; docs/decisions/0054) to a file (not a folder).
pub(crate) fn check_scheme(url: &Url) -> Result<(), Error> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        "ftp" | "ftps" | "ftpes" => ftp::check(url),
        other => Err(Error::UnsupportedScheme(other.to_owned())),
    }
}

/// What an HTTP request (or redirect) may go to: http or https only.
pub(crate) fn check_http_scheme(url: &Url) -> Result<(), Error> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(Error::UnsupportedScheme(other.to_owned())),
    }
}

/// An FTP link (`ftp`, `ftps` or `ftpes`), which the FTP backend downloads.
pub(crate) fn is_ftp(url: &Url) -> bool {
    matches!(url.scheme(), "ftp" | "ftps" | "ftpes")
}
