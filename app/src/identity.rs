//! Whether what answers at an address is this computer's Ottie, before its
//! token goes there (docs/decisions/0058): it must prove it knows the token
//! without receiving it, an HMAC of a random nonce (`POST /api/v1/identity`,
//! as `ottie_api::identity` answers it). A program that took Ottie's port
//! gets nothing, and its page never gets the app's commands.

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tauri::Url;

/// The same as the daemon's (`crates/api/src/identity.rs`).
const CONTEXT: &[u8] = b"ottie identity v1\n";

/// What answers there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// This computer's Ottie: it proved it knows the token.
    Ours,
    /// Something answers, but can't prove it: another program, an Ottie
    /// with other folders, or one older than this app.
    Other,
    /// Nothing answers.
    Nothing,
}

#[derive(Serialize)]
struct Challenge<'a> {
    nonce: &'a str,
}

#[derive(Deserialize)]
struct Proof {
    proof: String,
}

/// Asks the daemon at `origin` to prove it knows `token`.
pub async fn check(origin: &Url, token: &str) -> Found {
    let Ok(url) = origin.join("/api/v1/identity") else {
        return Found::Nothing;
    };
    let Some(nonce) = nonce() else {
        return Found::Nothing;
    };
    let client = reqwest::Client::builder()
        // This computer's Ottie: never through a proxy.
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build();
    let Ok(client) = client else {
        return Found::Nothing;
    };
    let sent = client
        .post(url.as_str())
        .json(&Challenge { nonce: &nonce })
        .send()
        .await;
    let Ok(answer) = sent else {
        return Found::Nothing;
    };
    if !answer.status().is_success() {
        return Found::Other;
    }
    match answer.json::<Proof>().await {
        Ok(Proof { proof }) if proven(token, &nonce, &proof) => Found::Ours,
        _ => Found::Other,
    }
}

/// 32 random bytes as hex.
fn nonce() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Whether `proof` (hex) is HMAC-SHA256(token, context + nonce), compared in
/// constant time.
fn proven(token: &str, nonce: &str, proof: &str) -> bool {
    let bytes: Option<Vec<u8>> = (0..proof.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(proof.get(at..at + 2)?, 16).ok())
        .collect();
    let Some(bytes) = bytes else {
        return false;
    };
    // HMAC takes keys of any length: this never fails.
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(token.as_bytes()) else {
        return false;
    };
    mac.update(CONTEXT);
    mac.update(nonce.as_bytes());
    mac.verify_slice(&bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_daemon_s_own_proof_counts() {
        let nonce = nonce().unwrap();
        // As the daemon makes it.
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(CONTEXT);
        mac.update(nonce.as_bytes());
        let proof: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(proven("secret", &nonce, &proof));
        assert!(!proven("other", &nonce, &proof));
        assert!(!proven("secret", "another nonce", &proof));
        assert!(!proven("secret", &nonce, ""));
        assert!(!proven("secret", &nonce, &proof[..63]));
        assert!(!proven("secret", &nonce, "zz"));
        // What the daemon answers (`crates/api/src/identity.rs` tests the
        // same value).
        assert!(proven(
            "Jefe",
            "abcd",
            "8ee410b85c46f040b43e01cd1190006d73e238bb3bb9c0f730e35b19c980a729"
        ));
    }
}
