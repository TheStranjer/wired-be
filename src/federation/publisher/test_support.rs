//! The fixtures the publisher children's tests share: the state over the
//! test database, the publishing actor and remote mirrors, the follow and
//! policy rows fan-out reads, the fixture activity and the readers of the
//! jobs a publish queued. Test-only.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::{PublishOpts, publish};
use crate::{
    config::{Config, TEST_BASE_URL},
    federation::delivery::KIND_DELIVER,
    models::{
        follow::{self, FollowState},
        user::{NewUser, User, insert, local_ap_id},
    },
    state::AppState,
};

/// The full spelling of the public address.
pub(super) const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// The activity id every fixture carries.
pub(super) const ACTIVITY_ID: &str = "https://test.example/activities/1";

/// The state under test over `pool` and `config`.
pub(super) fn state_with(pool: &PgPool, config: Config) -> AppState {
    AppState::for_tests(pool.clone(), config)
}

/// The default test state: federation on, the standard test config.
pub(super) fn test_state(pool: &PgPool) -> AppState {
    state_with(pool, Config::for_tests())
}

/// The publishing actor: local `alice` on the test origin, with the
/// inbox and followers-collection URL a real actor row carries.
pub(super) async fn alice(pool: &PgPool) -> User {
    let ap_id = local_ap_id(TEST_BASE_URL, "alice");
    insert(
        pool,
        &NewUser {
            inbox: Some(format!("{ap_id}/inbox")),
            followers_url: Some(format!("{ap_id}/followers")),
            ..NewUser::local("alice", &ap_id)
        },
    )
    .await
    .unwrap()
}

/// Inserts a remote mirror `nickname@host` with the given inbox
/// endpoints, for the rows fan-out resolves addresses to.
pub(super) async fn mirror(
    pool: &PgPool,
    nickname: &str,
    host: &str,
    inbox: &str,
    shared: Option<&str>,
) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@{host}"),
            ap_id: format!("https://{host}/users/{nickname}"),
            domain: Some(host.to_owned()),
            inbox: Some(inbox.to_owned()),
            shared_inbox: shared.map(str::to_owned),
            ..NewUser::default()
        },
    )
    .await
    .unwrap()
}

/// Grants `follower` an accepted follow of `target` (a remote follower
/// of a local, unlocked account is granted on the spot).
pub(super) async fn followed(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .unwrap();
    assert_eq!(
        row.state,
        FollowState::Accepted,
        "a follow of an unlocked local account is granted at once"
    );
}

/// Inserts one instance-policy `reject` row.
pub(super) async fn policy_reject(pool: &PgPool, domain: &str) {
    let inserted =
        sqlx::query("INSERT INTO instance_policy_domains (domain, action) VALUES ($1, $2)")
            .bind(domain)
            .bind("reject")
            .execute(pool)
            .await
            .unwrap();
    assert_eq!(inserted.rows_affected(), 1, "the policy row is there");
}

/// Deactivates a stored mirror the way a suspend or deactivation would.
pub(super) async fn deactivate(pool: &PgPool, user_id: Uuid) {
    let updated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(
        updated.rows_affected(),
        1,
        "the mirror is there to deactivate"
    );
}

/// The fixture `Create` note by `actor`, addressed to the public and to
/// `actor`'s followers collection.
pub(super) fn public_activity(actor: &User) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": ACTIVITY_ID,
        "type": "Create",
        "actor": actor.ap_id,
        "to": [PUBLIC],
        "cc": [actor.followers_url],
    })
}

/// Runs [`publish`] and fails the test on any error.
pub(super) async fn publish_ok(
    state: &AppState,
    actor: &User,
    activity: &Value,
    opts: PublishOpts,
) -> usize {
    publish(state, actor, activity, opts).await.unwrap()
}

/// [`publish_ok`] with the options every plain fan-out test uses.
pub(super) async fn publish_default(state: &AppState, actor: &User, activity: &Value) -> usize {
    publish_ok(state, actor, activity, PublishOpts::default()).await
}

/// The inboxes the `federator.deliver` jobs queued so far ask for,
/// sorted, so fan-out sets compare directly.
pub(super) async fn delivered_inboxes(pool: &PgPool) -> Vec<String> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT args->>'inbox' FROM jobs WHERE kind = $1 ORDER BY 1",
    )
    .bind(KIND_DELIVER)
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter().map(|(inbox,)| inbox).collect()
}

/// How many jobs the database holds, of any kind.
pub(super) async fn job_count(pool: &PgPool) -> i64 {
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM jobs")
        .fetch_one(pool)
        .await
        .unwrap();
    count
}
