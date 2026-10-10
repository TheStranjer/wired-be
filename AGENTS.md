# AGENTS.md

Rules for anyone (human or AI) changing this codebase.

## Testing

- Every code path you add or change must be covered by tests: success paths, error paths, and edge cases.
- Run `cargo test` before considering any change done. All tests must pass.

## Linting

- `cargo clippy --all-targets --all-features` must pass with zero warnings or errors.
- Fix the code, never the linter. Do not loosen lint settings:
  - no `#[allow(...)]` / `#[expect(...)]` to silence a lint,
  - no removing or downgrading lints in `Cargo.toml` `[lints]`,
  - no raising thresholds in `clippy.toml`.
- Making lints stricter is welcome.
- Run `cargo fmt` before committing.

## Database and configuration

- PostgreSQL is the only database. Use `sqlx` with its Postgres driver; don't add other database backends or ORMs.
- Schema changes go in `sqlx` migrations under `migrations/` (`sqlx migrate add <name>`), never ad-hoc SQL run by hand.
- Configuration comes only from environment variables, parsed in `src/config.rs`. To add a setting: add it to `Config`, give it a test, and document it in `.env.example` and the `README.md` table.
- This repo will be public. Never commit secrets, real credentials, or `.env`. Don't log or `Debug`-print secrets (see the redacting `Debug` impl on `Config`).
- Tests must not mutate the process environment. Pass values in through `Config::from_lookup` instead.

## Conventions

These are community conventions, not compiler rules. Follow them anyway.

### Formatting

- `rustfmt` is the only authority on formatting: 4-space indents, no tabs, max width 100. Don't hand-format or argue with it.
- Imports: one `use` block per group, in this order with a blank line between groups: `std`/`core`, external crates, then `crate::`/`super::`. Let `rustfmt` sort within groups.
- Import traits only for their methods as `use foo::Trait as _;`.

### Naming

| Thing | Style | Example |
|---|---|---|
| Files, directories, modules | `snake_case` | `src/api/statuses.rs` |
| Functions, methods, variables | `snake_case` | `fn fetch_account` |
| Types, traits, enum variants | `UpperCamelCase` | `struct AppState`, `enum Visibility { Public }` |
| Constants, statics | `SCREAMING_SNAKE_CASE` | `const BIND_ADDR` |
| Crate name in `Cargo.toml` | `kebab-case` | `wired-be` (referenced in code as `wired_be`) |

- Acronyms are one word: `HttpClient`, `ActivityPubId`, `Json`, not `HTTPClient`.
- Conversions: `as_*` (cheap borrow), `to_*` (expensive or owned copy), `into_*` (consumes `self`).
- Getters drop `get_`: `fn name(&self)`, not `fn get_name(&self)`.
- Constructors are `new` (or `with_*`/`from_*`); prefer `impl From<T>` over ad-hoc `from_t` functions.

### Project layout

- `src/main.rs` stays thin: read config, build the app, start the server. Logic lives in `src/lib.rs` and its modules so integration tests can import it.
- Module files: use `src/foo.rs` plus `src/foo/bar.rs`. Don't use the old `src/foo/mod.rs` style.
- Unit tests go at the bottom of the file they test, in `#[cfg(test)] mod tests { use super::*; ... }`.
- Integration tests (exercise the whole router over HTTP) go in `tests/*.rs`. Shared test helpers go in `tests/common/mod.rs` (the one place `mod.rs` is conventional, so Cargo doesn't treat it as its own test binary).
- Commit `Cargo.lock` (this is an application).

Target layout as the project grows:

```text
src/
  main.rs         # entry point only
  lib.rs          # declares modules, exposes `app(state) -> Router`
  config.rs       # settings, loaded from env/files
  state.rs        # AppState (shared DB pool, config, HTTP client, ...)
  error.rs        # AppError + `impl IntoResponse for AppError`
  api.rs          # composes the routers below
  api/
    mastodon.rs   # /api/v1, /api/v2 (Mastodon-compatible client API)
    pleroma.rs    # /api/pleroma (Pleroma extensions)
    activitypub.rs# inboxes, outboxes, actors, objects
    well_known.rs # /.well-known/webfinger, nodeinfo, host-meta
  models/         # domain types and DB access
tests/
  common/mod.rs
  *.rs
```

### Axum specifics (v0.8)

Older Axum examples, and much model training data, use v0.6/0.7 syntax. Check `Cargo.toml` for the version actually in use.

- Path parameters are `/{id}` and wildcards are `/{*rest}`. The old `/:id` and `/*rest` syntax panics at startup.
- Custom extractors implement `FromRequestParts`/`FromRequest` as plain `async fn`. No `#[async_trait]`.
- Handlers are `async fn`s taking extractors and returning `impl IntoResponse` or `Result<T, AppError>`.
- At most one extractor may consume the body (`Json`, `Form`, `Bytes`, `String`, `Request`), and it must be the **last** argument.
- Shared state: one `#[derive(Clone)] struct AppState`, holding `Arc`s or cheap-to-clone handles, passed with `Router::with_state` and read with the `State<AppState>` extractor. Don't use globals or `Extension` for app state.
- Each API area exposes a `fn router() -> Router<AppState>`, composed with `.nest("/api/v1", ...)` / `.merge(...)`.
- Errors: one `AppError` type implementing `IntoResponse`, so handlers can use `?`. Don't `unwrap` in handlers (Clippy forbids it anyway).
- Middleware (tracing, timeouts, CORS, body limits) comes from `tower`/`tower-http` layers via `.layer(...)`, not hand-rolled.
- Test handlers without a network socket: build the router and call `tower::ServiceExt::oneshot` (see the tests in `src/main.rs`).

### Errors, docs, misc

- Return `Result`; don't panic in non-test code. Prefer `?` to `match`ing on errors only to re-wrap them.
- Every item, public or private, gets a `///` doc comment (enforced). Module-level docs use `//!`.
- Prefer borrowing (`&str`, `&[T]`) in parameters over owned `String`/`Vec<T>` unless the function needs ownership.
- Add dependencies with `cargo add`, not by hand-editing version numbers.
