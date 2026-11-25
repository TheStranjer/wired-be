# Architecture

Binding decisions for wired-be. Every ticket starts by reading this file; later
tickets refer to "read docs/ARCHITECTURE.md first". The numbering is stable —
cite these decisions by number.

1. **Target**: replicate the Pleroma 2.10 backend HTTP interface:
   Mastodon-compatible client API (/api/v1, /api/v2), Pleroma extensions
   (/api/v1/pleroma, /api/pleroma, admin), OAuth (/oauth), ActivityPub
   federation, WebFinger/NodeInfo, streaming WebSocket. API-only, no frontend.
   Internals are free to differ from Pleroma; the HTTP behaviour (paths,
   params, JSON shapes, status codes, headers) is what must match.
   wired-be serves only the Mastodon/Pleroma client API, ActivityPub
   (actor/object/collection JSON; content negotiation redirects a browser to
   `FRONTEND_BASE_URL`), WebFinger, host-meta and NodeInfo. It renders no HTML
   and serves no frontend: every browser page (login/consent, MFA, password
   reset, remote interaction, invites, permalinks) lives in the frontend at
   `FRONTEND_BASE_URL`. The frontend routes the backend links to are
   `/oauth/authorize`, `/oauth/external/register`, `/oauth/external/consent`,
   `/password_reset/{token}`, `/registration/{token}`,
   `/authorize_interaction?uri=`, `/notice/{id}` and `/{nickname}`.
   `tests/api/route_inventory.rs` gates it: no parameterless `GET` route
   answers `Accept: text/html` with `text/html`.

2. **Stack**: axum 0.8, sqlx 0.9 Postgres, tokio, serde. Runtime-checked
   queries only: `sqlx::query_as::<_, T>("...")` with `#[derive(sqlx::FromRow)]`.
   Do NOT use the `query!` family of macros (they need a live DB at compile
   time).

3. **Layout**: `src/api/<area>.rs` = routers (`pub fn router() -> Router<AppState>`),
   composed in `src/api.rs`; `src/models/<name>.rs` = DB structs + queries;
   `src/entities/<name>.rs` = API JSON serializers (Mastodon "entities":
   Account, Status, Notification...) as `serde::Serialize` structs built from
   models; `src/federation/` = ActivityPub; `src/jobs/` = background job
   handlers; `src/util/` = helpers. Always `src/foo.rs` + `src/foo/bar.rs`,
   never mod.rs.

4. **IDs**: every primary key is a `uuid` column holding a UUIDv7 generated in
   Rust (`uuid::Uuid::now_v7()`), serialized to JSON as the hyphenated
   lowercase string. UUIDv7 text sorts in creation order, so
   `max_id/min_id/since_id` pagination is plain comparison on the id column.

5. **Time**: `timestamptz` columns, `chrono::DateTime<Utc>`. JSON format is
   Mastodon style `2024-01-31T12:34:56.789Z` (millisecond precision, `Z`).

6. **Statuses** are relational (Mastodon style), not Pleroma's JSONB
   activities. Reblogs are status rows with `reblog_of_id`. Anything that
   federates (follows, favourites, reblogs, blocks, reactions) has an
   `ap_id text unique` column: local ones are `{BASE_URL}/activities/{uuid}`;
   remote ones are whatever the remote sent. A local status has
   `uri = {BASE_URL}/objects/{uuid}`.

7. **Users**: a single `users` table for local and remote accounts
   (`local boolean`). Nickname for remote users is stored as `user@host`;
   local as `user`. `users` holds what every protocol shares, for local and
   remote accounts alike. Each protocol keeps its own data in
   `<protocol id>_...` tables keyed by `user_id`, and the main table of a
   protocol is its identity record, which carries a `status`. ActivityPub's
   is `activitypub_actors`: the inbox, outbox and collection endpoints, the
   signing keys (`private_key` for local accounts only), the `publicKey.id`
   and the last fetch time. `ap_id`, `domain`, `actor_type`,
   `also_known_as`, `moved_to_ap_id` and `profile_url` stay on `users` for
   now: `ap_id` is the lookup key across the ActivityPub code and the rest
   are rendered to clients on every account. Moving `ap_id` is a possible
   later track.

8. **Errors**: one `AppError` (src/error.rs) implementing `IntoResponse`, JSON
   body `{"error": "message"}`. Handlers return `Result<T, AppError>`.

9. **Auth**: OAuth2 bearer tokens. Extractors in `src/auth.rs`: `AuthUser`
   (required), `MaybeAuthUser` (optional). Scope checks follow Mastodon rules
   (`read`, `write`, `follow`, `push`, `admin:read`, `admin:write` and
   sub-scopes like `read:accounts`). The authorization-code consent screen
   lives at `{FRONTEND_BASE_URL}/oauth/authorize`: a browser `GET
   /oauth/authorize` is validated and redirected there (302, query kept), the
   JSON `GET`/`POST` of the same path is what that page talks to, and the
   server renders no login or consent HTML on it. External (provider) logins
   follow the same rule: the provider callback redirects an unlinked account to
   `{FRONTEND_BASE_URL}/oauth/external/register?state=…` and a linked account
   of an untrusted application to
   `{FRONTEND_BASE_URL}/oauth/external/consent?state=…&confirm_token=…`; the
   frontend talks to `GET /oauth/registration_details`, `POST
   /oauth/register` and `GET`/`POST /oauth/external/consent`, which answer
   JSON only. The second factor is likewise API-only: an MFA-enabled
   account's password grant answers 403 `mfa_required`, and the client
   finishes it at `POST /oauth/mfa/challenge`; there is no `/oauth/mfa` page.

10. **Request bodies**: Mastodon clients send JSON,
    `application/x-www-form-urlencoded` (with `ids[]=1&ids[]=2` arrays and
    `a[b]=c` nesting) or multipart. All handlers use the project's `ApiBody<T>`
    extractor, never axum's `Json` or `Form` directly.

11. **Background work**: Postgres-backed job queue (table `jobs`,
    `FOR UPDATE SKIP LOCKED`), no Redis, no external queue. Jobs are enqueued
    by queue + kind + JSON args.

12. **Config**: env vars only, via `Config`/`Config::from_lookup` (see
    AGENTS.md). Every setting is documented in `.env.example` and the README
    table.

13. **Tests**: unit tests at the bottom of the file. HTTP tests live in
    one test crate, `tests/api/` (`main.rs` + one `<area>.rs` module per
    area, so the suite is one binary and one link, not 39 of each), and use
    `tests/api/common.rs` helpers with the pooled
    test-database fixture in `src/test_db.rs` (`common::test_db()` hands out a
    slot; it reads `DATABASE_URL` but never mutates the environment) and
    `tower::ServiceExt::oneshot`. The fixture replaced `#[sqlx::test]`, which
    created, migrated and dropped a database per test behind one global lock:
    it migrates one template per fingerprint of the migration set, copies it
    per slot, leases a slot with a session advisory lock and empties it with
    `TRUNCATE` on the way out. Every endpoint needs: success, auth failure
    (401/403), not found (404), validation error (422), pagination where
    applicable.

14. **Clippy realities** (pedantic+nursery+selected restriction, all denied):
    every item needs `///` docs (also private ones, struct fields, enum
    variants); derive `Debug` on every type; no
    `unwrap/expect/panic/indexing/slicing/as` casts outside tests; no `println!`
    or `{:?}` (use `tracing` and Display); functions max 80 lines, cognitive
    complexity 15, max 5 parameters (group extra ones in a params struct),
    nesting depth 4; `use x::Trait as _;` for trait-only imports;
    `unused_results` is denied so handle results or bind them
    (`let _unused = ...;`) only when truly irrelevant. Never add `#[allow]`. If
    a function grows too big, split it.

15. **Ticket discipline**: do exactly what the ticket says, nothing more.
    Migrations are plain forward-only files `migrations/<timestamp>_<name>.sql`
    (create with `sqlx migrate add <name>`, and keep the name it generates:
    a 14-digit `YYYYMMDDHHMMSS` timestamp). Never hand-number a new file. When a ticket extends an entity or
    endpoint from an earlier ticket, update the earlier tests rather than
    duplicating them. Update README/.env.example when adding config. Finish
    only when `scripts/ci.sh` exits 0.

16. **Reference implementation**: the Pleroma source (Elixir). If available at
    `../pleroma` or `/run/media/neetzsche/BMI60/Code/pleroma`, ticket bodies
    name the relevant files (`lib/pleroma/web/...`). Treat them as behavioural
    reference only; the ticket body is authoritative.

17. **Wired extensions**: Wired layers its own API additions over Pleroma's
    the way Pleroma and Akkoma layer theirs over Mastodon's: Mastodon at the
    bottom, Pleroma's extensions (`pleroma` key, `/api/pleroma`,
    `/api/v1/pleroma`), Akkoma's (`akkoma` key, `/api/v1/akkoma`), then
    Wired's. A Wired extension only adds; it never changes what a Mastodon or
    Pleroma field or route means.
    - Extra entity data goes under a `wired` key on the Mastodon entity.
    - New routes go under `/api/v1/wired/*`.
    - A capability is advertised as a `wired:<name>` entry in the shared
      feature list, `services::instance_meta::features()`, which both
      `/api/v1/instance`'s `pleroma.metadata.features` and NodeInfo publish.
    - Clients must check the flag before they rely on a Wired extension.

    The first one is `wired:protocols`: `wired.protocols` on
    `GET /api/v1/instance` and `GET /api/v2/instance` lists the ids
    (`ProtocolId::as_str()`) of the enabled protocols in registry order.

    The same flag covers `GET /api/v1/wired/protocols` (`src/api/wired.rs`),
    which says, for each enabled protocol, whether the account is on it and
    under which ID, or else suggests one.

## Writing tests

`tests/api/common.rs` holds the HTTP test harness (decision 13). It is
enabled by the `integration-tests` cargo feature (a self dev-dependency
turns it on for `tests/` only; it is not named `test-support` because the
denied `redundant_feature_names` lint rejects the `-support` suffix) and
provides `test_config()` (wraps `Config::for_tests()`), `test_db()` (a slot of
the fixture in `src/test_db.rs`), `test_app(pool)`, the `TestRequest`
verb/body builders, `TestResponse` and the `assert_answer`/`assert_error`
gate helpers. The whole suite is one crate (`tests/api/main.rs`) so Cargo
builds one test binary instead of one per area file. The shape every
endpoint test takes (`tests/api/health.rs` is the reference):

```rust
#![cfg(test)]

use axum::http::StatusCode;

use crate::common;

/// `/health` answers 204 No Content with an empty body.
#[tokio::test]
async fn health_returns_no_content() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/health")
        .bearer("tok3n") // optional: .json(&v), .form(&[(k, v)]), .header(k, v)
        .send(&app) // in-process oneshot; no socket
        .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT, "health status");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "errors are JSON"
    );
}
```

Rules that fall out of the lint gate (all verified against `scripts/ci.sh`):

- The suite is one test crate: `tests/api/main.rs` is its root, declares the
  harness once as `pub mod common;`, and `mod`s every `tests/api/<area>.rs`
  with a `///` doc line. Never add a top-level `tests/*.rs` file: Cargo
  compiles each one into its own test binary, statically linking the whole
  crate again — 39 such files cost 39 relinks and 39 debuginfo copies per
  `src/` change. Start every `tests/api/<area>.rs` with `#![cfg(test)]` and
  pull in the harness with `use crate::common;`. Bare `mod common;` at the
  root makes `unreachable_pub` deny the helpers' `pub`, while `pub(crate)`
  helpers trip the nursery `redundant_pub_crate`; `pub` in a `pub` module
  under `#![cfg(test)]` is the only combination that passes — and the
  `cfg(test)` gate is what lets Clippy's `allow-unwrap-in-tests` family
  apply at all (integration crates do not get that exemption by virtue of
  living under `tests/`).
- `common::test_db().await` leases a migrated, emptied slot database and hands
  back a `TestDb`; `db.pool()` is the `PgPool` to build the router on. Keep the
  `TestDb` binding alive for the whole test — dropping it releases the slot.
  In `src/`, the same fixture is `crate::test_db::test_db().await`, whose
  `Result` a test unwraps with `.expect("test database slot")`.
- `TestResponse` has `status`, `headers` and `body` fields, plus
  `.json::<T>()`, `.json_value()`, `.text()` and `.header(name)`.
- Every added code path needs coverage, including the harness itself:
  `tests/api/health.rs` checks the builders and accessors against an echo
  route.
- A WebSocket route is the one endpoint a real socket tests: `oneshot` never
  completes an upgrade (`hyper` needs the live connection to hand over), so
  `tests/api/streaming.rs` binds a `TcpListener` on `127.0.0.1:0`, serves the
  real router with `axum::serve`, and drives it with `tokio-tungstenite`. It
  still leases its database from the fixture, and it still runs in the one test
  crate.
- Later tickets add fixture helpers (`create_user`, `create_token`, ...) to
  `tests/api/common.rs`; keep them there, do not duplicate them per area.
