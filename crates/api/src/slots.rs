//! `/slots`: time slots, when downloads run, stop or are limited
//! (docs/decisions/0040).

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use ottie_core::{SlotId, SlotSettings};

use crate::AppState;
use crate::error::Failure;
use crate::settings::{MAX_CONCURRENT, SPEED_LIMIT, in_range};
use crate::types::{AddedSlot, ApiError, Reason, ReorderSlots, Slot, SlotBody, SlotList};

/// Every time slot, in the order of the list, and the time zone they follow.
#[utoipa::path(
    get,
    path = "/slots",
    operation_id = "listSlots",
    tag = "schedule",
    responses((status = OK, description = "Every slot, and the time zone", body = SlotList))
)]
pub(crate) async fn list(State(state): State<AppState>) -> Result<Json<SlotList>, Failure> {
    let slots = state.manager.slots().await?;
    Ok(Json(SlotList {
        time_zone: ottie_core::time_zone_name(),
        slots: slots.into_iter().map(Slot::from).collect(),
    }))
}

/// Adds a slot at the end of the list (the lowest: others win over it). It
/// applies at once if it covers now.
#[utoipa::path(
    post,
    path = "/slots",
    operation_id = "addSlot",
    tag = "schedule",
    request_body = SlotBody,
    responses(
        (status = CREATED, description = "Added", body = AddedSlot),
        (status = BAD_REQUEST, description = "`slotWithoutDays`, `slotWithoutEffect`, `invalidTime` or `outOfRange` (`maxConcurrent`, `speedLimit`)", body = ApiError),
        (status = NOT_FOUND, description = "`categoryNotFound`", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn add(
    State(state): State<AppState>,
    body: Result<Json<SlotBody>, JsonRejection>,
) -> Result<(StatusCode, Json<AddedSlot>), Failure> {
    let Json(body) = body?;
    let id = state.manager.add_slot(settings(body)?).await?;
    Ok((StatusCode::CREATED, Json(AddedSlot { id: id.0 })))
}

/// Replaces a slot; it applies at once.
#[utoipa::path(
    put,
    path = "/slots/{id}",
    operation_id = "updateSlot",
    tag = "schedule",
    params(("id" = u32, Path, description = "Slot ID")),
    request_body = SlotBody,
    responses(
        (status = NO_CONTENT, description = "Replaced"),
        (status = BAD_REQUEST, description = "`slotWithoutDays`, `slotWithoutEffect`, `invalidTime` or `outOfRange` (`maxConcurrent`, `speedLimit`)", body = ApiError),
        (status = NOT_FOUND, description = "`slotNotFound` or `categoryNotFound`", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    body: Result<Json<SlotBody>, JsonRejection>,
) -> Result<StatusCode, Failure> {
    let Json(body) = body?;
    state
        .manager
        .update_slot(SlotId(id), settings(body)?)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes a slot; what it held or limited goes back to normal.
#[utoipa::path(
    delete,
    path = "/slots/{id}",
    operation_id = "removeSlot",
    tag = "schedule",
    params(("id" = u32, Path, description = "Slot ID")),
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = NOT_FOUND, description = "`slotNotFound`", body = ApiError),
    )
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    Path(id): Path<u32>,
) -> Result<StatusCode, Failure> {
    state.manager.remove_slot(SlotId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Puts the slots in a new order (the higher one wins where two apply to
/// the same thing). Every client hears of each one that moved through a
/// `slotChanged` event.
#[utoipa::path(
    post,
    path = "/slots/reorder",
    operation_id = "reorderSlots",
    tag = "schedule",
    request_body = ReorderSlots,
    responses(
        (status = NO_CONTENT, description = "Reordered"),
        (status = BAD_REQUEST, description = "`invalidSlotOrder`: not every slot once", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn reorder(
    State(state): State<AppState>,
    body: Result<Json<ReorderSlots>, JsonRejection>,
) -> Result<StatusCode, Failure> {
    let Json(body) = body?;
    let ids = body.ids.into_iter().map(SlotId).collect();
    state.manager.reorder_slots(ids).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The engine's settings from a body, checking what the engine can't.
fn settings(body: SlotBody) -> Result<SlotSettings, Failure> {
    if let Some(max) = body.max_concurrent {
        in_range("maxConcurrent", u64::from(max), &MAX_CONCURRENT)?;
    }
    if let Some(limit) = body.speed_limit {
        in_range("speedLimit", limit, &SPEED_LIMIT)?;
    }
    body.settings().map_err(|text| {
        Failure::new(
            StatusCode::BAD_REQUEST,
            Reason::InvalidTime { text: text.clone() },
            format!("`{text}` is not a time (HH:MM) or a date (YYYY-MM-DD)"),
        )
    })
}
