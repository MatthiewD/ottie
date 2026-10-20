//! `/settings`: what applies to the whole daemon.

use std::ops::RangeInclusive;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;

use crate::AppState;
use crate::error::Failure;
use crate::types::{ApiError, Reason, Settings, SettingsChange};

/// Every setting.
#[utoipa::path(
    get,
    path = "/settings",
    operation_id = "getSettings",
    tag = "settings",
    responses((status = OK, description = "Every setting", body = Settings))
)]
pub(crate) async fn get(State(state): State<AppState>) -> Result<Json<Settings>, Failure> {
    Ok(Json(state.manager.settings().await?.into()))
}

/// Changes the settings given, all at once; each is saved and takes effect
/// at once. Every client hears of it through a `settingsChanged` event. One
/// value out of range refuses the whole change.
#[utoipa::path(
    patch,
    path = "/settings",
    operation_id = "changeSettings",
    tag = "settings",
    request_body = SettingsChange,
    responses(
        (status = NO_CONTENT, description = "Changed"),
        (status = BAD_REQUEST, description = "`outOfRange` (with the setting and its range) or `invalidLanguage`", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "`invalidBody`", body = ApiError),
    )
)]
pub(crate) async fn change(
    State(state): State<AppState>,
    body: Result<Json<SettingsChange>, JsonRejection>,
) -> Result<StatusCode, Failure> {
    let Json(change) = body?;
    check(&change)?;
    state.manager.change_settings(change.into()).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// How many downloads may run at once: the setting, and a time slot's.
pub(crate) const MAX_CONCURRENT: RangeInclusive<u64> = 1..=50;

/// Every speed limit (the total one, a category's, a time slot's, a
/// download's), in bytes per second: 0 (none) up to 1 TiB/s, far past any
/// link. The database couldn't hold more than `i64::MAX`.
pub(crate) const SPEED_LIMIT: RangeInclusive<u64> = 0..=1 << 40;

/// A download's start and stop time, in milliseconds since 1970: up to the
/// end of year 9999 (UTC). Every system's clock goes that far (Windows' stops
/// in 30828), and so do the database and JavaScript.
pub(crate) const TIME: RangeInclusive<u64> = 0..=253_402_300_799_999;

/// What people may choose for each setting, by its field name, with the
/// value `change` gives it. The UI gets the same ranges from
/// `shared/api-types/Ranges.ts`, which the tests write from this list.
fn ranged(change: &SettingsChange) -> [(&'static str, Option<u64>, RangeInclusive<u64>); 6] {
    [
        (
            "maxConcurrent",
            change.max_concurrent.map(u64::from),
            MAX_CONCURRENT,
        ),
        ("globalLimit", change.global_limit, SPEED_LIMIT),
        ("maxRetries", change.max_retries.map(u64::from), 0..=1000),
        (
            "retriesBeforeYield",
            change.retries_before_yield.map(u64::from),
            0..=100,
        ),
        // Half a second to an hour.
        (
            "firstRetryWaitMs",
            change.first_retry_wait_ms,
            500..=3_600_000,
        ),
        ("maxBatch", change.max_batch.map(u64::from), 10..=100_000),
    ]
}

fn check(change: &SettingsChange) -> Result<(), Failure> {
    for (setting, value, range) in ranged(change) {
        if let Some(value) = value {
            in_range(setting, value, &range)?;
        }
    }
    if let Some(Some(language)) = &change.language
        && !is_language_tag(language)
    {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            Reason::InvalidLanguage {
                language: language.clone(),
            },
            "not a language tag like `en` or `pt-BR`",
        ));
    }
    Ok(())
}

/// Refuses a value outside its range with `outOfRange`; `setting` is its
/// field name.
pub(crate) fn in_range(
    setting: &str,
    value: u64,
    range: &RangeInclusive<u64>,
) -> Result<(), Failure> {
    if range.contains(&value) {
        Ok(())
    } else {
        Err(out_of_range(setting, range))
    }
}

/// `outOfRange`, for a value of `setting` outside `range`.
pub(crate) fn out_of_range(setting: &str, range: &RangeInclusive<u64>) -> Failure {
    let (min, max) = (*range.start(), *range.end());
    Failure::new(
        StatusCode::BAD_REQUEST,
        Reason::OutOfRange {
            setting: setting.into(),
            min,
            max,
        },
        format!("{setting} must be from {min} to {max}"),
    )
}

/// The shape of a language tag (`en`, `fr`, `pt-BR`, `zh-Hant`): letters,
/// then parts of letters and digits joined by `-`. Whether a front end has
/// that language is its own business.
fn is_language_tag(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let first_ok = parts.next().is_some_and(|first| {
        (2..=8).contains(&first.len()) && first.chars().all(|c| c.is_ascii_alphabetic())
    });
    first_ok
        && tag.len() <= 35
        && parts.all(|part| {
            (1..=8).contains(&part.len()) && part.chars().all(|c| c.is_ascii_alphanumeric())
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{SPEED_LIMIT, is_language_tag, ranged};
    use crate::types::SettingsChange;

    /// Writes the ranges for TypeScript clients, beside the types ts-rs
    /// writes (`TS_RS_EXPORT_DIR`, set in `.cargo/config.toml`).
    #[test]
    fn export_ranges() {
        let mut text = [
            "// Written by `cargo test -p ottie-api` from crates/api/src/settings.rs. Do not edit this file manually.",
            "",
            "/**",
            " * What people may choose for each setting; the daemon refuses the rest",
            " * (`outOfRange`). Waits are in milliseconds, limits in bytes per second;",
            " * `speedLimit` is a download's, a category's or a time slot's.",
            " */",
            "export const RANGES = {",
        ]
        .map(|line| format!("{line}\n"))
        .concat();
        let ranges = ranged(&SettingsChange::default())
            .map(|(setting, _, range)| (setting, range))
            .into_iter()
            .chain([("speedLimit", SPEED_LIMIT)]);
        for (setting, range) in ranges {
            let (min, max) = (range.start(), range.end());
            text.push_str(&format!("\t{setting}: {{ min: {min}, max: {max} }},\n"));
        }
        text.push_str("} as const;\n");

        let Some(dir) = std::env::var_os("TS_RS_EXPORT_DIR") else {
            return; // Not run through cargo: nowhere to write.
        };
        let path = PathBuf::from(dir).join("Ranges.ts");
        // Rewritten only when it changes, so the file's date means something.
        if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&path, text).unwrap();
        }
    }

    #[test]
    fn language_tags_are_checked_for_their_shape() {
        for tag in ["en", "fr", "pt-BR", "zh-Hant-TW", "es-419"] {
            assert!(is_language_tag(tag), "{tag}");
        }
        for tag in [
            "",
            "e",
            "en-",
            "-en",
            "en_US",
            "fr/../x",
            "1a",
            "englishlanguage",
        ] {
            assert!(!is_language_tag(tag), "{tag}");
        }
    }
}
