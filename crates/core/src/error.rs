use std::path::PathBuf;
use std::time::Duration;

use reqwest::StatusCode;

use crate::category::CategoryId;
use crate::certificate::CertificateId;
use crate::id::JobId;
use crate::job::JobState;
use crate::slot::SlotId;
use crate::view::ViewId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported URL scheme `{0}` (only http, https, ftp, ftps and ftpes are supported)")]
    UnsupportedScheme(String),

    /// Something that starts like a link but isn't one. The text isn't
    /// kept: it may hold credentials.
    #[error("not a valid link")]
    InvalidUrl,

    /// A header the browser sent with a download (`cookie`, `referrer` or
    /// `userAgent`) that can't be sent again: line breaks, or too long.
    #[error("the {0} can't be sent as a header")]
    InvalidHeader(&'static str),

    /// More links at once than [`crate::Settings::max_batch`] allows.
    #[error("{count} links at once; the most is {max}")]
    TooManyLinks { count: usize, max: usize },

    /// Network or protocol failure, with its causes. The URL is stripped so
    /// query strings never end up in logs or error messages.
    #[error("HTTP request failed: {}", with_causes(.0))]
    Http(reqwest::Error),

    /// `retry_after` is the server's `Retry-After`, if it sent one in seconds.
    #[error("server answered {status}")]
    Status {
        status: StatusCode,
        retry_after: Option<Duration>,
    },

    #[error("no data from the server for {} s", .0.as_secs())]
    Stalled(Duration),

    /// FTP: connecting, the TLS handshake or reading failed, or the server's
    /// answer made no sense. English, never with the password.
    #[error("FTP failed: {0}")]
    Ftp(String),

    /// FTP: the server refused a command; `text` is what it said.
    #[error("the FTP server answered {code} {text}")]
    FtpReply { code: u16, text: String },

    /// FTPS: neither the system nor the person trusts the server's
    /// certificate (docs/decisions/0054). `fingerprint` is its SHA-256,
    /// lowercase hex, for trusting it.
    #[error("the certificate of {host} isn't trusted (SHA-256 {fingerprint})")]
    UntrustedCertificate { host: String, fingerprint: String },

    /// An `ftpes://` link, and the server doesn't offer TLS.
    #[error("the FTP server doesn't offer TLS, which an ftpes:// link requires")]
    NoTls,

    #[error("download stopped after {received} of {expected} bytes")]
    Incomplete { received: u64, expected: u64 },

    #[error("the file kept changing on the server while downloading")]
    RemoteUnstable,

    /// A stop (pause, cancel, shutdown…) came while a run was still asking
    /// the server for the file (a request, a login). Never leaves the
    /// backend: its run ends as paused (`transfer::step`).
    #[error("stopped while asking the server")]
    Stopped,

    /// The download's task panicked: a bug in Ottie. The text is the
    /// panic's message.
    #[error("Ottie hit a bug and stopped this download: {0}")]
    Crashed(String),

    #[error("file error: {0}")]
    Io(#[from] std::io::Error),

    /// A file with the download's name is already in its folder, and the
    /// download asks what to do (docs/decisions/0049).
    #[error("{name} is already in the folder")]
    FileExists { name: String },

    #[error("no job with ID {0}")]
    JobNotFound(JobId),

    #[error("a {from:?} job can't become {to:?}")]
    InvalidTransition { from: JobState, to: JobState },

    #[error("a download's stop time must be after its start time")]
    StopBeforeStart,

    #[error("no time slot with ID {0}")]
    SlotNotFound(SlotId),

    #[error("a weekly time slot needs at least one day")]
    SlotWithoutDays,

    #[error("a time slot must allow or stop downloads, or set a limit")]
    SlotWithoutEffect,

    #[error("the new order must list every time slot once")]
    InvalidSlotOrder,

    #[error("no more time slot IDs are available")]
    TooManySlots,

    #[error("`{0}` is not a job ID (expected yyyy.mm.dd-hh.mm.ss-N)")]
    InvalidJobId(String),

    #[error("the download manager has stopped")]
    ManagerStopped,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error(
        "the database was written by a newer Ottie (schema {found}, this version knows up to {supported})"
    )]
    DatabaseTooNew { found: usize, supported: usize },

    #[error("upgrading the database to schema {0} left references to missing rows")]
    MigrationBroken(usize),

    #[error("the database thread has stopped")]
    StoreStopped,

    /// Writing to the database failed (disk full, a drive gone, a broken
    /// file): the database's English text.
    #[error("can't save to the database: {0}")]
    StorageFailed(String),

    #[error("folder path is not valid UTF-8: {}", .0.display())]
    NonUtf8Path(PathBuf),

    #[error("no category with ID {0}")]
    CategoryNotFound(CategoryId),

    #[error("a category name can't be empty")]
    InvalidCategoryName,

    #[error("there is already a category named `{0}`")]
    CategoryNameTaken(String),

    #[error("`{0}` is not a file extension")]
    InvalidExtension(String),

    #[error("files ending in .{extension} already go to category `{category}`")]
    ExtensionTaken { extension: String, category: String },

    #[error("no more category IDs are available")]
    TooManyCategories,

    #[error("the built-in category `{0}` can't be removed")]
    CannotRemoveBuiltIn(String),

    #[error("no folder given, and category `{0}` has none")]
    NoFolder(String),

    #[error("download {0} is being moved")]
    JobMoving(JobId),

    #[error("download {0} can't resume, so it can only move to another folder once it has stopped")]
    CannotMoveWhileRunning(JobId),

    #[error("could not move the file: {0}")]
    MoveFailed(#[source] std::io::Error),

    #[error("no view with ID {0}")]
    ViewNotFound(ViewId),

    #[error("a view name can't be empty")]
    InvalidViewName,

    #[error("there is already a view named `{0}`")]
    ViewNameTaken(String),

    #[error("no more view IDs are available")]
    TooManyViews,

    #[error("not a server's name and a certificate's SHA-256")]
    InvalidCertificate,

    #[error("no trusted certificate with ID {0}")]
    CertificateNotFound(CertificateId),

    #[error("no more certificate IDs are available")]
    TooManyCertificates,

    /// Reading a download that failed or was cancelled before the bytes
    /// asked for arrived.
    #[error("download {0} stopped before these bytes arrived")]
    DownloadStopped(JobId),

    /// The download started over (its file changed on the server) while
    /// being read, so what was read so far belongs to another version.
    #[error("download {0} started over while being read")]
    FileChanged(JobId),
}

impl Error {
    /// Whether trying again later may work: network trouble, a dropped or
    /// stalled connection, or a server that is busy or failing. Not: an
    /// answer like 404, a bad redirect, or a problem with our own disk.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Http(err) => !err.is_redirect() && !err.is_builder(),
            Error::Status { status, .. } => {
                status.is_server_error()
                    || matches!(
                        *status,
                        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
                    )
            }
            Error::Incomplete { .. } | Error::Stalled(_) | Error::Ftp(_) => true,
            // 4xx: the command may succeed later (busy, too many connections).
            Error::FtpReply { code, .. } => (400..500).contains(code),
            _ => false,
        }
    }

    /// How long the server asked us to wait before trying again, if it did.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// `err`, then each error that caused it. reqwest's own message is often
/// only a label over the real cause ("error decoding response body" for any
/// failure while reading a body, say a connection closed by the server).
pub(crate) fn with_causes(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        // Some errors already end with their cause's message.
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Error::Http(err.without_url())
    }
}
