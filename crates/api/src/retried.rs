//! `POST /jobs` sent again (docs/decisions/0060): a client that got no
//! answer can't tell whether its download was added, so it sends the same
//! `requestId` again and gets the first one's answer instead of a second
//! download. Kept in memory, per token, for `KEPT`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use ottie_core::JobId;

use crate::error::Failure;
use crate::types::Reason;

/// How long a request is remembered: a client sending it again does so at
/// once (the extension: after starting Ottie, a few seconds).
const KEPT: Duration = Duration::from_secs(10 * 60);
/// At most this many at once; past it, the oldest is forgotten.
const MOST: usize = 10_000;
const LONGEST_ID: usize = 64;

/// Who sent it (`None`: this computer's token, else a device's ID), and its
/// `requestId`.
type Key = (Option<u32>, String);

/// The requests seen lately, as shared state.
#[derive(Default)]
pub(crate) struct Retried {
    seen: Mutex<Seen>,
}

#[derive(Default)]
struct Seen {
    requests: HashMap<Key, Arc<Request>>,
    /// Requests seen so far: their order, which `Instant` can't always tell
    /// (on Windows, many in a row get the same one).
    count: u64,
}

/// One request: what it added, once it has. Locked while it's being added,
/// so the same request sent again meanwhile waits for its answer.
pub(crate) struct Request {
    at: Instant,
    order: u64,
    pub(crate) added: tokio::sync::Mutex<Option<JobId>>,
}

impl Retried {
    /// The request `id` from `caller`, remembered from now on if new.
    pub(crate) fn request(&self, caller: Option<u32>, id: &str) -> Result<Arc<Request>, Failure> {
        if !(1..=LONGEST_ID).contains(&id.len())
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                Reason::InvalidRequestId,
                format!("a requestId has 1 to {LONGEST_ID} letters, digits, - or _"),
            ));
        }
        // Nothing can panic while it's held: a poisoned map is still whole.
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let Seen { requests, count } = &mut *seen;
        let now = Instant::now();
        requests.retain(|_, request| now.duration_since(request.at) < KEPT);
        let key = (caller, id.to_owned());
        if !requests.contains_key(&key) && requests.len() >= MOST {
            let oldest = requests
                .iter()
                .min_by_key(|(_, request)| request.order)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                requests.remove(&oldest);
            }
        }
        let request = requests.entry(key).or_insert_with(|| {
            *count += 1;
            Arc::new(Request {
                at: now,
                order: *count,
                added: tokio::sync::Mutex::new(None),
            })
        });
        Ok(Arc::clone(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_request_from_the_same_token_is_one() {
        let retried = Retried::default();
        let first = retried.request(Some(2), "a1-B_").unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &retried.request(Some(2), "a1-B_").unwrap()
        ));
        // Another token's, or another request.
        assert!(!Arc::ptr_eq(
            &first,
            &retried.request(None, "a1-B_").unwrap()
        ));
        assert!(!Arc::ptr_eq(
            &first,
            &retried.request(Some(2), "a2").unwrap()
        ));
    }

    #[test]
    fn odd_request_ids_are_refused() {
        let retried = Retried::default();
        for id in ["", "a b", "a/b", "é", &"a".repeat(LONGEST_ID + 1)] {
            assert!(retried.request(None, id).is_err(), "{id:?}");
        }
        assert!(retried.request(None, &"a".repeat(LONGEST_ID)).is_ok());
    }

    #[test]
    fn the_oldest_goes_when_too_many_are_kept() {
        let retried = Retried::default();
        let first = retried.request(None, "0").unwrap();
        for n in 1..MOST {
            retried.request(None, &n.to_string()).unwrap();
        }
        assert!(Arc::ptr_eq(&first, &retried.request(None, "0").unwrap()));
        retried.request(None, "new").unwrap();
        assert!(!Arc::ptr_eq(&first, &retried.request(None, "0").unwrap()));
        let kept = retried.seen.lock().unwrap().requests.len();
        assert_eq!(kept, MOST);
    }
}
