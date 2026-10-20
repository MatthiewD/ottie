//! `POST /identity`: the daemon proves it's this computer's Ottie without
//! receiving its token (docs/decisions/0058). A program that took Ottie's
//! port first could otherwise collect the token from the first client that
//! sends it. So before sending it, a client sends a random nonce, and the
//! daemon answers with an HMAC of that nonce keyed with the token: only a
//! process that can read the token file can make it. A new nonce each time,
//! so an answer can't be replayed.

use std::ops::RangeInclusive;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::AppState;
use crate::error::Failure;
use crate::types::{ApiError, IdentityChallenge, IdentityProof, Reason};

/// Mixed into the HMAC, so a proof can't stand for anything else made with
/// the token.
const CONTEXT: &[u8] = b"ottie identity v1\n";
/// A nonce is 32 to 128 hex digits (16 to 64 random bytes).
const NONCE_LEN: RangeInclusive<usize> = 32..=128;

/// Proves the daemon knows this computer's token, without the token: the
/// answer is HMAC-SHA256(token, "ottie identity v1\n" + nonce), as hex.
/// Needs no token; ask it before sending one.
#[utoipa::path(
    post,
    path = "/identity",
    operation_id = "proveIdentity",
    tag = "access",
    request_body = IdentityChallenge,
    security(()),
    responses(
        (status = OK, description = "The proof", body = IdentityProof),
        (status = BAD_REQUEST, description = "`invalidBody`: the nonce isn't 32 to 128 hex digits", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn prove(
    State(state): State<AppState>,
    body: Result<Json<IdentityChallenge>, JsonRejection>,
) -> Result<Json<IdentityProof>, Failure> {
    let Json(body) = body?;
    let nonce = body.nonce;
    let fits = NONCE_LEN.contains(&nonce.len()) && nonce.bytes().all(|b| b.is_ascii_hexdigit());
    if !fits {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            Reason::InvalidBody,
            "the nonce must be 32 to 128 hex digits",
        ));
    }
    let proof = state.access.with_owner_token(|token| proof(token, &nonce));
    Ok(Json(IdentityProof { proof }))
}

/// A new nonce: 32 random bytes as hex.
pub fn nonce() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(hex(&bytes))
}

/// What a daemon with `token` answers to `nonce`.
pub fn proof(token: &str, nonce: &str) -> String {
    mac(token, nonce).map_or_else(String::new, |mac| hex(&mac.finalize().into_bytes()))
}

/// Whether `proof` is the answer to `nonce` of a daemon with `token`,
/// compared in constant time.
pub fn proven(token: &str, nonce: &str, proof: &str) -> bool {
    let Some(bytes) = unhex(proof) else {
        return false;
    };
    mac(token, nonce).is_some_and(|mac| mac.verify_slice(&bytes).is_ok())
}

fn mac(token: &str, nonce: &str) -> Option<Hmac<Sha256>> {
    // HMAC takes keys of any length: this never fails.
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).ok()?;
    mac.update(CONTEXT);
    mac.update(nonce.as_bytes());
    Some(mac)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proof_answers_its_nonce_and_token_only() {
        let nonce = nonce().unwrap();
        assert_eq!(nonce.len(), 64);
        let answer = proof("secret", &nonce);
        assert_eq!(answer.len(), 64);
        assert!(proven("secret", &nonce, &answer));
        assert!(!proven("other", &nonce, &answer), "another token");
        assert!(
            !proven("secret", &super::nonce().unwrap(), &answer),
            "another nonce"
        );
        assert!(!proven("secret", &nonce, ""));
        assert!(!proven("secret", &nonce, "zz"));
        assert!(!proven("secret", &nonce, &answer[..62]));
    }

    #[test]
    fn the_proof_is_hmac_sha256_of_the_context_and_nonce() {
        // RFC 4231 test case 2: the HMAC itself.
        let mut mac = Hmac::<Sha256>::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac.finalize().into_bytes()),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // With the context in front of the nonce. The desktop app checks
        // proofs with its own code (`app/src/identity.rs`): its test expects
        // this same value.
        assert_eq!(
            proof("Jefe", "abcd"),
            "8ee410b85c46f040b43e01cd1190006d73e238bb3bb9c0f730e35b19c980a729"
        );
    }
}
