//! Test-only fixtures shared by the module's children: the state and users
//! the database tests build on, the unfederated config, the status, poll,
//! report and chat rows the hooks announce, and the queue readers every test
//! asserts on.
//!
//! A fixture lives here as soon as a second child needs it; the ones that
//! belong to a single arm stand in that child's own tests.

use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::{Config, TEST_BASE_URL, Toggle},
    federation::publisher,
    http_client::HttpClient,
    models::{
        chat::{self, ChatMessageReference, NewChatMessage},
        follow,
        poll::{self, Poll as PollRow, PollParams},
        report::{self, Report as ReportRow},
        status::{self, NewStatus, Status, Visibility},
        user::{Flag, NewUser, User, insert, local_ap_id},
    },
    state::AppState,
    uploads::UploaderBackend,
};
use serde_json::Value;

/// The test state over `pool` with the default (federating) config.
pub(super) fn app_state(pool: &PgPool) -> AppState {
    state_with(pool, Config::for_tests())
}

/// The test state over `pool` with an injected config.
pub(super) fn state_with(pool: &PgPool, config: Config) -> AppState {
    let config = Arc::new(config);
    AppState {
        db: pool.clone(),
        hub: crate::streaming::hub::Hub::new(pool),
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
        mrf_config_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
        mailer: Arc::new(crate::mailer::MemoryTransport::new()),
    }
}

/// The config with federation switched off.
pub(super) fn unfederated() -> Config {
    let mut config = Config::for_tests();
    config.federation_enabled = Toggle::new(false);
    config
}

/// A registered-looking local actor: keys, inbox, followers collection,
/// exactly the row `create_local_user` writes.
pub(super) async fn local(pool: &PgPool, nickname: &str) -> User {
    let ap_id = local_ap_id(TEST_BASE_URL, nickname);
    insert(
        pool,
        &NewUser {
            private_key: Some("PEM".to_owned()),
            public_key: Some("PUB".to_owned()),
            inbox: Some(format!("{ap_id}/inbox")),
            followers_url: Some(format!("{ap_id}/followers")),
            ..NewUser::local(nickname, &ap_id)
        },
    )
    .await
    .expect("the local fixture inserts")
}

/// A remote mirror `nickname@remote.example`, shared inbox included.
pub(super) async fn remote(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@remote.example"),
            ap_id: format!("https://remote.example/users/{nickname}"),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            inbox: Some(format!("https://remote.example/users/{nickname}/inbox")),
            shared_inbox: Some("https://remote.example/inbox".to_owned()),
            public_key: Some("PUB".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// `follower` follows `target` with an accepted relationship: a local
/// target grants on the spot, a remote one is answered the way a peer's
/// `Accept` would.
pub(super) async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the follow fixture writes");
    if row.state != follow::FollowState::Accepted {
        let _granted = follow::accept(pool, row.id)
            .await
            .expect("the fixture grants it");
    }
}

/// The queued `federator.publish` jobs: `(args, unique_key)` pairs.
pub(super) async fn publish_jobs(pool: &PgPool) -> Vec<(Value, Option<String>)> {
    sqlx::query_as::<_, (Value, Option<String>)>(
        "SELECT args, unique_key FROM jobs WHERE kind = $1 ORDER BY inserted_at",
    )
    .bind(publisher::KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the jobs read")
}

/// How many jobs the database holds, of any kind.
pub(super) async fn job_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
        .fetch_one(pool)
        .await
        .expect("the job count")
}

/// The WBE-181 fixtures of the disabled test: a REMOTE question with a
/// two-option [`multiple`](PollRow::multiple) poll — answered with its
/// row and the option ids one ballot marks — beside the sender's
/// reference to a chat message of the `alice → bob` pair.
pub(super) async fn remote_ballot_and_chat(
    pool: &PgPool,
    alice: &User,
    bob: &User,
) -> (PollRow, Vec<Uuid>, ChatMessageReference) {
    let asked = remote_status(pool, bob, "question-disabled", Visibility::Public).await;
    let stand = {
        let mut conn = pool.acquire().await.expect("a slot connection");
        poll::create(
            &mut conn,
            asked.id,
            &PollParams {
                options: vec!["tea".to_owned(), "coffee".to_owned()],
                multiple: true,
                ..PollParams::default()
            },
        )
        .await
        .expect("the poll creates")
    };
    let marks: Vec<Uuid> = poll::options(pool, stand.id)
        .await
        .expect("the options read")
        .iter()
        .map(|option| option.id)
        .collect();
    let ballot = {
        let mut tx = pool.begin().await.expect("a transaction begins");
        let (_, reference, _) = chat::add_message(
            &mut tx,
            TEST_BASE_URL,
            alice,
            bob,
            &NewChatMessage::text("unheard"),
        )
        .await
        .expect("the chat message writes");
        tx.commit().await.expect("the transaction commits");
        reference
    };
    (stand, marks, ballot)
}

// ───────────────  the interaction hooks  ───────────────

/// A status mirrored from `remote.example`, authored by `author` and
/// seen as `visibility`.
pub(super) async fn remote_status(
    pool: &PgPool,
    author: &User,
    n: &str,
    visibility: Visibility,
) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            uri: Some(format!("https://remote.example/objects/{n}")),
            user_id: author.id,
            visibility,
            ..NewStatus::default()
        },
    )
    .await
    .expect("the remote status fixture inserts")
}

/// A local public post by `author`.
pub(super) async fn local_status(pool: &PgPool, author: &User, text: &str) -> Status {
    status::insert(pool, TEST_BASE_URL, &NewStatus::local(author, text))
        .await
        .expect("the local status fixture inserts")
}

/// The first attempt a background handler runs on.
pub(super) const fn first_attempt() -> crate::jobs::AttemptInfo {
    crate::jobs::AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    }
}

/// Runs every queued `federator.publish` job through the publisher's
/// own handler, so the fan-out expands into `federator.deliver` jobs
/// the test can name.
pub(super) async fn run_publishes(state: &AppState, pool: &PgPool) {
    let rows =
        sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at")
            .bind(publisher::KIND_PUBLISH)
            .fetch_all(pool)
            .await
            .expect("the publish jobs read");
    for (args,) in rows {
        publisher::handle(state.clone(), first_attempt(), args)
            .await
            .expect("the fan-out job completes");
    }
}

/// The inboxes the `federator.deliver` jobs queued so far ask for,
/// sorted, so fan-out sets compare directly.
pub(super) async fn delivered_inboxes(pool: &PgPool) -> Vec<String> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT args->>'inbox' FROM jobs WHERE kind = $1 ORDER BY 1",
    )
    .bind(crate::federation::delivery::KIND_DELIVER)
    .fetch_all(pool)
    .await
    .expect("the delivery jobs read");
    rows.into_iter().map(|(inbox,)| inbox).collect()
}

/// The favourite document the queue holds: exactly one publish job,
/// unwrapped for the assertions every interaction test shares.
pub(super) async fn the_single_activity(pool: &PgPool) -> Value {
    let jobs = publish_jobs(pool).await;
    assert_eq!(jobs.len(), 1, "the write is announced once");
    assert!(jobs[0].1.is_none(), "interactions never debounce");
    jobs[0].0["activity"].clone()
}

/// The queued publish job whose activity carries `type_name` — the
/// move test reads the pair out of one queue without depending on the
/// two inserts' order.
pub(super) fn queued_activity(jobs: &[(Value, Option<String>)], type_name: &str) -> Value {
    jobs.iter()
        .find(|(args, _)| args["activity"]["type"].as_str() == Some(type_name))
        .map_or_else(
            || panic!("a {type_name} job is queued"),
            |(args, _)| args["activity"].clone(),
        )
}

/// A local account files `new`'s report about `target`, asserting the
/// row came back: the fixture shape every flag test starts from.
pub(super) async fn filed(
    pool: &PgPool,
    reporter: &User,
    target: &User,
    new: &report::NewReport,
) -> ReportRow {
    report::create(pool, TEST_BASE_URL, reporter, target, new)
        .await
        .expect("the report files")
}

/// The full body of a filing with nothing but a comment and the
/// forward answer varied: the flag fixtures' shared shape.
pub(super) fn filing(comment: &str, forwarded: bool, status_ids: Vec<Uuid>) -> report::NewReport {
    report::NewReport {
        comment: comment.to_owned(),
        category: report::Category::Spam,
        forwarded,
        rule_ids: Vec::new(),
        status_ids,
    }
}

/// A local post by `author` of `text`, seen as `visibility`.
pub(super) async fn local_post(
    pool: &PgPool,
    author: &User,
    text: &str,
    visibility: Visibility,
) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility,
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the local post fixture inserts")
}

/// The `{BASE_URL}/activities/{uuid}` shape an answer or a withdrawal
/// must carry as its own id.
pub(super) fn assert_fresh_activity_id(id: &str) {
    assert!(
        id.starts_with(&format!("{TEST_BASE_URL}/activities/")),
        "the activity carries this instance's own id shape: {id}"
    );
    let _uuid =
        Uuid::parse_str(id.rsplit('/').next().unwrap_or_default()).expect("the id ends in a uuid");
}

/// The args of every queued `federator.deliver` job, in insertion order.
pub(super) async fn delivery_jobs(pool: &PgPool) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id")
        .bind(crate::federation::delivery::KIND_DELIVER)
        .fetch_all(pool)
        .await
        .expect("the delivery jobs read")
        .into_iter()
        .map(|(args,)| args)
        .collect()
}
