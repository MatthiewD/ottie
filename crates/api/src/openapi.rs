//! The OpenAPI description of the API (docs/decisions/0030). It's built from
//! the same routes the server answers (`utoipa-axum`), so every route is in
//! it. What each route says it answers (its error statuses) and which fields
//! may be missing are written by hand in the annotations, and can still be
//! wrong. The tests write it to `docs/api/openapi.json`; a running
//! daemon serves it at `GET /api/v1/openapi.json`.

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
