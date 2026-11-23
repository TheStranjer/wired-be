# AGENTS.md

Rules for anyone (human or AI) changing this codebase.

## The gate, and what you run before it

`scripts/ci.sh` runs every check: a tracked-file length gate, `cargo fmt --check`, Clippy, the whole test suite, and rustdoc with warnings denied. It takes 10–20 minutes. It keeps going after a failure, so one run reports everything that's broken, and it exits 0 only when all checks pass. `.github/workflows/ci.yml` runs this same script. To add a check, add it to the script, not the workflow.

**The full gate runs once, at the end, and not by you.** The harness runs it after you finish (CI runs it again on the push). A change is only done when that run is green, but **you do not run `scripts/ci.sh` or the unfiltered `cargo test` yourself**: it would duplicate the run that is coming anyway, and every extra full run costs minutes on a shared machine.

Your job is to make that run pass the first time, with the least work:

1. **Run only the tests you reason will matter.** Name the code you changed and the behaviour that could break around it: its own unit tests, the `tests/api/<area>.rs` module for any endpoint it serves, and any test that exercises a function you changed or a caller of it. Run those with filters (see "Running checks" below). Err toward a few more related tests, not the whole suite. If a change is wide (a shared helper, `AppState`, the router, a migration), widen the filter to the areas that depend on it.
2. **Run the linter once, after your last edit,** and fix what it reports in the files you added or changed (see "Linting").
3. **Check the file-length limit yourself** with `wc -l` on every file you added or changed: no tracked file may exceed 1,000 lines (`Cargo.lock` is exempt as generated); new code should aim for ≤500. When a file is too long, the fix is to split it into submodules, never to add an exemption.
4. **Run `cargo fmt` once** before you finish.

These runs also compile the code, so the gate's later build mostly reuses it. They don't replace the gate: a failure you didn't predict is expected to turn up there. If the harness hands you a failing gate output, fix what it reports, then re-run only the tests that failed and the ones around your fix.

## Commands you may run

You may run only the commands in the allowlist below, from the repository root. Anything not listed is forbidden; if you think you need one, stop and say so instead of improvising. Where a dedicated file-read or search tool exists, use it instead of the shell equivalents.

### Allowlist

| Purpose | Allowed |
|---|---|
| Build and check | `cargo check`, `cargo build` (add `--locked`) |
| Tests (always filtered) | `cargo test --locked --all-features <filter>`, `cargo test --locked --all-features --lib <filter>`, `cargo test --locked --all-features --test api <filter>`, `cargo test --locked --all-features --test config_docs` |
| Lint, format, docs | `cargo clippy --locked --all-targets --all-features`, `cargo fmt`, `cargo fmt --check`, `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features` (docs: only if you added or changed intra-doc links) |
| Dependencies | `cargo add <crate>` |
| Migrations | `sqlx migrate add <name>` |
| Read files (if no tool exists) | `ls`, `cat`, `head`, `tail` (never `-f`), `wc`, `diff`, `find` (read-only), `grep`/`rg` |
| Git, read-only | `git status`, `git diff`, `git log`, `git show`, `git branch` |
| Git, write | `git switch -c <branch>`, `git add <paths>`, `git commit` |
| Inspect the dev database | `psql "$DATABASE_URL" -c '<single SELECT>'`, read-only |

Everything else is forbidden. The whole gate (`scripts/ci.sh`) and the unfiltered `cargo test` are the harness's to run, not yours. In particular, never run:

- **Process hunting or control**: `pgrep`, `pkill`, `kill`, `killall`, `ps`, `top`, `lsof`, `fuser`, `jobs`, `wait`. `pgrep -f <text>` and `ps | grep <text>` match their *own* command line, so they always find something and any loop around them never ends. You don't need to find processes because you never start any (see below).
- **Sleeping and polling**: `sleep`, `timeout`, `watch`, `tail -f`, `while`/`until`/`for` loops that re-check a condition, and any retry loop. Never wait for time to pass.
- **Background or long-lived processes**: `&`, `nohup`, `disown`, `setsid`, `screen`, `tmux`, `cargo run`, `cargo watch`, starting the server, starting PostgreSQL, `docker`, `systemctl`. Every command runs in the foreground, once, and finishes by itself. Test the app with `tower::ServiceExt::oneshot`, not a running server.
- **Network and system changes**: `curl`, `wget`, `ssh`, `sudo`, `apt`, `pacman`, `pip`, `npm`, `rustup`, `cargo install`, editing anything outside the repository.
- **Destructive or history-rewriting commands**: `rm` in any form, `cargo clean`, `git push`, `git reset --hard`, `git checkout -- <path>`, `git clean`, `git stash`, `git rebase`, `git commit --amend`, `--force`, `--no-verify`, `sqlx database drop`, `psql` with anything but a `SELECT`.
- **Escape hatches**: `eval`, `bash -c`, `sh -c`, `xargs` into other commands, `python`/`perl`/`node` one-liners, and `sed -i`/`awk` to edit files. Edit files with the editing tool.

### Running checks

- **Use exactly the flags in the allowlist.** `--locked --all-features` are what the gate uses, so a test run with them builds the same artifacts the gate will and the gate reuses that build. A run with different flags compiles everything twice.
- **Run each command once per change and read its whole output.** Don't re-run the same command to "check again", to see if a failure is flaky, or hoping the result changes; re-run after you change code. If the output is long, redirect it to a file under `target/` and read the file; don't pipe `cargo` into `head` (it can kill the build with SIGPIPE) and don't hide failures with `| grep`.
- **One cargo command at a time.** Never start a second `cargo` run while another is going; they queue on the same build lock and fight for the same cores and disk.
- **Never run tests (or any other check) in the background.** No `run_in_background`, no `&`, no detached or backgrounded shell sessions, no "start it and look at the output later". Either run `cargo test <filter>` in the foreground and wait for it to return, or don't run it. Never start a run you won't read to completion.
- **Waiting is not a task.** If a command is slow, let it finish. A foreground command that returns has finished; one that doesn't return is hung, so stop and report it rather than polling around it.
- **Two strikes.** If the same fix fails twice, stop and report what you tried and what the output said. Don't try a third variation of the same idea and never work around a block with a command from the forbidden list.
- **Don't weaken the gate.** Never edit `scripts/ci.sh`, `.github/workflows/ci.yml`, `clippy.toml` or the `[lints]` table to make a failure go away; never `#[ignore]`, delete or loosen a failing test to get green; fix the code.
- **Stay in the repo.** Don't create files outside it, and don't leave scratch files in it. Commit only when asked.
- **Non-interactive only.** Never run a command that waits for input or opens an editor (`git commit` needs `-m`).

## Testing

- Never sleep (`std::thread::sleep`, `tokio::time::sleep`) in a test to wait for a job, expiry, token or timeout to elapse. Make time controllable instead: take a timestamp or clock as a parameter, back-date rows in the database (`expires_at` in the past), or call the job's run function directly. If a test must wait for an async effect, await the specific future or channel that signals it, with a bounded `tokio::time::timeout` as a failsafe, never a fixed delay.
- Every code path you add or change must be covered by tests: success paths, error paths, and edge cases.
- Run the tests that cover what you changed and what depends on it (see "The gate, and what you run before it"); the ones you run must pass. Don't run the whole suite: the gate does that.
- A filtered run that matches nothing proves nothing. If `cargo test` reports `0 passed` for your filter, the filter is wrong; fix it.
- A test that needs the database takes a slot from the fixture in `src/test_db.rs`
  (decision 13) — `let db = common::test_db().await;` in `tests/`,
  `crate::test_db::test_db().await` in `src/` — and keeps `db` alive while its
  pool is used. Do not use `#[sqlx::test]`: it creates, migrates and drops a
  database per test, serialized behind one global lock, which is most of the
  suite's wall time. The fixture migrates a template once per fingerprint,
  copies it per slot, leases slots with advisory locks and `TRUNCATE`s a slot
  when it hands it over.
- Because a slot is shared by the tests that run after you, a test must leave
  the schema alone: `TRUNCATE` clears rows, not a `DROP TABLE`. A test that
  changes the schema makes the next lease of its slot rebuild the database from
  the template, which is correct but slow.
- Keep `#[tokio::test]` on the current-thread runtime (the default), like the
  fixture's own tests; use `flavor = "multi_thread"` only when a test really
  needs it.

## Linting

- `cargo clippy --locked --all-targets --all-features` must pass with zero warnings or errors; the gate enforces it. Clippy lints the whole crate and cannot be limited to a file, so run it once after your last edit and read the whole output. Fix every diagnostic in a file you added or changed. A diagnostic in a file you didn't touch is either caused by your change (fix it) or already there (don't chase it; name it in your final report).
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
- Integration tests (exercise the whole router over HTTP) live in one test crate: `tests/api/main.rs` is its root and declares one `mod <area>;` per area file `tests/api/<area>.rs`; shared test helpers go in `tests/api/common.rs`. Cargo compiles every *top-level* `tests/*.rs` file as its own test binary that statically links the whole crate and all dependencies, so each extra top-level file costs a full relink on every `src/` change and a private copy of the debuginfo. One crate means one binary and one link. Add a new area as a module of `tests/api/` (and a `mod` line in `tests/api/main.rs`); never as a new top-level `tests/*.rs`.
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
  fixtures/      # sample actors, included by src/ unit tests
  api/
    main.rs      # the one integration-test crate: `mod`s every file below
    common.rs    # shared HTTP test harness
    <area>.rs    # one module per API area (accounts.rs, ...)
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
