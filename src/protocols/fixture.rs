//! The fixture protocol: the minimal template for a new protocol.
//!
//! It exists only in test builds. Copy this file to start a protocol: it
//! implements every required method of [`ProtocolAdapter`] as simply as
//! possible. Every hook added to [`ProtocolAdapter`] from now on must also be
//! implemented here and tested through it, so this file always shows the full
//! set of things a protocol decides.
//!
//! `protocols::activitypub` is the other worked example, a façade over a
//! large existing implementation.

use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex, MutexGuard, PoisonError},
};

use axum::{Json, Router, http::StatusCode, routing::MethodRouter, routing::get};
use futures::future::BoxFuture;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    api::well_known::{FingerLink, WebfingerContribution},
    config::Config,
    error::{AppError, AppResult},
    jobs::{AttemptInfo, HandlerFn, HandlerFuture, JobError},
    models::user::User,
    protocols::{
        ProtocolId,
        adapter::ProtocolAdapter,
        events::AccountEvent,
        identity::{ClaimOutcome, Identity, IdentityStatus},
    },
    state::AppState,
};

/// The fixture adapter.
#[derive(Debug, Clone, Copy)]
pub struct Fixture;

/// The fixture's one job kind, which does nothing.
pub const KIND_NOOP: &str = "fixture.noop";

/// The fixture's own job queue.
///
/// It is deliberately not in [`crate::jobs::DEFAULT_JOB_QUEUES`]: the fixture
/// exists only in test builds, so the default worker never drains it.
pub const QUEUE_FIXTURE: &str = "fixture";

/// The handler of [`KIND_NOOP`]: succeeds without doing anything.
pub fn handle_noop(
    _state: AppState,
    _attempt: AttemptInfo,
    _args: serde_json::Value,
) -> HandlerFuture {
    Box::pin(async { Ok::<(), JobError>(()) })
}

/// The job kinds the fixture registers.
const FIXTURE_JOB_HANDLERS: &[(&str, HandlerFn)] = &[(KIND_NOOP, handle_noop)];

/// The queues the fixture declares.
const FIXTURE_JOB_QUEUES: &[&str] = &[QUEUE_FIXTURE];

/// The `/.well-known` path only the fixture serves.
pub const WELL_KNOWN_PATH: &str = "/.well-known/fixture";

/// The `rel` of the fixture's host-meta link.
pub const HOST_META_REL: &str = "urn:wired:fixture";

/// The longest fixture ID, in characters.
const IDENTIFIER_MAX_LEN: usize = 30;

/// The IDs the fixture treats as taken, standing in for a protocol's table.
static TAKEN_IDENTIFIERS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// Locks [`TAKEN_IDENTIFIERS`], ignoring a poisoning left by a panicked test.
fn taken_identifiers() -> MutexGuard<'static, HashSet<String>> {
    TAKEN_IDENTIFIERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// The ID each account claimed, standing in for the rows of a protocol's table.
static ENROLLED: LazyLock<Mutex<HashMap<Uuid, String>>> = LazyLock::new(Mutex::default);

/// Locks [`ENROLLED`], ignoring a poisoning left by a panicked test.
fn enrolled() -> MutexGuard<'static, HashMap<Uuid, String>> {
    ENROLLED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Claims `identifier` for `user_id` in the taken set, or reports that it is already there.
///
/// The sets are not transactional: a claim stays after the caller's transaction
/// rolls back, which a real protocol's table would not do.
fn claim_identifier(user_id: Uuid, identifier: &str) -> ClaimOutcome {
    if taken_identifiers().insert(identifier.to_owned()) {
        let _previous: Option<String> = enrolled().insert(user_id, identifier.to_owned());
        ClaimOutcome::Claimed
    } else {
        ClaimOutcome::Taken
    }
}

/// Marks `identifier` as taken, as a row in the protocol's table would.
///
/// The set is shared by every test in the process, so a test uses IDs no
/// other test uses.
pub fn mark_taken(identifier: &str) {
    taken_identifiers().extend([identifier.to_owned()]);
}

/// The nickname prefix that makes the fixture's account hooks fail.
///
/// It lets a test prove that one adapter's failure is contained.
pub const FAILING_HOOK_NICKNAME_PREFIX: &str = "fail-hook";

/// One call the fixture's account hooks received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// [`ProtocolAdapter::on_account_event`] with this event.
    Event(AccountEvent),
    /// [`ProtocolAdapter::on_password_change_in_tx`]; the password is not kept.
    PasswordChangedInTx,
}

/// Every hook call the fixture received, with the user it was for.
static RECORDER: LazyLock<Mutex<RecordedCalls>> = LazyLock::new(Mutex::default);

/// The fixture's hook calls, each with the id of the user it was for.
type RecordedCalls = Vec<(Uuid, Recorded)>;

/// Locks [`RECORDER`], ignoring a poisoning left by a panicked test.
fn recorder() -> MutexGuard<'static, RecordedCalls> {
    RECORDER.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The hook calls the fixture received for `user_id`, oldest first.
///
/// The recorder is shared by every test in the process, so a test uses a user
/// of its own.
#[must_use]
pub fn recorded_for(user_id: Uuid) -> Vec<Recorded> {
    recorder()
        .iter()
        .filter(|(recorded_user, _)| *recorded_user == user_id)
        .map(|(_, call)| *call)
        .collect()
}

/// Records `call` for `user`, then fails it when the nickname asks for that.
fn record_hook_call(user: &User, call: Recorded) -> AppResult<()> {
    recorder().push((user.id, call));
    if user.nickname.starts_with(FAILING_HOOK_NICKNAME_PREFIX) {
        Err(AppError::ServiceUnavailable(
            "the fixture hook was asked to fail".to_owned(),
        ))
    } else {
        Ok(())
    }
}

/// Whether `c` may appear in a fixture ID: `a-z`, `0-9` or `-`.
const fn is_identifier_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
}

/// The fixture's one client API route: `GET` answers 204 No Content.
pub const CLIENT_API_PING_PATH: &str = "/api/v1/fixture/ping";

/// `GET /fixture/ping`: answers 204 No Content.
async fn ping() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `GET /.well-known/fixture`: answers a small JSON object.
async fn well_known_document() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "protocol": "fixture" }))
}

impl ProtocolAdapter for Fixture {
    /// The fixture's id.
    fn id(&self) -> ProtocolId {
        ProtocolId::Fixture
    }

    /// One route, `GET /fixture/ping`.
    fn router(&self, _state: &AppState) -> Router<AppState> {
        Router::new().route("/fixture/ping", get(ping))
    }

    /// One client API route, `GET /api/v1/fixture/ping`.
    fn client_api_router(&self, _state: &AppState) -> Router<AppState> {
        Router::new().route(CLIENT_API_PING_PATH, get(ping))
    }

    /// One job kind, [`KIND_NOOP`].
    fn job_handlers(&self) -> &'static [(&'static str, HandlerFn)] {
        FIXTURE_JOB_HANDLERS
    }

    /// One queue, [`QUEUE_FIXTURE`].
    fn job_queues(&self) -> &'static [&'static str] {
        FIXTURE_JOB_QUEUES
    }

    /// One owned path, `GET /.well-known/fixture`.
    fn well_known_routes(&self) -> Vec<(&'static str, MethodRouter<AppState>)> {
        vec![(WELL_KNOWN_PATH, get(well_known_document))]
    }

    /// One alias, `fixture:<nickname>`, and no links.
    fn webfinger(&self, _config: &Config, user: &User) -> WebfingerContribution {
        WebfingerContribution {
            aliases: vec![format!("fixture:{}", user.nickname)],
            links: Vec::new(),
        }
    }

    /// One link, `rel="urn:wired:fixture"`, pointing at [`WELL_KNOWN_PATH`].
    fn host_meta_links(&self, config: &Config) -> Vec<FingerLink> {
        vec![FingerLink {
            rel: HOST_META_REL,
            link_type: None,
            href: Some(format!("{}{WELL_KNOWN_PATH}", config.base_url)),
            template: None,
        }]
    }

    /// No name: `NodeInfo` has no protocol called `fixture`.
    fn nodeinfo_protocol(&self) -> Option<&'static str> {
        None
    }

    /// `fixture_identifier`.
    fn identifier_field(&self) -> &'static str {
        "fixture_identifier"
    }

    /// Trims and lowercases, then requires `^[a-z0-9-]{1,30}$`.
    fn validate_identifier(
        &self,
        requested: &str,
        _config: &Config,
    ) -> Result<String, Vec<String>> {
        let identifier = requested.trim().to_ascii_lowercase();
        let mut errors = Vec::new();
        if !(1..=IDENTIFIER_MAX_LEN).contains(&identifier.chars().count()) {
            errors.push(format!("should be 1 to {IDENTIFIER_MAX_LEN} character(s)"));
        }
        if !identifier.chars().all(is_identifier_char) {
            errors.push("may only contain a-z, 0-9 and -".to_owned());
        }
        if errors.is_empty() {
            Ok(identifier)
        } else {
            Err(errors)
        }
    }

    /// Lowercases the nickname and maps `_` to `-`.
    fn derive_identifier(&self, nickname: &str, _config: &Config) -> String {
        nickname.to_ascii_lowercase().replace('_', "-")
    }

    /// Whether [`mark_taken`] marked the ID; the connection is not used.
    fn identifier_taken<'a>(
        &'a self,
        _conn: &'a mut PgConnection,
        identifier: &'a str,
    ) -> BoxFuture<'a, AppResult<bool>> {
        let taken = taken_identifiers().contains(identifier);
        Box::pin(async move { Ok(taken) })
    }

    /// `fixture:<id>`.
    fn identifier_uri(&self, identifier: &str, _config: &Config) -> String {
        format!("fixture:{identifier}")
    }

    /// The active identity the account claimed, or `None` before it claimed one.
    fn identity<'a>(
        &'a self,
        state: &'a AppState,
        user: &'a User,
    ) -> BoxFuture<'a, AppResult<Option<Identity>>> {
        let identity = enrolled().get(&user.id).map(|identifier| Identity {
            identifier: identifier.clone(),
            uri: self.identifier_uri(identifier, &state.config),
            status: IdentityStatus::Active,
        });
        Box::pin(async move { Ok(identity) })
    }

    /// Adds the ID to the static set and remembers it as the account's identity, or `Taken`.
    fn claim<'a>(
        &'a self,
        _conn: &'a mut PgConnection,
        user: &'a User,
        identifier: &'a str,
        _state: &'a AppState,
    ) -> BoxFuture<'a, AppResult<ClaimOutcome>> {
        let outcome = claim_identifier(user.id, identifier);
        Box::pin(async move { Ok(outcome) })
    }

    /// Records the event; fails for a user whose nickname starts with `fail-hook`.
    fn on_account_event<'a>(
        &'a self,
        _state: &'a AppState,
        user: &'a User,
        event: AccountEvent,
    ) -> BoxFuture<'a, AppResult<()>> {
        let result = record_hook_call(user, Recorded::Event(event));
        Box::pin(async move { result })
    }

    /// Records that it was called, never the password; fails like [`Self::on_account_event`].
    fn on_password_change_in_tx<'a>(
        &'a self,
        _conn: &'a mut PgConnection,
        user: &'a User,
        _new_password: &'a str,
    ) -> BoxFuture<'a, AppResult<()>> {
        let result = record_hook_call(user, Recorded::PasswordChangedInTx);
        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::app_state;

    /// Validates `requested` with the test config.
    fn validate(requested: &str) -> Result<String, Vec<String>> {
        Fixture.validate_identifier(requested, &Config::for_tests())
    }

    /// Derivation lowercases and maps `_` to `-`.
    #[test]
    fn derivation_lowercases_and_maps_underscores() {
        let derived = Fixture.derive_identifier("Cool_Guy", &Config::for_tests());
        assert_eq!(derived, "cool-guy", "derived ID");
    }

    /// A derived ID passes the validator, for the longest and the shortest nickname.
    #[test]
    fn derived_identifiers_validate() {
        let config = Config::for_tests();
        let longest = "Z_".repeat(15);
        for nickname in ["A", "Under_Score_NAME", longest.as_str()] {
            let derived = Fixture.derive_identifier(nickname, &config);
            assert_eq!(validate(&derived), Ok(derived), "derived from {nickname}");
        }
    }

    /// Validation trims and lowercases.
    #[test]
    fn validation_normalises() {
        assert_eq!(validate("  Mixed-Case1 "), Ok("mixed-case1".to_owned()));
    }

    /// Validation rejects blank, long and badly spelled IDs, each with a message.
    #[test]
    fn validation_rejects_bad_identifiers() {
        assert!(rejected_once("   "), "blank");
        assert!(rejected_once(&"a".repeat(31)), "31 characters");
        assert!(rejected_once("under_score"), "underscore");
        assert!(rejected_once("dot.ted"), "dot");
        assert!(
            validate(&"a".repeat(30)).is_ok(),
            "30 characters is allowed"
        );
    }

    /// Whether `requested` is refused with exactly one message.
    fn rejected_once(requested: &str) -> bool {
        matches!(validate(requested), Err(messages) if messages.len() == 1)
    }

    /// The error key, the URI scheme and the taken set are the fixture's own.
    #[tokio::test]
    async fn field_uri_and_taken_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let mut conn = db.pool().acquire().await.expect("a connection");
        assert_eq!(Fixture.identifier_field(), "fixture_identifier", "field");
        assert_eq!(
            Fixture.identifier_uri("cool-guy", &Config::for_tests()),
            "fixture:cool-guy",
            "uri"
        );

        let before = Fixture.identifier_taken(&mut conn, "taken-by-test").await;
        mark_taken("taken-by-test");
        let after = Fixture.identifier_taken(&mut conn, "taken-by-test").await;

        assert!(!before.expect("lookup"), "free before it is marked");
        assert!(after.expect("lookup"), "taken once marked");
    }

    /// Claiming adds the ID to the set once; a second claim finds it taken.
    #[tokio::test]
    async fn claim_adds_the_identifier_then_reports_it_taken() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());
        let user = crate::test_fixtures::local_user(db.pool(), "claimer").await;
        let mut conn = db.pool().acquire().await.expect("a connection");

        let first = Fixture
            .claim(&mut conn, &user, "claimed-once", &state)
            .await;
        let second = Fixture
            .claim(&mut conn, &user, "claimed-once", &state)
            .await;
        let taken = Fixture.identifier_taken(&mut conn, "claimed-once").await;

        assert_eq!(first.expect("claim"), ClaimOutcome::Claimed, "first");
        assert_eq!(second.expect("claim"), ClaimOutcome::Taken, "second");
        assert!(taken.expect("lookup"), "the set holds the ID");
    }

    /// `identity` is `None` for a local user until it claims an ID, then the active claim.
    #[tokio::test]
    async fn identity_is_none_until_claimed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());
        let user = crate::test_fixtures::local_user(db.pool(), "fixture_user").await;
        let mut conn = db.pool().acquire().await.expect("a connection");

        let before = Fixture.identity(&state, &user).await;
        let claimed = Fixture
            .claim(&mut conn, &user, "fixture-user-id", &state)
            .await;
        let after = Fixture.identity(&state, &user).await;

        assert_eq!(before.expect("lookup"), None, "not enrolled");
        assert_eq!(claimed.expect("claim"), ClaimOutcome::Claimed, "claimed");
        assert_eq!(
            after.expect("lookup"),
            Some(Identity {
                identifier: "fixture-user-id".to_owned(),
                uri: "fixture:fixture-user-id".to_owned(),
                status: IdentityStatus::Active,
            }),
            "enrolled under the claimed ID"
        );
    }
}
