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
   never mod.rs (except tests/common/mod.rs).

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
   local as `user`.

8. **Errors**: one `AppError` (src/error.rs) implementing `IntoResponse`, JSON
   body `{"error": "message"}`. Handlers return `Result<T, AppError>`.

9. **Auth**: OAuth2 bearer tokens. Extractors in `src/auth.rs`: `AuthUser`
   (required), `MaybeAuthUser` (optional). Scope checks follow Mastodon rules
   (`read`, `write`, `follow`, `push`, `admin:read`, `admin:write` and
   sub-scopes like `read:accounts`).

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
    `tests/<area>.rs` and use `tests/common/mod.rs` helpers with `#[sqlx::test]`
    (each test gets a fresh migrated DB; it reads `DATABASE_URL` but never
    mutates the environment) and `tower::ServiceExt::oneshot`. Every endpoint
    needs: success, auth failure (401/403), not found (404), validation error
    (422), pagination where applicable.

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
    (create with `sqlx migrate add <name>`). When a ticket extends an entity or
    endpoint from an earlier ticket, update the earlier tests rather than
    duplicating them. Update README/.env.example when adding config. Finish
    only when `scripts/ci.sh` exits 0.

16. **Reference implementation**: the Pleroma source (Elixir). If available at
    `../pleroma` or `/run/media/neetzsche/BMI60/Code/pleroma`, ticket bodies
    name the relevant files (`lib/pleroma/web/...`). Treat them as behavioural
    reference only; the ticket body is authoritative.
