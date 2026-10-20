// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Ottie's HTTP API: REST + WebSocket events over the download engine. Every
//! client (web UI, desktop and Android apps, browser extension, CLI) goes
//! through it. See docs/decisions/0014.
//!
//! Everything is under `/api/v1` and needs a token, as
//! `Authorization: Bearer <token>` or, for the events WebSocket (browsers
//! can't set headers there), as the WebSocket protocol `ottie.token.<token>`:
//! the token file's (this computer's), or a device's ([`Access`],
//! docs/decisions/0052). Only play links (`/stream/…`) do without it: their
//! ticket is the permission.
//!
//! [`openapi`] describes it all; the tests write that to `docs/api/openapi.json`.

mod access;
mod auth;
mod categories;
mod certificates;
mod error;
mod events;
pub mod identity;
mod jobs;
mod openapi;
mod retried;
mod settings;
mod slots;
mod speed;
mod stream;
mod types;
mod ui;
mod views;

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{Method, Uri};
use axum::middleware;
use ottie_core::Manager;
use tokio_util::sync::CancellationToken;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub use access::{ACCESS_FILE, Access, AccessConfig};
pub use auth::{EVENTS_PROTOCOL, TOKEN_PROTOCOL_PREFIX, Token};
pub use types::{AccessInfo, AddDevice, AddedDevice, AllowedFolders, Device, FolderList};
pub use types::{
    AddJob, AddJobs, Added, AddedCategory, AddedJobs, AddedView, AddedWithin, ApiError, Appearance,
    AppearanceMode, Arrangement, BuiltIn, ByteRange, Category, CategoryBody, Event, Existing,
    FoundLink, Job, JobChange, JobState, MatchLink, Matched, MoveJob, Palette, Preview,
    PreviewLinks, Priority, Problem, Reason, RefusedJob, Reorder, ReorderJobs, Settings,
    SettingsChange, SetupStep, SortDirection, SortKey, SpeedHistory, StreamLink, View, ViewBody,
    ViewFilters,
};
pub use types::{AddedSlot, Day, ReorderSlots, Slot, SlotBody, SlotDownloads, SlotList};
pub use types::{IdentityChallenge, IdentityProof, TrustCertificate, TrustedCertificate};
pub use ui::UiSource;

/// Shared by every request handler.
#[derive(Clone)]
struct AppState {
    manager: Manager,
    access: Access,
    /// Cancelled when the daemon stops, so open WebSockets and streams close.
    shutdown: CancellationToken,
    links: Arc<stream::Links>,
    /// `POST /jobs` requests seen lately, for when one is sent again.
    retried: Arc<retried::Retried>,
    /// The OpenAPI description, as served at `/openapi.json`.
    spec: Arc<serde_json::Value>,
}

/// The largest body of a request that carries many links at once (the
/// `maxBatch` setting allows up to 100,000); others keep axum's 2 MB.
const BATCH_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// Everything that needs the token. Each handler's `#[utoipa::path]`
/// gives both its route and its description.
fn private_routes() -> OpenApiRouter<AppState> {
    let batches = OpenApiRouter::new()
        .routes(routes!(jobs::add_many))
        .routes(routes!(jobs::preview))
        .layer(DefaultBodyLimit::max(BATCH_BODY_LIMIT));
    OpenApiRouter::new()
        .merge(batches)
        .routes(routes!(jobs::list, jobs::add))
        .routes(routes!(jobs::get, jobs::remove, jobs::change))
        .routes(routes!(jobs::pause))
        .routes(routes!(jobs::resume))
        .routes(routes!(jobs::cancel))
        .routes(routes!(jobs::set_category))
        .routes(routes!(jobs::reorder))
        .routes(routes!(stream::link))
        .routes(routes!(categories::list, categories::add))
        .routes(routes!(categories::matching))
        .routes(routes!(categories::update, categories::remove))
        .routes(routes!(settings::get, settings::change))
        .routes(routes!(speed::history))
        .routes(routes!(views::list, views::add))
        .routes(routes!(views::update, views::remove))
        .routes(routes!(slots::list, slots::add))
        .routes(routes!(slots::update, slots::remove))
        .routes(routes!(slots::reorder))
        .routes(routes!(access::get))
        .routes(routes!(access::set_folders))
        .routes(routes!(access::list_devices, access::add_device))
        .routes(routes!(access::remove_device))
        .routes(routes!(certificates::list, certificates::trust))
        .routes(routes!(certificates::distrust))
        .routes(routes!(events::subscribe))
        .routes(routes!(openapi::spec))
}

/// Players can't send the token: the ticket in the path stands for it.
fn public_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(stream::serve))
        .routes(routes!(stream::serve_named))
        .routes(routes!(identity::prove))
}

/// The OpenAPI description of the API, as `docs/api/openapi.json` holds it.
pub fn openapi() -> utoipa::openapi::OpenApi {
    let api = OpenApiRouter::new()
        .nest("/api/v1", private_routes().merge(public_routes()))
        .into_openapi();
    openapi::describe(api)
}

/// The API under `/api/v1`, and the web UI everywhere else. Cancel
/// `shutdown` before stopping the server so event streams end.
pub fn router(
    manager: Manager,
    access: Access,
    shutdown: CancellationToken,
    ui: UiSource,
) -> Router {
    // Can't fail: these types always serialize.
    let spec = serde_json::to_value(openapi()).unwrap_or_default();
    access.watch_token_file(shutdown.clone());
    let state = AppState {
        manager,
        access,
        shutdown,
        links: Arc::default(),
        retried: Arc::default(),
        spec: Arc::new(spec),
    };
    let private = private_routes()
        .fallback(not_found)
        // Also covers the fallback, so unknown paths don't reveal what exists.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ));
    let (api, _) = public_routes().merge(private).split_for_parts();
    Router::new()
        .nest("/api/v1", api.with_state(state))
        .fallback(move |method: Method, uri: Uri| async move { ui::serve(&ui, method, &uri).await })
}

async fn not_found() -> error::Failure {
    error::Failure::no_such_endpoint()
}
