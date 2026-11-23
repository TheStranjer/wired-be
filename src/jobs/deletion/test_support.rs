//! Fixtures the account-deletion job tests share: test state, local and
//! mirrored accounts, the data a sweep clears and the counting helpers.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use super::{handle_delete, handle_delete_remote, job_args};
use crate::{
    config::{Config, TEST_BASE_URL},
    jobs::{AttemptInfo, JobError, scheduled_statuses},
    models::{
        backup, bookmark,
        emoji_reaction::{self, NewReaction},
        favourite::{self, Favourite},
        follow,
        list::{self, List, NewList},
        notification::{self, NewNotification, Notification},
        oauth_app::{self, NewOauthApp},
        oauth_token::{self, NewToken},
        scheduled_status::{self, NewScheduledStatus, ScheduledStatus},
        status::{self, NewStatus, Status},
        user::{self, Flag, NewUser, User, insert, local_ap_id},
        web_push_subscription::{self, NewSubscription},
    },
    state::AppState,
    uploads::UploaderBackend,
};

/// How many statuses the batch test inserts: one full 100-row pass and
/// a remainder, so the loop must run at least twice.
pub(super) const BATCH_OVERSHOOT: i64 = 120;

/// Test state over `pool` with the fixed test config.
pub(super) fn app_state(pool: &PgPool) -> AppState {
    AppState::for_tests(pool.clone(), Config::for_tests())
}

/// Test state over a pool that cannot answer: closed before it ever
/// connected, which is how a handler sees a database that went away.
pub(super) async fn broken_state() -> AppState {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused")
        .expect("a lazy pool needs no database");
    pool.close().await;
    app_state(&pool)
}

/// Inserts a local user named `nickname` carrying the personal data the
/// purge step must blank: an email, a password hash and a display name.
pub(super) async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser {
            email: Some(format!("{nickname}@example.org")),
            password_hash: Some("hash-of-a-secret".to_owned()),
            name: nickname.to_owned(),
            ..NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
        },
    )
    .await
    .expect("the local fixture inserts")
}

/// Posts one local status for `author`.
pub(super) async fn post(pool: &PgPool, author: &User, text: &str) -> Status {
    status::insert(pool, TEST_BASE_URL, &NewStatus::local(author, text))
        .await
        .expect("the status fixture inserts")
}

/// Posts `count` statuses for `author` in one multi-row statement, for
/// the batch-boundary fixture.
pub(super) async fn post_many(pool: &PgPool, author: &User, count: i64) {
    let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "INSERT INTO statuses (id, uri, url, user_id, visibility, conversation_id, local) ",
    );
    let _values: &mut _ = builder.push_values(0..count, |mut row, _| {
        let id = Uuid::now_v7();
        let _fields: &mut _ = row
            .push_bind(id)
            .push_bind(format!("{TEST_BASE_URL}/objects/{id}"))
            .push_bind(format!("{TEST_BASE_URL}/notice/{id}"))
            .push_bind(author.id)
            .push_bind("public")
            .push_bind(id)
            .push_bind(true);
    });
    let inserted = builder
        .build()
        .execute(pool)
        .await
        .expect("the batch status inserts");
    assert_eq!(
        inserted.rows_affected(),
        u64::try_from(count).unwrap_or(u64::MAX),
        "every batch row lands"
    );
}

/// `follower` follows `target` through the model's normal path.
pub(super) async fn follows(pool: &PgPool, follower: &User, target: &User) {
    let _granted = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the follow fixture is recorded");
}

/// `who` favourites `status_id`, answering the created row.
pub(super) async fn likes(pool: &PgPool, who: &User, status_id: Uuid) -> Favourite {
    favourite::favourite(pool, TEST_BASE_URL, who.id, status_id)
        .await
        .expect("the favourite write")
        .expect("the favourite is created")
}

/// `who` bookmarks `status_id`.
pub(super) async fn marks(pool: &PgPool, who: &User, status_id: Uuid) {
    let _added = bookmark::add(pool, who.id, status_id, None)
        .await
        .expect("the bookmark write succeeds");
}

/// A `follow` notification of `recipient` caused by `actor`, answering
/// the created row.
pub(super) async fn notify(pool: &PgPool, recipient: &User, actor: &User) -> Notification {
    notification::create(
        pool,
        &NewNotification {
            user_id: recipient.id,
            from_account_id: actor.id,
            notification_type: "follow",
            status_id: None,
            emoji: None,
            report_id: None,
            chat_message_id: None,
        },
    )
    .await
    .expect("the notification fixture inserts")
}

/// A list owned by `owner` holding `member` as its only member.
pub(super) async fn list_of(pool: &PgPool, owner: &User, member: &User) -> List {
    let created = list::create(pool, owner.id, &NewList::default())
        .await
        .expect("the list fixture creates");
    let _added = list::add_members(pool, created.id, &[member.id])
        .await
        .expect("the membership fixture adds");
    created
}

/// Inserts a media row owned by `owner`, attached to `status_id` when
/// one is named, and answers its id.
pub(super) async fn media_row(pool: &PgPool, owner: &User, status_id: Option<Uuid>) -> Uuid {
    let id = Uuid::now_v7();
    let written = sqlx::query(
        "INSERT INTO media_attachments (id, user_id, status_id, url, content_type)
         VALUES ($1, $2, $3, $4, 'image/png')",
    )
    .bind(id)
    .bind(owner.id)
    .bind(status_id)
    .bind(format!("{TEST_BASE_URL}/media/{id}.png"))
    .execute(pool)
    .await
    .expect("the media fixture inserts");
    assert_eq!(written.rows_affected(), 1, "one row lands");
    id
}

/// The count of live (not tombstoned) statuses authored by `author_id`.
pub(super) async fn live_statuses(pool: &PgPool, author_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM statuses WHERE user_id = $1 AND deleted_at IS NULL",
    )
    .bind(author_id)
    .fetch_one(pool)
    .await
    .expect("the live statuses count")
}

/// The count of rows in `column`-keyed table `table` naming `id`. The
/// table names are test constants, never user input, so the dynamic
/// statement is asserted safe (the same bypass `src/test_db.rs`
/// documents).
pub(super) async fn count_where(pool: &PgPool, table: &str, column: &str, id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {table} WHERE {column} = $1"
    )))
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|_| panic!("{table}.{column} rows count"))
}

/// The `deleted_at` stamp of status `id`.
pub(super) async fn status_deleted_at(pool: &PgPool, id: Uuid) -> Option<DateTime<Utc>> {
    sqlx::query_scalar::<_, Option<DateTime<Utc>>>("SELECT deleted_at FROM statuses WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the status reads")
}

/// Runs [`handle_delete`] to completion on `state` for `user_id`.
pub(super) async fn run(state: &AppState, user_id: Uuid) -> Result<(), JobError> {
    let attempt = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };
    handle_delete(state.clone(), attempt, job_args(user_id)).await
}

/// Whether `follower` → `target` currently has a follow row.
pub(super) async fn has_follow(pool: &PgPool, follower: &User, target: &User) -> bool {
    follow::get(pool, follower.id, target.id)
        .await
        .unwrap()
        .is_some()
}

/// Inserts a mirror of `nickname` on `peer.example`: a remote account,
/// which owns none of this instance's local data.
pub(super) async fn remote_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@peer.example"),
            ap_id: format!("https://peer.example/users/{nickname}"),
            local: Flag::new(false),
            domain: Some("peer.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// Posts one status authored by the remote `author`, at the `uri` the
/// peer named (decision 6).
pub(super) async fn remote_post(pool: &PgPool, author: &User, text: &str) -> Status {
    let id = Uuid::now_v7();
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            user_id: author.id,
            uri: Some(format!("https://peer.example/objects/{id}")),
            content: text.to_owned(),
            raw_text: text.to_owned(),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the remote status fixture inserts")
}

/// `follower` follows `target` *and* the request is granted through the
/// model's own [`follow::accept`], the only way a follow of a remote
/// account ever counts: the answer a peer's `Accept` activity would
/// bring, applied to the fixture directly.
pub(super) async fn accepted_follow(pool: &PgPool, follower: &User, target: &User) {
    let requested = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the follow fixture is recorded");
    let _granted = follow::accept(pool, requested.id)
        .await
        .expect("the fixture grants the request");
}

/// `who` reacts to `status_id` with `emoji`.
pub(super) async fn reacts(pool: &PgPool, who: &User, status_id: Uuid, emoji: &str) {
    let _stored = emoji_reaction::react(
        pool,
        TEST_BASE_URL,
        &NewReaction {
            user_id: who.id,
            status_id,
            emoji,
            url: None,
        },
    )
    .await
    .expect("the reaction fixture writes");
}

/// User `id` as it stands now, for reading the kept row and its counters
/// back.
pub(super) async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the user exists")
}

/// Runs [`handle_delete_remote`] to completion on `state` for `user_id`.
pub(super) async fn run_remote(state: &AppState, user_id: Uuid) -> Result<(), JobError> {
    let attempt = AttemptInfo {
        attempts: 1,
        max_attempts: 5,
    };
    handle_delete_remote(state.clone(), attempt, job_args(user_id)).await
}

/// The remote-deletion fixture, as the inbound `Delete` hands it over: a
/// departed mirror past a batch boundary of statuses, followed and
/// following in accepted relationships, with likes, marks and reactions
/// of its own and a reaction of another account on its post, a
/// notification it caused and one of someone else's, its row already
/// marked deleted by the handler.
pub(super) struct Departed {
    /// The mirror the sweep clears.
    pub(super) alice: User,
    /// A local account that follows alice and carol.
    pub(super) bob: User,
    /// A local account alice follows.
    pub(super) carol: User,
    /// One of alice's posts, named by the assertions.
    pub(super) alices_post: Status,
    /// Bob's own post, which alice liked.
    pub(super) bobs_post: Status,
    /// A notification of bob's about carol, nobody else's to clear.
    pub(super) bobs_notification: Notification,
}

/// Builds the [`Departed`] fixture.
pub(super) async fn departed_mirror(pool: &PgPool) -> Departed {
    let alice = remote_user(pool, "alice").await;
    let bob = local_user(pool, "bob").await;
    let carol = local_user(pool, "carol").await;
    let alices_post = remote_post(pool, &alice, "alice posts").await;
    post_many(pool, &alice, BATCH_OVERSHOOT).await;
    let bobs_post = post(pool, &bob, "bob posts").await;
    accepted_follow(pool, &bob, &alice).await;
    accepted_follow(pool, &alice, &carol).await;
    accepted_follow(pool, &bob, &carol).await;
    let _alices_like = likes(pool, &alice, bobs_post.id).await;
    marks(pool, &alice, bobs_post.id).await;
    reacts(pool, &alice, bobs_post.id, "\u{1f44d}").await;
    reacts(pool, &bob, alices_post.id, "\u{1f389}").await;
    let _to_bob = notify(pool, &bob, &alice).await;
    let bobs_notification = notify(pool, &carol, &bob).await;
    user::mark_deleted(pool, alice.id)
        .await
        .expect("the delete handler marks the mirror first");
    Departed {
        alice,
        bob,
        carol,
        alices_post,
        bobs_post,
        bobs_notification,
    }
}

/// Test state whose uploads live in a fresh temporary directory, with the
/// directory's guard: the sweep deletes from that same directory, so the
/// guard must outlive the run.
pub(super) fn uploads_state(pool: &PgPool) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::TempDir::new().expect("a temp uploads dir");
    let mut state = app_state(pool);
    let mut config = Config::for_tests();
    config.uploads_dir = dir.path().to_path_buf();
    state.uploader = UploaderBackend::from(&config);
    (dir, state)
}

/// A schedule of `owner` due tomorrow, with its publish job queued under
/// the `sched:<id>` key, as the create endpoint leaves it.
pub(super) async fn scheduled_with_job(state: &AppState, owner: &User) -> ScheduledStatus {
    let row = scheduled_status::insert(
        &state.db,
        &NewScheduledStatus {
            user_id: owner.id,
            scheduled_at: Utc::now() + chrono::TimeDelta::days(1),
            params: serde_json::json!({ "status": "later" }),
        },
    )
    .await
    .expect("the schedule fixture inserts");
    scheduled_statuses::schedule(state, &row).await;
    row
}

/// How many `available` jobs carry the unique key `key`.
pub(super) async fn available_jobs_keyed(pool: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM jobs WHERE state = 'available' AND unique_key = $1")
        .bind(key)
        .fetch_one(pool)
        .await
        .expect("the job count reads")
}

/// A processed backup of `owner` whose archive of `bytes` is stored in
/// `state`'s uploader; answers the row and the key.
pub(super) async fn processed_backup(
    state: &AppState,
    owner: &User,
    bytes: &'static [u8],
) -> (backup::Backup, String) {
    let row = backup::create(&state.db, owner)
        .await
        .expect("the backup fixture creates");
    let key = format!("backups/{}/{}", owner.id, row.id);
    state
        .uploader
        .put(&key, Bytes::from_static(bytes), "application/zip")
        .await
        .expect("the archive fixture stores");
    let size = i64::try_from(bytes.len()).expect("a fixture's length is an i64");
    let marked = backup::mark_processed(&state.db, row.id, size, &key)
        .await
        .expect("the mark runs")
        .expect("the requested row is there");
    (marked, key)
}

/// Registers a push subscription for `owner` under a fresh app and token.
pub(super) async fn push_subscription(pool: &PgPool, owner: &User, endpoint: &str) {
    let app = oauth_app::create(
        pool,
        &NewOauthApp {
            client_name: "Push Tester".to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: "push".to_owned(),
            website: None,
            user_id: Some(owner.id),
        },
    )
    .await
    .expect("the app fixture inserts");
    let token = oauth_token::create_token(
        pool,
        &NewToken {
            user_id: Some(owner.id),
            app_id: app.id,
            scopes: "push".to_owned(),
            ttl: None,
        },
    )
    .await
    .expect("the token fixture inserts");
    let _stored = web_push_subscription::create(
        pool,
        &NewSubscription {
            user_id: owner.id,
            token_id: token.id,
            endpoint: endpoint.to_owned(),
            key_p256dh: "BFraw-key".to_owned(),
            key_auth: "auth_secret".to_owned(),
            alerts: serde_json::json!({ "follow": true }),
            policy: "all".to_owned(),
        },
    )
    .await
    .expect("the subscription fixture registers");
}
