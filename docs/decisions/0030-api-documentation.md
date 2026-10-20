# 0030: API documentation: a written guide and a generated OpenAPI description

Accepted 2026-10-13. Takes over from `docs/decisions/0014` as the place that describes the API: 0014 records how the API started, `docs/api/` describes it as it is now.

**Decision:** the API is documented in two parts, both in `docs/api/`:
- **`README.md`, written by hand,** for people writing a client: where the daemon listens, signing in, conventions, a tour with `curl`, the live events and the order to use them in, errors and their codes, and play links. It's what the OpenAPI description can't say well.
- **`openapi.json`, generated from the code** (OpenAPI 3.1) with `utoipa` 6 and `utoipa-axum` 0.3. Every handler has a `#[utoipa::path]` (its route, parameters, body and answers, with the codes it can refuse with), and every wire type derives `ToSchema` beside `TS`. `utoipa-axum`'s `OpenApiRouter` registers each route from that same annotation, so a route can't exist without its description. That's all it guarantees: the statuses each route says it answers and which fields may be missing are written by hand in the annotations, and can still be wrong (added 2026-10-20). `cargo test -p ottie-api` writes the file (only when it changes), like the TypeScript types. A running daemon serves it at `GET /api/v1/openapi.json`, behind the token like the rest.

**Added to every operation** in one place (`crates/api/src/openapi.rs`), rather than on each handler: the bearer-token scheme, a `401` on every operation that needs it, and a `503` (`stopping`) on all. Play links declare that they need no token, and a test checks they're the only ones.

**Rules**
- A change to the API changes its annotations in the same commit (the file then follows). If it changes how to use the API (a new flow, a new kind of event), update `docs/api/README.md` too.
- `utoipa` ignores serde's `rename_all_fields`, so a field inside a tagged enum (`Problem`, `Reason`, `Event`) would be described in snake_case while it's sent in camelCase. Those fields stay single words, and a test fails on any property that isn't camelCase.
- The `v1` in the path changes only for a change that breaks clients. New endpoints, fields and codes can come at any time, and clients ignore what they don't know.

**Why both crates:** `utoipa` is the usual OpenAPI generator for axum. It reads serde's attributes (`rename_all`, `tag`, `flatten`, `default`), which the types already have for the wire. `utoipa-axum` is a small extension of axum's `Router` that keeps routes and the description in step. Checked with Redocly's linter: valid, with one expected warning (the events WebSocket's only success answer is `101`).

**Alternatives:**
- A written guide only: quick to read, but nothing keeps it true, and tools can't generate clients from it.
- The generated description only: always accurate, but it can't say how the events WebSocket is meant to be used, or why things are the way they are.
- `aide` instead of `utoipa`: also derives descriptions from axum handlers, but it's less widely used, and it builds schemas through `schemars`, a third set of derives next to serde and ts-rs.
