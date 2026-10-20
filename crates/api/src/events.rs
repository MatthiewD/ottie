//! `GET /events`: a WebSocket that pushes job changes and progress.

use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use ottie_core::JobEvent;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use crate::AppState;
use crate::access::Caller;
use crate::auth::EVENTS_PROTOCOL;
use crate::types::{ApiError, ByteRange, Category, Event, Job, Settings, Slot, View};

/// Live changes, as a WebSocket of JSON `Event` messages. Offer the
/// protocols `ottie.v1` and `ottie.token.<token>` (browsers can't set headers
/// here); the server picks `ottie.v1`. Connect first, then fetch the lists
/// (`GET /jobs`, …), then apply events in order; on `resync`, fetch again.
#[utoipa::path(
    get,
    path = "/events",
    operation_id = "events",
    tag = "events",
    responses(
        (status = SWITCHING_PROTOCOLS, description = "The WebSocket is open; each message is an `Event`", body = Event),
        (status = UNAUTHORIZED, description = "`unauthorized`: no `ottie.token.<token>` protocol with the right token", body = ApiError),
    )
)]
pub(crate) async fn subscribe(
    State(state): State<AppState>,
    caller: Caller,
    ws: WebSocketUpgrade,
) -> Response {
    // Subscribe now rather than after the upgrade, so a client that fetches
    // `GET /jobs` once connected can't miss a change in between.
    let events = state.manager.subscribe();
    let shutdown = state.shutdown.clone();
    ws.protocols([EVENTS_PROTOCOL])
        .on_upgrade(move |socket| forward(socket, events, shutdown, caller.revoked))
}

/// Sends engine events to the client until it leaves, the daemon stops or
/// its token is taken away.
async fn forward(
    mut socket: WebSocket,
    mut events: Receiver<JobEvent>,
    shutdown: CancellationToken,
    revoked: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = revoked.cancelled() => break,
            event = events.recv() => event,
            // The client sends nothing we use; this notices when it leaves.
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => continue,
            },
        };
        let message = match event {
            Ok(JobEvent::Changed(job)) => Event::Changed {
                job: Job::from(job),
            },
            Ok(JobEvent::Progress {
                id,
                downloaded,
                total,
                segments,
            }) => Event::Progress {
                id: id.to_string(),
                downloaded,
                total,
                ranges: segments.into_iter().map(ByteRange::from).collect(),
            },
            Ok(JobEvent::Removed(id)) => Event::Removed { id: id.to_string() },
            Ok(JobEvent::CategoryChanged(category)) => Event::CategoryChanged {
                category: Category::from(category),
            },
            Ok(JobEvent::CategoryRemoved(id)) => Event::CategoryRemoved { id: id.0 },
            Ok(JobEvent::SettingsChanged(settings)) => Event::SettingsChanged {
                settings: Settings::from(settings),
            },
            Ok(JobEvent::ViewChanged(view)) => Event::ViewChanged {
                view: View::from(view),
            },
            Ok(JobEvent::ViewRemoved(id)) => Event::ViewRemoved { id: id.0 },
            Ok(JobEvent::SlotChanged(slot)) => Event::SlotChanged {
                slot: Slot::from(slot),
            },
            Ok(JobEvent::SlotRemoved(id)) => Event::SlotRemoved { id: id.0 },
            // Not part of the API yet (restarts).
            Ok(_) => continue,
            Err(RecvError::Lagged(_)) => Event::Resync,
            Err(RecvError::Closed) => break,
        };
        let Ok(text) = serde_json::to_string(&message) else {
            continue; // Can't happen: these types always serialize.
        };
        // A client that stops reading can't hold up revoking or stopping.
        let sent = tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = revoked.cancelled() => break,
            sent = socket.send(Message::Text(text.into())) => sent,
        };
        if sent.is_err() {
            return;
        }
    }
    // Fails only if the client is already gone; never waits long for one
    // that doesn't read.
    let _ = tokio::time::timeout(CLOSE_WAIT, socket.send(Message::Close(None))).await;
}

/// How long the goodbye may wait for a client.
const CLOSE_WAIT: Duration = Duration::from_secs(1);
