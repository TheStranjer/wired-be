//! Turning `@nick` and `@nick@domain.tld` references in post text into
//! accounts.
//!
//! The reference is Pleroma's formatter (`lib/pleroma/formatter.ex`), which
//! expands mentions while formatting a post, looking each one up and — for
//! accounts it does not know yet — fetching them over the network. This
//! module is that lookup: [`resolve_mentions`] runs [`extract_mentions`]
//! over the text and answers the references it can settle as
//! [`MentionRef`]-[`User`] pairs. Stored accounts answer from the database
//! alone: local ones (written bare or qualified with this instance's own
//! domain) and remote mirrors already kept here (decision 7). A mention of
//! a foreign domain no row stores is fetched through
//! [`get_or_fetch_by_acct`]: every such lookup of one status runs
//! concurrently under one shared [`REMOTE_LOOKUP_BUDGET`], at most
//! [`Config::max_remote_mention_lookups`] of them per status, and a failure
//! or an elapsed budget leaves that mention the plain text it was written
//! as. Pleroma has no equivalent budget: its formatter queues the fetch as
//! a background job and rewrites the mention when the job lands.
//!
//! What callers do with the pairs is theirs: [`crate::models::status`]
//! persists the links (see [`NewStatus::mentioned_user_ids`]), and the
//! Status entity renders them (see [`crate::entities::mention`]).
//!
//! [`Config::max_remote_mention_lookups`]: crate::config::Config::max_remote_mention_lookups
//! [`get_or_fetch_by_acct`]: crate::federation::fetch_actor::get_or_fetch_by_acct
//! [`users`]: crate::models::user
//! [`NewStatus::mentioned_user_ids`]: crate::models::status::NewStatus::mentioned_user_ids

use std::{collections::HashMap, time::Duration};

use futures::future::join_all;
use tracing::debug;

use crate::{
    error::AppResult,
    federation::{
        fetch_actor::{FetchError, get_or_fetch_by_acct, get_or_fetch_by_fingered_ap_id},
        webfinger_client::resolve_acct_with_base,
    },
    models::user::{self, User},
    state::AppState,
    util::entities::{MentionRef, extract_mentions},
};

/// The time all remote mention lookups of one status may take together.
///
/// It is one budget for the whole concurrent batch, not a per-lookup
/// allowance: when it elapses, every mention the batch has not settled is
/// abandoned to plain text.
pub const REMOTE_LOOKUP_BUDGET: Duration = Duration::from_secs(5);

/// Resolves every mention [`extract_mentions`] finds in `text`.
///
/// Answers the pairs it can settle in order of first appearance: a
/// [`MentionRef`] (with the case it was written in) beside the [`User`] it
/// names.
///
/// A reference resolves when [`user::get_by_acct`] finds a row — a local
/// account as bare `nick` or as `nick@`this instance's domain, and a remote
/// account stored under its full `nick@domain` (decision 7). A mention of
/// a foreign domain with no stored row is fetched by this module's
/// `resolve_remote_mention`, concurrently with the other unknown remotes
/// of the same status under the shared [`REMOTE_LOOKUP_BUDGET`] and capped
/// at [`Config::max_remote_mention_lookups`](crate::config::Config::max_remote_mention_lookups)
/// lookups per status. What is
/// left unknown — an unresolvable local, a failed or abandoned fetch, a
/// mention past the cap — is simply absent from the answer, so callers
/// leave it the plain text it was written as. A mention repeated in the
/// text appears once, as [`extract_mentions`] deduplicates it, and is
/// looked up once.
///
/// # Errors
///
/// Any database failure behind the lookups (connection, encoding) arrives
/// as [`AppError::Database`](crate::error::AppError). A remote lookup's own
/// failures are never errors: they resolve to nothing.
pub async fn resolve_mentions(state: &AppState, text: &str) -> AppResult<Vec<(MentionRef, User)>> {
    resolve_mentions_with_base(state, text, None).await
}

/// [`resolve_mentions`] with the `WebFinger` origin of the remote arm
/// replaced by `webfinger_base` (`scheme://authority`) when given.
/// `WebFinger` is HTTPS-only and resolves against the handle's domain, so
/// only this override — the same seam
/// [`crate::federation::fetch_actor`] drives its account tests through —
/// can point a lookup at a loopback mock peer. Production calls
/// [`resolve_mentions`], which passes [`None`].
pub(crate) async fn resolve_mentions_with_base(
    state: &AppState,
    text: &str,
    webfinger_base: Option<&str>,
) -> AppResult<Vec<(MentionRef, User)>> {
    resolve_mentions_on_budget(state, text, webfinger_base, REMOTE_LOOKUP_BUDGET).await
}

/// [`resolve_mentions_with_base`] with the shared remote-lookup batch
/// budget replaced by `budget`. Production always runs the module's
/// [`REMOTE_LOOKUP_BUDGET`]; a test whose mock peer and leased database
/// slot share the machine with the whole suite passes a roomy one, so
/// suite-wide contention over Postgres and the scheduler cannot spend
/// the five seconds the lookups get — the assertions then race the
/// resolver's logic, not wall time. The budget itself is what the
/// stalled-lookup test exercises, driving the module budget on a
/// hand-stepped clock.
pub(crate) async fn resolve_mentions_on_budget(
    state: &AppState,
    text: &str,
    webfinger_base: Option<&str>,
    budget: Duration,
) -> AppResult<Vec<(MentionRef, User)>> {
    let mentions = extract_mentions(text);
    let mut found: HashMap<String, User> = HashMap::new();
    let mut queued: Vec<&MentionRef> = Vec::new();
    for mention in &mentions {
        if let Some(user) = user::get_by_acct(&state.db, &state.config, &mention.acct()).await? {
            drop(found.insert(mention.key(), user));
        } else if !mention.names_domain(state.config.domain())
            && queued.len() < state.config.max_remote_mention_lookups
        {
            queued.push(mention);
        }
    }
    resolve_queued_remotes(state, &queued, &mut found, webfinger_base, budget).await;
    Ok(mentions
        .into_iter()
        .filter_map(|mention| {
            let user = found.remove(&mention.key())?;
            Some((mention, user))
        })
        .collect())
}

/// Runs the queued remote lookups of [`resolve_mentions_on_budget`]
/// concurrently under the single shared `budget` and records whoever was
/// found, keyed as the stored arm keys it. An elapsed budget abandons
/// the whole batch: its mentions resolve to nothing and stay the plain
/// text they were written as.
async fn resolve_queued_remotes(
    state: &AppState,
    queued: &[&MentionRef],
    found: &mut HashMap<String, User>,
    webfinger_base: Option<&str>,
    budget: Duration,
) {
    if queued.is_empty() {
        return;
    }
    let lookups = queued
        .iter()
        .map(|mention| resolve_remote_mention(state, mention, webfinger_base));
    match tokio::time::timeout(budget, join_all(lookups)).await {
        Ok(outcomes) => {
            for (mention, user) in queued.iter().zip(outcomes) {
                if let Some(user) = user {
                    drop(found.insert(mention.key(), user));
                }
            }
        }
        Err(_elapsed) => {
            debug!(
                budget_secs = budget.as_secs(),
                lookups = queued.len(),
                "remote mention lookups ran past their shared budget; leaving them as plain text"
            );
        }
    }
}

/// The federation hook of [`resolve_mentions_with_base`]: find the account
/// a mention of a foreign domain names even though no row stores it yet —
/// Pleroma's formatter resolves such mentions through `User.remote_fetch`
/// (`lib/pleroma/formatter.ex`).
///
/// The lookup is [`get_or_fetch_by_acct`]: `WebFinger` turns the handle
/// into an actor id, the actor is fetched, mirrored into `users`, and it
/// must vouch for the requested handle. Any failure answers `None` — an
/// unreachable peer makes a mention unlinked, not a post refused — and is
/// logged at `debug` with the handle.
///
/// `webfinger_base` is that lookup's mock-peer override, [`None`] in
/// production; see [`resolve_mentions_with_base`] and
/// [`fetch_remote_acct`].
async fn resolve_remote_mention(
    state: &AppState,
    mention: &MentionRef,
    webfinger_base: Option<&str>,
) -> Option<User> {
    let acct = mention.acct();
    match fetch_remote_acct(state, &acct, webfinger_base).await {
        Ok(user) => Some(user),
        Err(err) => {
            debug!(%err, %acct, "remote mention lookup failed; leaving the mention as plain text");
            None
        }
    }
}

/// The one-handle fetch behind [`resolve_remote_mention`]. The override arm
/// (`Some(base)`) drives the same `WebFinger`-then-actor round trip against
/// a loopback mock peer, and skips only the final vouch check
/// [`get_or_fetch_by_acct`] closes it with: a mock answering on `127.0.0.1`
/// can never produce an actor whose id host equals the dotted domain of a
/// handle [`resolve_acct_with_base`] accepts, so the check would reject
/// every mock success. Production passes [`None`] and always goes through
/// [`get_or_fetch_by_acct`] with the check.
async fn fetch_remote_acct(
    state: &AppState,
    acct: &str,
    webfinger_base: Option<&str>,
) -> Result<User, FetchError> {
    match webfinger_base {
        None => get_or_fetch_by_acct(state, acct).await,
        Some(base) => {
            let ap_id = resolve_acct_with_base(&state.http_client, acct, Some(base))
                .await?
                .actor;
            get_or_fetch_by_fingered_ap_id(state, &ap_id).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Router,
        extract::{Path, Query, State},
        http::{StatusCode, header::CONTENT_TYPE},
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use serde_json::json;
    use sqlx::PgPool;
    use tokio::{net::TcpListener, task::JoinHandle};
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        models::user::{NewUser, get_by_id, insert},
        test_fixtures::remote_user,
    };

    /// The instance the fixtures resolve against.
    const BASE: &str = TEST_BASE_URL;
    /// `User-Agent` for the mock-reachable client.
    const TEST_UA: &str = "Wired (https://test.example)";

    /// The state under test: `pool` and the test config, with a client
    /// whose SSRF guard is off so it may reach the loopback mocks.
    fn test_state(pool: &PgPool) -> AppState {
        test_state_with_config(pool, Config::for_tests())
    }

    /// The state under test on `config` instead of the stock test one.
    fn test_state_with_config(pool: &PgPool, config: Config) -> AppState {
        AppState {
            http_client: HttpClient::new_for_tests(TEST_UA),
            ..AppState::for_tests(pool.clone(), config)
        }
    }

    /// Inserts a local user named `nickname`.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &format!("{BASE}/users/{nickname}")),
        )
        .await
        .expect("fixture local user inserts")
    }

    /// The resolved pairs as `(written nickname, written domain, user id)`,
    /// so assertions name both sides of each pair.
    async fn resolved(state: &AppState, text: &str) -> Vec<(String, Option<String>, Uuid)> {
        resolved_against(state, text, None).await
    }

    /// [`resolved`] with the `WebFinger` override `webfinger_base` leads
    /// the remote arm to.
    async fn resolved_against(
        state: &AppState,
        text: &str,
        webfinger_base: Option<&str>,
    ) -> Vec<(String, Option<String>, Uuid)> {
        resolved_against_budget(state, text, webfinger_base, REMOTE_LOOKUP_BUDGET).await
    }

    /// [`resolved_against`] with the lookup batch run on `budget` instead
    /// of the module's [`REMOTE_LOOKUP_BUDGET`].
    async fn resolved_against_budget(
        state: &AppState,
        text: &str,
        webfinger_base: Option<&str>,
        budget: Duration,
    ) -> Vec<(String, Option<String>, Uuid)> {
        resolve_mentions_on_budget(state, text, webfinger_base, budget)
            .await
            .expect("resolution reads")
            .into_iter()
            .map(|(mention, user)| (mention.nickname, mention.domain, user.id))
            .collect()
    }

    /// The batch budget the mock-peer success tests run with: wall-clock
    /// headroom as a bounded failsafe, never a wait, so suite-mates
    /// saturating Postgres or the scheduler cannot spend the module's
    /// five seconds before the mock's answers land. The budget itself is
    /// exercised by the stalled-lookup test, which drives it on a
    /// hand-stepped virtual clock.
    const ROOMY_BUDGET: Duration = Duration::from_secs(120);

    /// The mock remote peer: its `http://` origin plus the per-endpoint
    /// request counts the tests assert on.
    #[derive(Clone, Debug)]
    struct Mock {
        /// The peer's origin.
        base: String,
        /// `WebFinger` requests it received.
        webfinger_hits: Arc<AtomicUsize>,
        /// Actor dereferences it received.
        actor_hits: Arc<AtomicUsize>,
        /// Whether `WebFinger` answers stall instead of replying.
        stalling: bool,
    }

    /// The origin of a port nothing listens on any more: every request to
    /// it fails to connect, which is how a dead remote domain reads here
    /// without waiting on any network or timer.
    async fn dead_base() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        base
    }

    /// Serves the mock peer on a loopback port; the returned task is
    /// aborted by the test once its assertions are in.
    async fn mock_peer(stalling: bool) -> (Mock, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock = Mock {
            base: format!("http://{}", listener.local_addr().unwrap()),
            webfinger_hits: Arc::new(AtomicUsize::new(0)),
            actor_hits: Arc::new(AtomicUsize::new(0)),
            stalling,
        };
        let app = Router::new()
            .route("/.well-known/webfinger", get(mock_webfinger))
            .route("/users/{nick}", get(mock_actor))
            .with_state(mock.clone());
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock peer stopped: {err}");
            }
        });
        (mock, server)
    }

    /// Counts the request and answers any `acct:` `WebFinger` with a JRD
    /// whose self link points at this peer's own `/users/{nick}`.
    async fn mock_webfinger(
        State(mock): State<Mock>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Response {
        let _hits = mock.webfinger_hits.fetch_add(1, Ordering::SeqCst);
        if mock.stalling {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let handle = params
            .get("resource")
            .and_then(|resource| resource.strip_prefix("acct:"))
            .and_then(|handle| handle.split_once('@').map(|(nick, _)| (nick, handle)))
            .unwrap_or_else(|| panic!("the mock was fingered for no acct: {params:?}"));
        let actor = format!("{}/users/{}", mock.base, handle.0);
        let jrd = json!({
            "subject": format!("acct:{}@mock.example", handle.0),
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": actor,
            }],
        });
        ([(CONTENT_TYPE, "application/jrd+json")], jrd.to_string()).into_response()
    }

    /// Counts the request and answers with a self-identifying `Person`
    /// document for `{base}/users/{nick}`.
    async fn mock_actor(State(mock): State<Mock>, Path(nick): Path<String>) -> Response {
        let _hits = mock.actor_hits.fetch_add(1, Ordering::SeqCst);
        let id = format!("{}/users/{nick}", mock.base);
        let document = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "type": "Person",
            "id": id,
            "preferredUsername": nick,
            "name": "Mock",
            "inbox": format!("{id}/inbox"),
            "publicKey": {
                "id": format!("{id}#main-key"),
                "owner": id,
                "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
            },
        });
        (
            [(CONTENT_TYPE, "application/activity+json")],
            document.to_string(),
        )
            .into_response()
    }

    /// The hits one mock counter has recorded.
    fn hits(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }

    /// One real Postgres round trip per manual clock step: its latency is
    /// what keeps the virtual clock from outrunning the resolve task's own
    /// real round trips (the scheduler test's pattern).
    async fn probe_users(pool: &PgPool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM users")
            .fetch_one(pool)
            .await
            .expect("the probe reads")
            .0
    }

    #[tokio::test]
    async fn a_bare_nickname_resolves_its_local_account() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = local_user(&pool, "alice").await;
        assert_eq!(
            resolved(&state, "hello @alice, how are you?").await,
            vec![("alice".to_owned(), None, alice.id)],
            "the reference keeps its written shape, the user is the row"
        );
    }

    #[tokio::test]
    async fn a_locally_written_mention_matches_case_insensitively() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = local_user(&pool, "alice").await;
        assert_eq!(
            resolved(&state, "@ALICE!").await,
            vec![("ALICE".to_owned(), None, alice.id)],
            "the citext column finds alice from any spelling"
        );
    }

    #[tokio::test]
    async fn an_own_domain_qualifier_resolves_the_local_account_without_network() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (mock, server) = mock_peer(false).await;
        let state = test_state(&pool);
        let alice = local_user(&pool, "alice").await;
        assert_eq!(
            resolved_against(
                &state,
                "@alice@test.example @ALICE@Test.Example",
                Some(&mock.base),
            )
            .await,
            vec![(
                "alice".to_owned(),
                Some("test.example".to_owned()),
                alice.id
            )],
            "nick@our-domain is the same account as nick in any case, and \
             extract_mentions' case-insensitive dedupe leaves one reference"
        );
        assert_eq!(
            (hits(&mock.webfinger_hits), hits(&mock.actor_hits)),
            (0, 0),
            "the own-domain form never reached the network"
        );
        assert!(
            resolved_against(&state, "@missing@test.example", Some(&mock.base))
                .await
                .is_empty(),
            "an unknown local qualified with OUR domain resolves to nothing"
        );
        assert_eq!(
            hits(&mock.webfinger_hits),
            0,
            "and even that is settled without a fetch"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_stored_remote_account_resolves_by_its_full_acct() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (mock, server) = mock_peer(false).await;
        let state = test_state(&pool);
        let bob = remote_user(&pool, "bob").await;
        assert_eq!(
            resolved_against(&state, "@bob@remote.example", Some(&mock.base)).await,
            vec![("bob".to_owned(), Some("remote.example".to_owned()), bob.id)],
            "the mirror already in users is found without any fetching"
        );
        assert_eq!(
            (hits(&mock.webfinger_hits), hits(&mock.actor_hits)),
            (0, 0),
            "a stored remote account never touches the mock"
        );
        server.abort();
    }

    #[tokio::test]
    async fn unknown_mentions_are_skipped() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let dead = dead_base().await;
        let state = test_state(&pool);
        let alice = local_user(&pool, "alice").await;
        assert!(
            resolved_against(&state, "@nobody says hi", None)
                .await
                .is_empty(),
            "an unknown local is no pair, and no fetch is tried"
        );
        assert!(
            resolved_against(&state, "@whoever@nowhere.example", Some(&dead))
                .await
                .is_empty(),
            "a remote whose lookup fails is skipped too"
        );
        assert_eq!(
            resolved_against(
                &state,
                "@nobody @alice @whoever@nowhere.example",
                Some(&dead)
            )
            .await,
            vec![("alice".to_owned(), None, alice.id)],
            "the known mention still comes back between the unknown ones"
        );
    }

    #[tokio::test]
    async fn resolution_keeps_text_order_and_dedupe() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        assert_eq!(
            resolved(&state, "@bob @alice and @bob again, plus @alice").await,
            vec![
                ("bob".to_owned(), None, bob.id),
                ("alice".to_owned(), None, alice.id),
            ],
            "first appearance order, each acct once"
        );
    }

    #[tokio::test]
    async fn a_bare_name_never_resolves_to_a_remote_mirror() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let _carol = remote_user(&pool, "carol").await;
        assert!(
            resolved(&state, "@carol @carol@test.example")
                .await
                .is_empty(),
            "carol exists only on remote.example, so neither the bare \
             mention nor one qualified with OUR domain names her"
        );
    }

    #[tokio::test]
    async fn plain_text_resolves_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        assert!(
            resolved(
                &state,
                "no mentions in an email a@b.c or a url \
                            https://x.example/@alice"
            )
            .await
            .is_empty(),
            "extract_mentions' rules carry over: nothing to look up"
        );
        assert!(resolved(&state, "").await.is_empty(), "empty text");
    }

    #[tokio::test]
    async fn an_unknown_remote_mention_is_fetched_mirrored_and_resolved() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (mock, server) = mock_peer(false).await;
        let state = test_state(&pool);

        let pairs = resolve_mentions_on_budget(
            &state,
            "hi @alice@mock.example!",
            Some(&mock.base),
            ROOMY_BUDGET,
        )
        .await
        .expect("resolution reads");
        server.abort();

        let (mention, user) = pairs.first().expect("the remote mention resolved");
        assert_eq!(mention.nickname, "alice", "written case kept");
        assert_eq!(
            mention.domain.as_deref(),
            Some("mock.example"),
            "the written domain kept"
        );
        assert_eq!(
            user.nickname,
            format!("alice@{}", mock.base.trim_start_matches("http://")),
            "mirrored under the actor's own handle, port included (decision 7)"
        );
        assert_eq!(
            user.ap_id,
            format!("{}/users/alice", mock.base),
            "the actor id the mock served"
        );
        assert!(!user.local.get(), "and a remote mirror, not a local row");
        let mirrored = get_by_id(&pool, user.id)
            .await
            .unwrap()
            .expect("the resolved user is a stored row");
        assert_eq!(mirrored.ap_id, user.ap_id, "the pair is the mirror row");
        assert_eq!(
            (hits(&mock.webfinger_hits), hits(&mock.actor_hits)),
            (1, 1),
            "one WebFinger resolve and one actor dereference"
        );
    }

    #[tokio::test]
    async fn duplicate_remote_mentions_resolve_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (mock, server) = mock_peer(false).await;
        let state = test_state(&pool);

        let found = resolved_against_budget(
            &state,
            "@bob@mock.example @BOB@MOCK.Example and @bob@mock.example again",
            Some(&mock.base),
            ROOMY_BUDGET,
        )
        .await;
        server.abort();

        assert_eq!(
            found.len(),
            1,
            "extract_mentions' case-insensitive dedupe leaves one reference"
        );
        assert_eq!(
            (hits(&mock.webfinger_hits), hits(&mock.actor_hits)),
            (1, 1),
            "and one lookup for the repeated mention"
        );
    }

    #[tokio::test]
    async fn the_lookup_cap_binds_one_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (mock, server) = mock_peer(false).await;
        let mut config = Config::for_tests();
        config.max_remote_mention_lookups = 2;
        let state = test_state_with_config(&pool, config);

        let found = resolved_against_budget(
            &state,
            "@one@mock.example @two@mock.example @three@mock.example",
            Some(&mock.base),
            ROOMY_BUDGET,
        )
        .await;
        server.abort();

        let nicks: Vec<&str> = found.iter().map(|(nick, _, _)| nick.as_str()).collect();
        assert_eq!(
            nicks,
            vec!["one", "two"],
            "the first cap-many mentions resolve, in text order, and the \
             one past the cap does not"
        );
        assert_eq!(
            found[0].1.as_deref(),
            Some("mock.example"),
            "the resolved pair keeps the written domain"
        );
        assert_ne!(found[0].2, found[1].2, "one, two are distinct mirrors");
        assert_eq!(
            (hits(&mock.webfinger_hits), hits(&mock.actor_hits)),
            (2, 2),
            "exactly the capped number of lookups ran"
        );
    }

    #[tokio::test]
    async fn a_refused_lookup_leaves_the_mention_plain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let dead = dead_base().await;

        assert!(
            resolved_against(&state, "@carol@mock.example", Some(&dead))
                .await
                .is_empty(),
            "a handle whose WebFinger cannot even connect resolves to nothing"
        );
    }

    #[tokio::test]
    async fn the_shared_budget_abandons_a_stalled_lookup() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (mock, server) = mock_peer(true).await;
        let state = test_state(&pool);

        // The paused clock is driven by hand (the scheduler test's
        // pattern): the inhibitor keeps tokio from auto-advancing over the
        // real Postgres round trips, each 20 ms step moves the virtual
        // clock once, and the step's own real query is what hands the
        // resolve task's round trips the time to land. The stalling mock
        // never answers, so the shared five-second budget fires after
        // roughly 250 steps — well past the real-database phase.
        tokio::time::pause();
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let inhibitor = tokio::task::spawn_blocking(move || {
            let _held = hold_rx.recv();
        });
        let base = mock.base.clone();
        let resolving = tokio::spawn(async move {
            resolve_mentions_with_base(&state, "@slowpoke@mock.example", Some(&base)).await
        });
        let step = Duration::from_millis(20);
        let mut steps = 0_usize;
        while !resolving.is_finished() {
            steps += 1;
            assert!(steps < 1200, "the five-second budget never elapsed");
            tokio::time::advance(step).await;
            let _users = probe_users(&pool).await;
        }
        let found = resolving
            .await
            .expect("the resolve task ran")
            .expect("resolution reads");
        drop(hold_tx);
        inhibitor
            .await
            .expect("the clock inhibitor exits with its channel");
        server.abort();

        assert!(found.is_empty(), "the stalled mention stays plain text");
        assert_eq!(
            hits(&mock.webfinger_hits),
            1,
            "the lookup did start before the budget ran out"
        );
        assert!(
            steps > 250,
            "the whole shared budget was spent, not one short per-lookup \
             wait: {steps} steps of 20 ms"
        );
    }
}
