//! The fixtures the `notify` module's unit tests share: the test
//! [`AppState`], local and remote accounts, posts, follows, polls,
//! reactions and push subscriptions, and the row reads every assertion
//! compares. Test-only.
//!
//! [`AppState`]: crate::state::AppState

use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    models::{
        emoji_reaction::{self, NewReaction},
        follow::{self, Follow},
        notification,
        oauth_app::{self, NewOauthApp},
        oauth_token::{self, NewToken},
        poll::{self, Poll as PollRow},
        status::{self, NewStatus, Status as StatusRow},
        user::{self, NewUser, User},
        web_push_subscription::{self, NewSubscription},
    },
    services::notify::on_reaction_changed,
    state::AppState,
};

// The state the job handlers are tested with, over a slot's pool.
pub(super) use crate::test_support::app_state;
// A local user named `nickname`.
pub(super) use crate::test_fixtures::local_user as local;

/// The fixed test config's instance origin.
pub(super) const BASE: &str = crate::config::TEST_BASE_URL;

/// A local user named `nickname` who answers follows with requests.
pub(super) async fn locked(pool: &PgPool, nickname: &str) -> User {
    user::insert(
        pool,
        &NewUser {
            locked: user::Flag::new(true),
            ..NewUser::local(nickname, &user::local_ap_id(BASE, nickname))
        },
    )
    .await
    .expect("locked user fixture")
}

/// The mirror of the remote actor `nickname` on `host`.
pub(super) async fn remote(pool: &PgPool, nickname: &str, host: &str) -> User {
    let ap_id = format!("https://{host}/users/{nickname}");
    user::insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@{host}"),
            ap_id,
            domain: Some(host.to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("remote user fixture")
}

/// A public post by `author`.
pub(super) async fn post(pool: &PgPool, author: &User, text: &str) -> StatusRow {
    status::insert(pool, BASE, &NewStatus::local(author, text))
        .await
        .expect("status fixture")
}

/// The recipient's stored notification rows, newest first.
pub(super) async fn rows(pool: &PgPool, recipient: Uuid) -> Vec<notification::Notification> {
    sqlx::query_as::<_, notification::Notification>(
        "SELECT * FROM notifications WHERE user_id = $1 ORDER BY id DESC",
    )
    .bind(recipient)
    .fetch_all(pool)
    .await
    .expect("the rows read")
}

/// The stored notifications of `recipient`, as `(type, from, status)`
/// triples: what every assertion below compares.
pub(super) async fn facts(pool: &PgPool, recipient: Uuid) -> Vec<(String, Uuid, Option<Uuid>)> {
    rows(pool, recipient)
        .await
        .into_iter()
        .map(|row| (row.event_type, row.from_account_id, row.status_id))
        .collect()
}

/// The relationship the follow endpoint leaves behind: `follower`
/// following `target`, straight through the model.
pub(super) async fn followed(pool: &PgPool, follower: &User, target: &User) -> Follow {
    follow::follow(pool, BASE, follower, target)
        .await
        .expect("the follow inserts")
}

/// A fresh `push`-scoped access token of `owner_id`, as a subscription
/// is keyed to one.
pub(super) async fn push_token(pool: &PgPool, owner_id: Uuid, name: &str) -> Uuid {
    let app = oauth_app::create(
        pool,
        &NewOauthApp {
            client_name: name.to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: "push".to_owned(),
            website: None,
            user_id: Some(owner_id),
        },
    )
    .await
    .expect("the push app fixture inserts");
    oauth_token::create_token(
        pool,
        &NewToken {
            user_id: Some(owner_id),
            app_id: app.id,
            scopes: "push".to_owned(),
            ttl: None,
        },
    )
    .await
    .expect("the push token fixture inserts")
    .id
}

/// A registered subscription of `owner`'s `token_id`, follow alert on.
pub(super) async fn register_push(pool: &PgPool, owner: &User, token_id: Uuid) -> Uuid {
    web_push_subscription::create(
        pool,
        &NewSubscription {
            user_id: owner.id,
            token_id,
            endpoint: format!("https://push.example/{token_id}"),
            key_p256dh: "BFpush-key".to_owned(),
            key_auth: "push-auth".to_owned(),
            alerts: json!({"follow": true}),
            policy: "all".to_owned(),
        },
    )
    .await
    .expect("the push subscription registers")
    .id
}

/// The queued `push.deliver` jobs as `(queue, args, max_attempts)`,
/// ordered by the subscription each names.
pub(super) async fn push_jobs(pool: &PgPool) -> Vec<(String, serde_json::Value, i32)> {
    sqlx::query_as::<_, (String, serde_json::Value, i32)>(
        "SELECT queue, args, max_attempts FROM jobs WHERE kind = $1 \
          ORDER BY args->>'subscription_id'",
    )
    .bind(crate::jobs::web_push::KIND_DELIVER)
    .fetch_all(pool)
    .await
    .expect("the jobs read")
}

/// The poll fixture: a two-option multiple-choice poll on `posted`,
/// closing `mins` minutes from now (negative: already closed).
pub(super) async fn poll_on(pool: &PgPool, posted: &StatusRow, mins: i64) -> PollRow {
    poll::create(
        &mut pool.acquire().await.expect("a slot connection"),
        posted.id,
        &poll::PollParams {
            options: vec!["tea".to_owned(), "coffee".to_owned()],
            expires_at: Some(Utc::now() + chrono::TimeDelta::minutes(mins)),
            multiple: true,
            ..poll::PollParams::default()
        },
    )
    .await
    .expect("the poll fixture creates")
}

/// `voter` marks the first option of `stand`, straight through the
/// vote write the endpoint uses.
pub(super) async fn ballot(pool: &PgPool, stand: &PollRow, voter: &User) {
    let first = poll::options(pool, stand.id)
        .await
        .expect("the options read")
        .into_iter()
        .next()
        .expect("the fixture poll has an option")
        .id;
    assert!(
        poll::cast_vote(pool, stand.id, voter.id, &[first])
            .await
            .expect("the ballot writes"),
        "the fixture pair had not voted yet"
    );
}

/// `reactor` reacts to `target` with `emoji` through the reaction
/// write, and the hook lands: the pair the `PUT` route runs.
pub(super) async fn reacted(
    state: &AppState,
    pool: &PgPool,
    reactor: &User,
    target: &StatusRow,
    emoji: &str,
) {
    let stored = emoji_reaction::react(
        pool,
        BASE,
        &NewReaction {
            user_id: reactor.id,
            status_id: target.id,
            emoji,
            url: None,
        },
    )
    .await
    .expect("the reaction writes");
    on_reaction_changed(state, &stored, true).await;
}
