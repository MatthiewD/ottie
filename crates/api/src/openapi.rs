//! The OpenAPI description of the API (docs/decisions/0030). It's built from
//! the same routes the server answers (`utoipa-axum`), so every route is in
//! it. What each route says it answers (its error statuses) is written by hand
//! in the annotations, and can still be wrong; which fields may be missing is
//! too, but a test checks it against the TypeScript types. The tests write it
//! to `docs/api/openapi.json`; a running daemon serves it at
//! `GET /api/v1/openapi.json`.

use axum::Json;
use axum::extract::State;
use utoipa::OpenApi;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme};
use utoipa::openapi::{ContentBuilder, Ref, RefOr, ResponseBuilder};

use crate::AppState;
use crate::types::Event;

/// The scheme's name in the spec.
const TOKEN: &str = "token";

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Ottie API",
        description = "The HTTP API of the Ottie daemon (`ottie serve`): every client \
            (web UI, desktop and Android apps, browser extension, scripts) goes through it. \
            How to use it, with examples: docs/api/README.md in the repository.",
        license(name = "MPL-2.0", identifier = "MPL-2.0"),
    ),
    servers((url = "http://127.0.0.1:24068", description = "A daemon on this computer, at the default address")),
    tags(
        (name = "jobs", description = "Downloads: add, pause, resume, cancel, remove, change"),
        (name = "play", description = "Play while downloading: play links that need no token"),
        (name = "categories", description = "Where downloads go, by file extension or folder"),
        (name = "views", description = "Saved views of the downloads list"),
        (name = "certificates", description = "FTPS servers' certificates trusted by hand"),
        (name = "schedule", description = "Time slots: when downloads run, stop or are limited"),
        (name = "settings", description = "What applies to the whole daemon"),
        (name = "speed", description = "The total download speed over the last hour"),
        (name = "events", description = "Live changes over a WebSocket"),
        (name = "access", description = "Devices with tokens of their own, and the folders they may use"),
        (name = "meta", description = "About the API itself"),
    ),
    // Only sent over the WebSocket, so no route names it.
    components(schemas(Event)),
)]
struct ApiDoc;

/// The API's description, with the routes of `routes` (as served under
/// `/api/v1`). Every operation needs the token unless it says otherwise,
/// and every one can answer 503 while the daemon stops.
pub(crate) fn describe(mut api: utoipa::openapi::OpenApi) -> utoipa::openapi::OpenApi {
    let mut doc = ApiDoc::openapi();
    doc.paths = std::mem::take(&mut api.paths);
    doc.merge(api);

    let scheme = HttpBuilder::new()
        .scheme(HttpAuthScheme::Bearer)
        .description(Some(
            "The token from `ottie token` (or `<config folder>/token`): this computer's, \
             or a device's from `POST /access/devices` (`ottie device add`). \
             The events WebSocket takes it as the protocol `ottie.token.<token>` instead.",
        ))
        .build();
    doc.components
        .get_or_insert_with(Default::default)
        .add_security_scheme(TOKEN, SecurityScheme::Http(scheme));
    doc.security = Some(vec![SecurityRequirement::new(TOKEN, Vec::<String>::new())]);

    for (path, item) in doc.paths.paths.iter_mut() {
        let operations = [
            (&mut item.get, false),
            (&mut item.put, true),
            (&mut item.post, true),
            (&mut item.delete, true),
            (&mut item.patch, true),
        ];
        for (operation, changes) in operations {
            let Some(operation) = operation else {
                continue;
            };
            let responses = &mut operation.responses.responses;
            // Operations that set their own security are the public ones.
            if operation.security.is_none() {
                responses
                    .entry("401".into())
                    .or_insert_with(|| refusal("`unauthorized`: no token, or the wrong one"));
                // A read-only device's token may only read (auth.rs).
                let reads = ["/jobs/preview", "/categories/match", "/stream"];
                if changes && !reads.iter().any(|end| path.ends_with(end)) {
                    responses.entry("403".into()).or_insert_with(|| {
                        refusal("`readOnly`: a device's token that may only read")
                    });
                    // What changes is saved before it's answered (0059).
                    responses.entry("500".into()).or_insert_with(|| {
                        refusal("`storageFailed`: the change couldn't be saved, so nothing changed")
                    });
                }
            }
            // A body that axum can't take as JSON (error.rs).
            if operation.request_body.is_some() {
                responses
                    .entry("413".into())
                    .or_insert_with(|| refusal("`invalidBody`: the body is too large"));
                responses
                    .entry("415".into())
                    .or_insert_with(|| refusal("`invalidBody`: not sent as `application/json`"));
            }
            // A parameter in the path that can't be read (`error::Param`).
            if path.contains('{') {
                const BAD_PATH: &str = "`invalidPath`: a parameter in the path can't be read";
                match responses.get_mut("400") {
                    Some(RefOr::T(response)) => {
                        response.description = format!("{}; or {BAD_PATH}", response.description);
                    }
                    Some(RefOr::Ref(_)) => {}
                    None => {
                        responses.insert("400".into(), refusal(BAD_PATH));
                    }
                }
            }
            responses
                .entry("503".into())
                .or_insert_with(|| refusal("`stopping`: the daemon is shutting down"));
        }
    }
    doc
}

/// An error answer with an `ApiError` body.
fn refusal(description: &str) -> RefOr<utoipa::openapi::Response> {
    let body = ContentBuilder::new()
        .schema(Some(Ref::from_schema_name("ApiError")))
        .build();
    RefOr::T(
        ResponseBuilder::new()
            .description(description)
            .content("application/json", body)
            .build(),
    )
}

/// This description, as JSON.
#[utoipa::path(
    get,
    path = "/openapi.json",
    operation_id = "openapi",
    tag = "meta",
    responses((status = OK, description = "The OpenAPI description of this API", content_type = "application/json"))
)]
pub(crate) async fn spec(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(state.spec.as_ref().clone())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::Value;

    /// Writes the description to `docs/api/openapi.json`, where it's
    /// committed for people and tools to read.
    #[test]
    fn export_openapi() {
        let mut text = serde_json::to_string_pretty(&crate::openapi()).unwrap();
        text.push('\n');
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/api");
        if !dir.is_dir() {
            return; // The published crate, outside the repository: nowhere to write.
        }
        let path = dir.join("openapi.json");
        // Rewritten only when it changes, so the file's date means something.
        if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&path, text).unwrap();
        }
    }

    /// utoipa ignores serde's `rename_all_fields`, so a two-word field in a
    /// tagged enum would be described in snake_case while sent in camelCase.
    #[test]
    fn every_property_is_camel_case() {
        fn check(value: &Value, wrong: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    if let Some(Value::Object(properties)) = map.get("properties") {
                        wrong.extend(properties.keys().filter(|key| key.contains('_')).cloned());
                    }
                    map.values().for_each(|value| check(value, wrong));
                }
                Value::Array(items) => items.iter().for_each(|value| check(value, wrong)),
                _ => {}
            }
        }
        let mut wrong = Vec::new();
        check(&serde_json::to_value(crate::openapi()).unwrap(), &mut wrong);
        assert!(wrong.is_empty(), "not camelCase: {wrong:?}");
    }

    /// The fields a TypeScript declaration lets be left out (`name?:`),
    /// and all of them, read from `type X = { … };`; `None` for anything
    /// else (a union, an enum).
    fn ts_fields(decl: &str) -> Option<(Vec<String>, Vec<String>)> {
        // Doc comments may hold any character.
        let mut text = String::new();
        let mut rest = decl;
        while let Some(start) = rest.find("/*") {
            text.push_str(&rest[..start]);
            let end = rest[start..].find("*/")? + start + 2;
            rest = &rest[end..];
        }
        text.push_str(rest);
        let body = text.split_once('=')?.1.trim().trim_end_matches(';').trim();
        let body = body.strip_prefix('{')?.strip_suffix('}')?;
        let (mut optional, mut all) = (Vec::new(), Vec::new());
        // A field starts the body or follows a comma, outside any nesting.
        let mut depth = 0;
        let mut field_next = true;
        let mut chars = body.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            match c {
                '{' | '<' | '(' | '[' => depth += 1,
                '}' | '>' | ')' | ']' => depth -= 1,
                ',' if depth == 0 => field_next = true,
                c if field_next && depth == 0 && (c.is_alphanumeric() || c == '"') => {
                    field_next = false;
                    let mut name = c.to_string();
                    while let Some(&(_, next)) = chars.peek() {
                        if next.is_alphanumeric() || next == '_' || next == '"' {
                            name.push(next);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    let name = name.trim_matches('"').to_owned();
                    let after = body[at..]
                        .trim_start_matches(|c: char| c.is_alphanumeric() || c == '_' || c == '"');
                    if after.trim_start().starts_with('?') {
                        optional.push(name.clone());
                    }
                    all.push(name);
                }
                _ => {}
            }
        }
        Some((optional, all))
    }

    /// The OpenAPI description and the TypeScript types say the same of
    /// each field: required, or left out at times. Both are generated, and
    /// apart: utoipa takes every `Option` as optional unless the field says
    /// `#[schema(required = true)]`, ts-rs only a field with `#[ts(optional)]`.
    /// Answers send their `Option`s as `null`: required. Bodies may leave
    /// theirs out: optional.
    #[test]
    fn openapi_and_typescript_agree_on_which_fields_may_be_missing() {
        use crate::types::*;
        use ts_rs::{Config, TS};

        let cfg = Config::default();
        let mut decls = std::collections::HashMap::new();
        macro_rules! declare {
            ($($name:ident),* $(,)?) => {
                $(decls.insert(stringify!($name), <$name as TS>::decl(&cfg));)*
            };
        }
        declare![
            AccessInfo,
            AddDevice,
            AddJob,
            AddJobs,
            Added,
            AddedCategory,
            AddedDevice,
            AddedJobs,
            AddedSlot,
            AddedView,
            AddedWithin,
            AllowedFolders,
            ApiError,
            Appearance,
            AppearanceMode,
            Arrangement,
            BuiltIn,
            ByteRange,
            Category,
            CategoryBody,
            Day,
            Device,
            Event,
            Existing,
            FolderList,
            FoundLink,
            IdentityChallenge,
            IdentityProof,
            Job,
            JobChange,
            JobState,
            MatchLink,
            Matched,
            MoveJob,
            Palette,
            Preview,
            PreviewLinks,
            Priority,
            Problem,
            Reason,
            RefusedJob,
            Reorder,
            ReorderJobs,
            ReorderSlots,
            Settings,
            SettingsChange,
            SetupStep,
            Slot,
            SlotBody,
            SlotDownloads,
            SlotList,
            SortDirection,
            SortKey,
            SpeedHistory,
            StreamLink,
            TrustCertificate,
            TrustedCertificate,
            View,
            ViewBody,
            ViewFilters,
        ];

        let spec = serde_json::to_value(crate::openapi()).unwrap();
        let mut differ = Vec::new();
        for (name, schema) in spec["components"]["schemas"].as_object().unwrap() {
            let Some(properties) = schema["properties"].as_object() else {
                continue; // A union or an enum: no fields of its own.
            };
            let decl = decls
                .get(name.as_str())
                .unwrap_or_else(|| panic!("{name}: add it to the list above"));
            let (optional, all) =
                ts_fields(decl).unwrap_or_else(|| panic!("{name}: not an object in TypeScript"));
            let required: Vec<&str> = schema["required"]
                .as_array()
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            for field in properties.keys() {
                assert!(all.contains(field), "{name}.{field} isn't in TypeScript");
                let in_openapi = !required.contains(&field.as_str());
                if in_openapi != optional.contains(field) {
                    let says = if in_openapi { "OpenAPI" } else { "TypeScript" };
                    differ.push(format!("{name}.{field} (optional in {says})"));
                }
            }
        }
        assert!(differ.is_empty(), "{} differ: {differ:#?}", differ.len());
    }

    #[test]
    fn only_play_links_and_the_identity_proof_go_without_the_token() {
        let spec = serde_json::to_value(crate::openapi()).unwrap();
        let mut public = Vec::new();
        for (path, item) in spec["paths"].as_object().unwrap() {
            for operation in item.as_object().unwrap().values() {
                if operation.get("security").is_some() {
                    public.push(path.as_str());
                }
            }
        }
        assert_eq!(
            public,
            [
                "/api/v1/identity",
                "/api/v1/stream/{ticket}",
                "/api/v1/stream/{ticket}/{name}"
            ]
        );
    }
}
