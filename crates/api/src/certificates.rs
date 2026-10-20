//! `/certificates`: FTPS servers' certificates trusted by hand
//! (docs/decisions/0054). A download that failed with `certificate` tells
//! the host and fingerprint to post here; resuming it then connects.

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use ottie_core::CertificateId;

use crate::AppState;
use crate::error::{Failure, Param};
use crate::types::{ApiError, TrustCertificate, TrustedCertificate};

/// Every certificate trusted by hand, oldest first.
#[utoipa::path(
    get,
    path = "/certificates",
    operation_id = "listCertificates",
    tag = "certificates",
    responses((status = OK, description = "Every trusted certificate", body = [TrustedCertificate]))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
) -> Result<Json<Vec<TrustedCertificate>>, Failure> {
    let certificates = state.manager.certificates().await?;
    Ok(Json(
        certificates
            .into_iter()
            .map(TrustedCertificate::from)
            .collect(),
    ))
}

/// Trusts a server's certificate from now on. Trusting one already trusted
/// answers with it.
#[utoipa::path(
    post,
    path = "/certificates",
    operation_id = "trustCertificate",
    tag = "certificates",
    request_body = TrustCertificate,
    responses(
        (status = CREATED, description = "Trusted", body = TrustedCertificate),
        (status = BAD_REQUEST, description = "`invalidCertificate`", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn trust(
    State(state): State<AppState>,
    body: Result<Json<TrustCertificate>, JsonRejection>,
) -> Result<(StatusCode, Json<TrustedCertificate>), Failure> {
    let Json(body) = body?;
    let trusted = state
        .manager
        .trust_certificate(&body.host, &body.fingerprint)
        .await?;
    Ok((StatusCode::CREATED, Json(trusted.into())))
}

/// Stops trusting a certificate: FTPS downloads from that server refuse it again.
#[utoipa::path(
    delete,
    path = "/certificates/{id}",
    operation_id = "distrustCertificate",
    tag = "certificates",
    params(("id" = u32, Path, description = "Certificate ID")),
    responses(
        (status = NO_CONTENT, description = "No longer trusted"),
        (status = NOT_FOUND, description = "`certificateNotFound`", body = ApiError),
    )
)]
pub(crate) async fn distrust(
    State(state): State<AppState>,
    Param(id): Param<u32>,
) -> Result<StatusCode, Failure> {
    state
        .manager
        .distrust_certificate(CertificateId(id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
