//! `/jobs`: list, one, add (one or several), pause, resume, cancel, remove,
//! change a job's own settings, reorder the queue, change category, and
//! which links some text holds.

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use ottie_core::{CategoryId, JobId, NewJob, RequestHeaders};

use crate::AppState;
use crate::access::{Caller, Folders};
use crate::categories::{absolute_folder, parse_url};
use crate::error::{Failure, Param};
use crate::types::{
    AddJob, AddJobs, Added, AddedJobs, ApiError, Job, JobChange, MoveJob, Preview, PreviewLinks,
    RefusedJob, ReorderJobs, from_millis,
};

/// Every job, oldest first.
#[utoipa::path(
    get,
    path = "/jobs",
    operation_id = "listJobs",
    tag = "jobs",
    responses((status = OK, description = "Every job, oldest first", body = [Job]))
)]
pub(crate) async fn list(State(state): State<AppState>) -> Result<Json<Vec<Job>>, Failure> {
    let jobs = state.manager.list().await?;
    Ok(Json(jobs.into_iter().map(Job::from).collect()))
}

/// One download.
#[utoipa::path(
    get,
    path = "/jobs/{id}",
    operation_id = "getJob",
    tag = "jobs",
    params(("id" = String, Path, description = "Job ID, like `2026.10.13-14.28.59-1`")),
    responses(
        (status = OK, description = "The download", body = Job),
        (status = BAD_REQUEST, description = "`invalidJobId`", body = ApiError),
        (status = NOT_FOUND, description = "`jobNotFound`", body = ApiError),
    )
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    Param(id): Param<String>,
) -> Result<Json<Job>, Failure> {
    let job = state.manager.job(id.parse::<JobId>()?).await?;
    Ok(Json(job.into()))
}

/// Queues a download. It starts as soon as a slot is free, or waits for
/// its start time (`scheduled`) if it has one. Sent again with the same
/// `requestId`, it answers the first one's ID instead of adding another.
#[utoipa::path(
    post,
    path = "/jobs",
    operation_id = "addJob",
    tag = "jobs",
    request_body = AddJob,
    responses(
        (status = CREATED, description = "Queued", body = Added),
        (status = BAD_REQUEST, description = "`invalidUrl`, `unsupportedScheme`, `relativeFolder`, `nonUtf8Path`, `noFolder`, `stopBeforeStart`, `outOfRange` (`startAt`, `stopAt`: past the end of year 9999) or `invalidRequestId`", body = ApiError),
        (status = FORBIDDEN, description = "`readOnly`, or `folderNotAllowed`: a device's `destDir` outside the folders it may use", body = ApiError),
        (status = NOT_FOUND, description = "`categoryNotFound`", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn add(
    State(state): State<AppState>,
    caller: Caller,
    body: Result<Json<AddJob>, JsonRejection>,
) -> Result<(StatusCode, Json<Added>), Failure> {
    let Json(body) = body?;
    let request = match &body.request_id {
        Some(id) => Some(state.retried.request(caller.device(), id)?),
        None => None,
    };
    // Held until it's added: the same request sent again meanwhile waits,
    // then answers the same.
    let mut added = match &request {
        Some(request) => Some(request.added.lock().await),
        None => None,
    };
    if let Some(id) = added.as_deref().copied().flatten() {
        return Ok((StatusCode::CREATED, Json(Added { id: id.to_string() })));
    }
    let folders = state.access.folders_for(&caller, &state.manager).await?;
    let id = state.manager.add(new_job(body, &folders)?).await?;
    if let Some(added) = &mut added {
        **added = Some(id);
    }
    Ok((StatusCode::CREATED, Json(Added { id: id.to_string() })))
}

fn new_job(body: AddJob, folders: &Folders) -> Result<NewJob, Failure> {
    let mut job = NewJob::new(parse_url(&body.url)?);
    if let Some(dir) = body.dest_dir {
        job = job.in_dir(folders.check(absolute_folder(dir)?)?);
    }
    if let Some(category) = body.category {
        job = job.in_category(CategoryId(category));
    }
    if let Some(on) = body.streaming {
        job = job.streaming(on);
    }
    if let Some(at) = body.start_at {
        job = job.starting_at(from_millis("startAt", at)?);
    }
    if let Some(at) = body.stop_at {
        job = job.stopping_at(from_millis("stopAt", at)?);
    }
    if let Some(existing) = body.existing {
        job = job.existing(existing.into());
    }
    Ok(job.with_headers(RequestHeaders {
        cookie: body.cookie,
        referrer: body.referrer,
        user_agent: body.user_agent,
    }))
}

/// Queues several downloads at once, in order, each as `POST /jobs` would.
/// Those that can be added are, even if others are refused: the answer
/// gives each one's ID or why it was refused. Clients hear of each one added
/// through a `changed` event (many at once may come as a `resync`).
#[utoipa::path(
    post,
    path = "/jobs/batch",
    operation_id = "addJobs",
    tag = "jobs",
    request_body = AddJobs,
    responses(
        (status = OK, description = "Each one's ID, or why it was refused", body = AddedJobs),
        (status = BAD_REQUEST, description = "`tooManyLinks`: more than the setting `maxBatch`; none added", body = ApiError),
        (status = FORBIDDEN, description = "`readOnly`", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn add_many(
    State(state): State<AppState>,
    caller: Caller,
    body: Result<Json<AddJobs>, JsonRejection>,
) -> Result<Json<AddedJobs>, Failure> {
    let Json(body) = body?;
    let folders = state.access.folders_for(&caller, &state.manager).await?;
    let count = body.jobs.len();
    let mut refused = Vec::new();
    // The jobs to add, and the place of each in the request.
    let (places, jobs): (Vec<usize>, Vec<NewJob>) = body
        .jobs
        .into_iter()
        .enumerate()
        .filter_map(|(place, job)| match new_job(job, &folders) {
            Ok(job) => Some((place, job)),
            Err(failure) => {
                refused.push((place, failure));
                None
            }
        })
        .unzip();
    let results = state.manager.add_many(jobs).await?;
    let mut ids = vec![None; count];
    for (place, result) in places.into_iter().zip(results) {
        match result {
            Ok(id) => {
                if let Some(slot) = ids.get_mut(place) {
                    *slot = Some(id.to_string());
                }
            }
            Err(err) => refused.push((place, err.into())),
        }
    }
    refused.sort_by_key(|(place, _)| *place);
    let refused = refused
        .into_iter()
        .map(|(place, failure)| RefusedJob {
            index: u32::try_from(place).unwrap_or(u32::MAX),
            error: failure.into(),
        })
        .collect();
    Ok(Json(AddedJobs { ids, refused }))
}

/// The links in some text (one per line, an email, a web page): in order,
/// each once, with what adding each would do. Anything of the form
/// `scheme://…` counts, so links that can't be added are listed too, with
/// why. Nothing is added. A POST, so links, which may carry tokens, never
/// sit in an address or a log. A device's `destDir` must be one it may
/// write to: whether a file is there is never looked up anywhere else.
#[utoipa::path(
    post,
    path = "/jobs/preview",
    operation_id = "previewLinks",
    tag = "jobs",
    request_body = PreviewLinks,
    responses(
        (status = OK, description = "The links found", body = Preview),
        (status = BAD_REQUEST, description = "`tooManyLinks` (more than the setting `maxBatch`) or `relativeFolder`", body = ApiError),
        (status = FORBIDDEN, description = "`folderNotAllowed`: a device's `destDir` outside its folders", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn preview(
    State(state): State<AppState>,
    caller: Caller,
    body: Result<Json<PreviewLinks>, JsonRejection>,
) -> Result<Json<Preview>, Failure> {
    let Json(body) = body?;
    let dest_dir = match body.dest_dir {
        Some(dir) => {
            let folders = state.access.folders_for(&caller, &state.manager).await?;
            Some(folders.check(absolute_folder(dir)?)?)
        }
        None => None,
    };
    let links = state.manager.preview(&body.text, dest_dir).await?;
    Ok(Json(Preview {
        links: links.into_iter().map(Into::into).collect(),
    }))
}

/// Pauses a download. Answers once it has really stopped and its file is flushed.
#[utoipa::path(
    post,
    path = "/jobs/{id}/pause",
    operation_id = "pauseJob",
    tag = "jobs",
    params(("id" = String, Path, description = "Job ID, like `2026.10.13-14.28.59-1`")),
    responses(
        (status = NO_CONTENT, description = "Paused"),
        (status = BAD_REQUEST, description = "`invalidJobId`", body = ApiError),
        (status = NOT_FOUND, description = "`jobNotFound`", body = ApiError),
        (status = CONFLICT, description = "`invalidTransition` (a finished download…) or `jobMoving`", body = ApiError),
    )
)]
pub(crate) async fn pause(
    State(state): State<AppState>,
    Param(id): Param<String>,
) -> Result<StatusCode, Failure> {
    state.manager.pause(id.parse::<JobId>()?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Puts a paused or failed download back in the queue; a scheduled one
/// joins it now, without waiting for its start time.
#[utoipa::path(
    post,
    path = "/jobs/{id}/resume",
    operation_id = "resumeJob",
    tag = "jobs",
    params(("id" = String, Path, description = "Job ID, like `2026.10.13-14.28.59-1`")),
    responses(
        (status = NO_CONTENT, description = "Queued again"),
        (status = BAD_REQUEST, description = "`invalidJobId`", body = ApiError),
        (status = NOT_FOUND, description = "`jobNotFound`", body = ApiError),
        (status = CONFLICT, description = "`invalidTransition` or `jobMoving`", body = ApiError),
    )
)]
pub(crate) async fn resume(
    State(state): State<AppState>,
    Param(id): Param<String>,
) -> Result<StatusCode, Failure> {
    state.manager.resume(id.parse::<JobId>()?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Stops a download for good and deletes its partial file. It stays on the
/// list as `cancelled`.
#[utoipa::path(
    post,
    path = "/jobs/{id}/cancel",
    operation_id = "cancelJob",
    tag = "jobs",
    params(("id" = String, Path, description = "Job ID, like `2026.10.13-14.28.59-1`")),
    responses(
        (status = NO_CONTENT, description = "Cancelled"),
        (status = BAD_REQUEST, description = "`invalidJobId`", body = ApiError),
        (status = NOT_FOUND, description = "`jobNotFound`", body = ApiError),
        (status = CONFLICT, description = "`invalidTransition` or `jobMoving`", body = ApiError),
    )
)]
pub(crate) async fn cancel(
    State(state): State<AppState>,
    Param(id): Param<String>,
) -> Result<StatusCode, Failure> {
    state.manager.cancel(id.parse::<JobId>()?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Takes a download off the list, in any state. Stops it first if it's
/// running (answering once it has) and deletes its partial file; a completed
/// file stays on disk.
#[utoipa::path(
    delete,
    path = "/jobs/{id}",
    operation_id = "removeJob",
    tag = "jobs",
    params(("id" = String, Path, description = "Job ID, like `2026.10.13-14.28.59-1`")),
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = BAD_REQUEST, description = "`invalidJobId`", body = ApiError),
        (status = NOT_FOUND, description = "`jobNotFound`", body = ApiError),
        (status = CONFLICT, description = "`jobMoving`", body = ApiError),
    )
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    Param(id): Param<String>,
) -> Result<StatusCode, Failure> {
    state.manager.remove(id.parse::<JobId>()?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Changes a download's own speed limit, priority, streaming mode, start
/// and stop time (the fields given), in any state; they take effect at
/// once, even while it runs. A start time to come makes it `scheduled` (a
/// running download stops first, and its `changed` event comes once it
/// has). Every client hears of it through one `changed` event, none if
/// nothing changed.
#[utoipa::path(
    patch,
    path = "/jobs/{id}",
    operation_id = "changeJob",
    tag = "jobs",
    params(("id" = String, Path, description = "Job ID, like `2026.10.13-14.28.59-1`")),
    request_body = JobChange,
    responses(
        (status = NO_CONTENT, description = "Changed"),
        (status = BAD_REQUEST, description = "`invalidJobId`, `stopBeforeStart` or `outOfRange` (`speedLimit`, `startAt`, `stopAt`)", body = ApiError),
        (status = NOT_FOUND, description = "`jobNotFound`", body = ApiError),
        (status = CONFLICT, description = "Times for a finished download (`invalidTransition`), or `jobMoving`", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn change(
    State(state): State<AppState>,
    Param(id): Param<String>,
    body: Result<Json<JobChange>, JsonRejection>,
) -> Result<StatusCode, Failure> {
    let Json(change) = body?;
    state
        .manager
        .change_job(id.parse::<JobId>()?, change.checked()?)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Moves downloads in the queue, among the waiting (queued, scheduled or paused)
/// downloads of their priority: priority comes first, the place only orders
/// downloads of the same one. Those moved keep their order among themselves,
/// so neighbours move as a block. Downloads that aren't waiting stay as they
/// are; next to a download (`before`, `after`), so do those of another
/// priority than it. Every client hears of each one that moved through a
/// `changed` event (its new `place`).
#[utoipa::path(
    post,
    path = "/jobs/reorder",
    operation_id = "reorderJobs",
    tag = "jobs",
    request_body = ReorderJobs,
    responses(
        (status = NO_CONTENT, description = "Moved (or already there)"),
        (status = BAD_REQUEST, description = "`invalidJobId`", body = ApiError),
        (status = NOT_FOUND, description = "`jobNotFound`: nothing moved", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn reorder(
    State(state): State<AppState>,
    body: Result<Json<ReorderJobs>, JsonRejection>,
) -> Result<StatusCode, Failure> {
    let Json(body) = body?;
    let ids = body
        .ids
        .iter()
        .map(|id| id.parse::<JobId>())
        .collect::<Result<Vec<_>, _>>()?;
    state.manager.reorder(ids, body.to.try_into()?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Moves a download to another category, as the user's choice (never
/// re-matched), and its file to that category's folder. A running download
/// stops, moves and carries on. Answers once the file has moved, which
/// between drives takes as long as a copy. A category without a folder
/// leaves the file where it is.
#[utoipa::path(
    post,
    path = "/jobs/{id}/category",
    operation_id = "moveJob",
    tag = "jobs",
    params(("id" = String, Path, description = "Job ID, like `2026.10.13-14.28.59-1`")),
    request_body = MoveJob,
    responses(
        (status = NO_CONTENT, description = "Moved"),
        (status = BAD_REQUEST, description = "`invalidJobId`", body = ApiError),
        (status = NOT_FOUND, description = "`jobNotFound` or `categoryNotFound`", body = ApiError),
        (status = CONFLICT, description = "`jobMoving`, or `cannotMoveWhileRunning` (it can't resume)", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "`moveFailed`: the file couldn't move (disk full…)", body = ApiError),
    )
)]
pub(crate) async fn set_category(
    State(state): State<AppState>,
    Param(id): Param<String>,
    body: Result<Json<MoveJob>, JsonRejection>,
) -> Result<StatusCode, Failure> {
    let Json(body) = body?;
    state
        .manager
        .set_category(id.parse::<JobId>()?, CategoryId(body.category))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
