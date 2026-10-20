//! Turning engine errors into HTTP answers: a status, a code clients can
//! translate, and the English text (`{"error": "…", "code": "…", …}`).

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ottie_core::Error;

use crate::types::{ApiError, Reason};

/// What a handler returns when it can't do what was asked.
#[derive(Debug)]
pub(crate) struct Failure {
    status: StatusCode,
    reason: Reason,
    message: String,
}

impl Failure {
    pub(crate) fn new(status: StatusCode, reason: Reason, message: impl Into<String>) -> Self {
        Failure {
            status,
            reason,
            message: message.into(),
        }
    }

    pub(crate) fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            Reason::Unauthorized,
            "missing or wrong API token",
        )
    }

    pub(crate) fn no_such_link() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            Reason::NoSuchLink,
            "no such play link, or it has expired",
        )
    }

    /// Something that can't go wrong went wrong.
    pub(crate) fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            Reason::Internal,
            "unexpected internal error",
        )
    }

    pub(crate) fn no_such_endpoint() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            Reason::NoSuchEndpoint,
            "no such API endpoint",
        )
    }
}

impl From<Error> for Failure {
    fn from(err: Error) -> Self {
        use StatusCode as S;
        let (status, reason) = match &err {
            Error::JobNotFound(id) => (S::NOT_FOUND, Reason::JobNotFound { id: id.to_string() }),
            Error::CategoryNotFound(id) => (S::NOT_FOUND, Reason::CategoryNotFound { id: id.0 }),
            Error::InvalidTransition { from, to } => (
                S::CONFLICT,
                Reason::InvalidTransition {
                    from: (*from).into(),
                    to: (*to).into(),
                },
            ),
            Error::StopBeforeStart => (S::BAD_REQUEST, Reason::StopBeforeStart),
            Error::SlotNotFound(id) => (S::NOT_FOUND, Reason::SlotNotFound { id: id.0 }),
            Error::SlotWithoutDays => (S::BAD_REQUEST, Reason::SlotWithoutDays),
            Error::SlotWithoutEffect => (S::BAD_REQUEST, Reason::SlotWithoutEffect),
            Error::InvalidSlotOrder => (S::BAD_REQUEST, Reason::InvalidSlotOrder),
            Error::CategoryNameTaken(name) => (
                S::CONFLICT,
                Reason::CategoryNameTaken { name: name.clone() },
            ),
            Error::ExtensionTaken {
                extension,
                category,
            } => (
                S::CONFLICT,
                Reason::ExtensionTaken {
                    extension: extension.clone(),
                    category: category.clone(),
                },
            ),
            Error::CannotRemoveBuiltIn(category) => (
                S::CONFLICT,
                Reason::CannotRemoveBuiltIn {
                    category: category.clone(),
                },
            ),
            Error::InvalidUrl => (S::BAD_REQUEST, Reason::InvalidUrl),
            Error::InvalidHeader(name) => (
                S::BAD_REQUEST,
                Reason::InvalidHeader {
                    name: (*name).to_owned(),
                },
            ),
            Error::TooManyLinks { count, max } => (
                S::BAD_REQUEST,
                Reason::TooManyLinks {
                    count: u64::try_from(*count).unwrap_or(u64::MAX),
                    max: u64::try_from(*max).unwrap_or(u64::MAX),
                },
            ),
            Error::UnsupportedScheme(scheme) => (
                S::BAD_REQUEST,
                Reason::UnsupportedScheme {
                    scheme: scheme.clone(),
                },
            ),
            Error::InvalidJobId(id) => (S::BAD_REQUEST, Reason::InvalidJobId { id: id.clone() }),
            Error::NonUtf8Path(_) => (S::BAD_REQUEST, Reason::NonUtf8Path),
            Error::NoFolder(category) => (
                S::BAD_REQUEST,
                Reason::NoFolder {
                    category: category.clone(),
                },
            ),
            Error::InvalidCategoryName => (S::BAD_REQUEST, Reason::InvalidCategoryName),
            Error::InvalidExtension(extension) => (
                S::BAD_REQUEST,
                Reason::InvalidExtension {
                    extension: extension.clone(),
                },
            ),
            Error::JobMoving(id) => (S::CONFLICT, Reason::JobMoving { id: id.to_string() }),
            Error::CannotMoveWhileRunning(_) => (S::CONFLICT, Reason::CannotMoveWhileRunning),
            Error::MoveFailed(err) => (
                S::INTERNAL_SERVER_ERROR,
                Reason::MoveFailed {
                    detail: err.to_string(),
                },
            ),
            Error::ViewNotFound(id) => (S::NOT_FOUND, Reason::ViewNotFound { id: id.0 }),
            Error::InvalidViewName => (S::BAD_REQUEST, Reason::InvalidViewName),
            Error::ViewNameTaken(name) => {
                (S::CONFLICT, Reason::ViewNameTaken { name: name.clone() })
            }
            Error::InvalidCertificate => (S::BAD_REQUEST, Reason::InvalidCertificate),
            Error::CertificateNotFound(id) => {
                (S::NOT_FOUND, Reason::CertificateNotFound { id: id.0 })
            }
            Error::DownloadStopped(id) => {
                (S::CONFLICT, Reason::DownloadStopped { id: id.to_string() })
            }
            Error::FileChanged(id) => (S::CONFLICT, Reason::FileChanged { id: id.to_string() }),
            Error::ManagerStopped => (S::SERVICE_UNAVAILABLE, Reason::Stopping),
            Error::StorageFailed(_) => (S::INTERNAL_SERVER_ERROR, Reason::StorageFailed),
            _ => {
                // Engine messages never contain URLs or credentials.
                tracing::error!(error = %err, "request failed");
                (S::INTERNAL_SERVER_ERROR, Reason::Internal)
            }
        };
        Failure::new(status, reason, err.to_string())
    }
}

/// A body that isn't the JSON we expect.
impl From<JsonRejection> for Failure {
    fn from(rejection: JsonRejection) -> Self {
        Failure::new(
            rejection.status(),
            Reason::InvalidBody,
            rejection.body_text(),
        )
    }
}

/// The body alone, for an answer that holds several (`POST /jobs/batch`).
impl From<Failure> for ApiError {
    fn from(failure: Failure) -> Self {
        ApiError {
            error: failure.message,
            reason: failure.reason,
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = self.status;
        (status, Json(ApiError::from(self))).into_response()
    }
}
