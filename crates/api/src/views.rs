//! `/views`: the saved views of the downloads list (docs/decisions/0026).

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use ottie_core::ViewId;

use crate::AppState;
use crate::error::{Failure, Param};
use crate::types::{AddedView, ApiError, View, ViewBody};

/// Every saved view, oldest first.
#[utoipa::path(
    get,
    path = "/views",
    operation_id = "listViews",
    tag = "views",
    responses((status = OK, description = "Every saved view", body = [View]))
)]
pub(crate) async fn list(State(state): State<AppState>) -> Result<Json<Vec<View>>, Failure> {
    let views = state.manager.views().await?;
    Ok(Json(views.into_iter().map(View::from).collect()))
}

/// Saves a new view.
#[utoipa::path(
    post,
    path = "/views",
    operation_id = "addView",
    tag = "views",
    request_body = ViewBody,
    responses(
        (status = CREATED, description = "Saved", body = AddedView),
        (status = BAD_REQUEST, description = "`invalidViewName`", body = ApiError),
        (status = CONFLICT, description = "`viewNameTaken`", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn add(
    State(state): State<AppState>,
    body: Result<Json<ViewBody>, JsonRejection>,
) -> Result<(StatusCode, Json<AddedView>), Failure> {
    let Json(body) = body?;
    let id = state.manager.add_view(body.into()).await?;
    Ok((StatusCode::CREATED, Json(AddedView { id: id.0 })))
}

/// Replaces a view, name included.
#[utoipa::path(
    put,
    path = "/views/{id}",
    operation_id = "updateView",
    tag = "views",
    params(("id" = u32, Path, description = "View ID")),
    request_body = ViewBody,
    responses(
        (status = NO_CONTENT, description = "Replaced"),
        (status = BAD_REQUEST, description = "`invalidViewName`", body = ApiError),
        (status = NOT_FOUND, description = "`viewNotFound`", body = ApiError),
        (status = CONFLICT, description = "`viewNameTaken`", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    Param(id): Param<u32>,
    body: Result<Json<ViewBody>, JsonRejection>,
) -> Result<StatusCode, Failure> {
    let Json(body) = body?;
    state.manager.update_view(ViewId(id), body.into()).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes a view.
#[utoipa::path(
    delete,
    path = "/views/{id}",
    operation_id = "removeView",
    tag = "views",
    params(("id" = u32, Path, description = "View ID")),
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = NOT_FOUND, description = "`viewNotFound`", body = ApiError),
    )
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    Param(id): Param<u32>,
) -> Result<StatusCode, Failure> {
    state.manager.remove_view(ViewId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
