//! The write-section consts, fixture and helpers the write submodules draw
//! on: the bodies and queue names the account writes answer with, the seeded
//! accounts a write weighs, the verbs and paths of the five routes, and the
//! row, queue, log and mail readers the assertions check.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::Config,
    models::{
        report::{self, Category, NewReport},
        user::{Flag, NewUser, User},
    },
    test_db::TestDb,
    util::password::hash_password,
};

use super::support::{BASE, V1, create};
use crate::common::{self, TestRequest};

// ===== WBE-213: the account writes — delete, enable, approve, reject, action =====

/// The 403 body of the two refusals the writes answer alike: the admin who
/// names their own account in a deletion, and the rejection of an account
/// nobody had left pending.
pub(super) const NOT_ALLOWED: &str = "This action is not allowed";
/// The 400 body of a `disable` or `suspend` aimed at a remote mirror.
pub(super) const ONLY_LOCAL: &str = "Only local accounts can be disabled";
/// The 422 body of a `type` word off the five the umbrella knows.
pub(super) const INVALID_TYPE: &str = "Validation failed: Type is invalid";
/// The 422 body of a request that states no `type` at all.
pub(super) const MISSING_TYPE: &str = "Validation failed: Type is required";
/// The 400 body of a `report_id` whose report is about another account.
pub(super) const WRONG_TARGET: &str = "Report does not target this account";
/// The 403 body for a valid user token whose account is not an admin: the
/// reads and the writes are both the admin's.
pub(super) const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body of a token that grants neither `admin:write` nor
/// `admin:write:accounts`.
pub(super) const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The password every write fixture signs in with.
pub(super) const PASSWORD: &str = "a long enough pass";
/// The 403 the bearer gate answers a deactivated account's request with.
pub(super) const DISABLED: &str = "Your account is currently disabled";
/// The queue the `users.delete` sweep rides on.
pub(super) const BACKGROUND: &str = "background";
/// The job kind of an account's deletion sweep.
pub(super) const DELETE_KIND: &str = "users.delete";
/// The subject of the letter a newly approved account is posted.
pub(super) const APPROVED_MAIL: &str = "Your account on Wired is approved";
/// The subject of the letter a rejected account is posted.
pub(super) const REJECTED_MAIL: &str = "Your account on Wired has been rejected";

/// What the write tests leave behind: the admin, one account per state a
/// write weighs, and the `admin:write:accounts` token the writes are signed
/// with.
///
/// The app is built on a config with `SMTP_HOST` set, because the two
/// letters the approval and the rejection post are what those tests count;
/// the suite default configures no relay and every send would drop.
pub(super) struct Writes {
    /// The pool every row was inserted into.
    pub(super) pool: PgPool,
    /// The router built on that pool.
    pub(super) app: Router,
    /// The admin who signs every request.
    pub(super) root: User,
    /// An ordinary active, approved, confirmed account, with a password.
    pub(super) plain: User,
    /// A disabled account (active and public, its owner locked out), with a
    /// password.
    pub(super) asleep: User,
    /// A registration awaiting staff, with an address.
    pub(super) newbie: User,
    /// A registration awaiting staff that gave no address at all.
    pub(super) quiet: User,
    /// The reporter whose reports the umbrella resolves.
    pub(super) beeper: User,
    /// A remote mirror, which moderation reaches by id like any other row.
    pub(super) mirror: User,
    /// `root`'s `admin:write:accounts` bearer token.
    pub(super) token: String,
}

/// Seeds the accounts the writes act on: one per state a write weighs, plus
/// a reporter and a mirror, beside the admin who acts on them.
pub(super) async fn seed_writes(db: &TestDb) -> Writes {
    let pool = db.pool().clone();
    let root = create(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        ..user
    })
    .await;
    let plain = common::create_user_with_password(&pool, "plain", PASSWORD).await;
    let asleep = common::create_user_with_password(&pool, "asleep", PASSWORD).await;
    let disabled = sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
        .bind(asleep.id)
        .execute(&pool)
        .await
        .expect("the fixture is disabled")
        .rows_affected();
    assert_eq!(disabled, 1, "the disabled fixture exists");
    let newbie = create(&pool, "newbie", |user| NewUser {
        approval_pending: Flag::new(true),
        email: Some("newbie@example.org".to_owned()),
        password_hash: Some(hash_password(PASSWORD).expect("the fixture password hashes")),
        ..user
    })
    .await;
    let quiet = create(&pool, "quiet", |user| NewUser {
        approval_pending: Flag::new(true),
        password_hash: Some(hash_password(PASSWORD).expect("the fixture password hashes")),
        ..user
    })
    .await;
    let beeper = create(&pool, "beeper", |user| NewUser {
        actor_type: "Service".to_owned(),
        ..user
    })
    .await;
    let mirror = common::create_remote_user(&pool, "faraway", "remote.example").await;
    let token = common::create_app_and_token(&pool, &root, "admin:write:accounts").await;
    Writes {
        app: common::test_app_with_config(pool.clone(), mail_config()),
        pool,
        root,
        plain,
        asleep,
        newbie,
        quiet,
        beeper,
        mirror,
        token,
    }
}

/// A config with `SMTP_HOST` set, so the letters the approval and the
/// rejection post really reach the mailer queue.
pub(super) fn mail_config() -> Config {
    let mut config = common::test_config();
    config.smtp_host = Some("smtp.example.org".to_owned());
    config
}

/// The `{id}` route of one account.
pub(super) fn account_path(id: Uuid) -> String {
    format!("{V1}/{id}")
}

/// The `{id}/{suffix}` route of one account.
pub(super) fn account_action_path(id: Uuid, suffix: &str) -> String {
    format!("{V1}/{id}/{suffix}")
}

/// `DELETE`s `path` as `token`.
pub(super) async fn delete_as(app: &Router, token: &str, path: &str) -> common::TestResponse {
    TestRequest::delete(path).bearer(token).send(app).await
}

/// `POST`s the JSON `body` to `path` as `token`.
pub(super) async fn post_json(
    app: &Router,
    token: &str,
    path: &str,
    body: &Value,
) -> common::TestResponse {
    TestRequest::post(path)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// The umbrella request for `id`, asserted to answer the empty object under
/// a 200 — the pinned shape of every accepted action.
pub(super) async fn act(app: &Router, token: &str, id: Uuid, body: &Value) {
    let response = post_json(app, token, &account_action_path(id, "action"), body).await;
    common::assert_answer(&response, StatusCode::OK, "the account action");
    assert_eq!(
        response.json_value(),
        json!({}),
        "the umbrella answers the empty object, not Pleroma's 204"
    );
}

/// The three state flags of one row, or `None` once the row is gone:
/// activation, the staff gate and whether a deletion was stamped.
pub(super) async fn row_state(pool: &PgPool, id: Uuid) -> Option<(bool, bool, bool)> {
    sqlx::query_as::<_, (bool, bool, bool)>(
        "SELECT is_active, approval_pending, deleted_at IS NOT NULL FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .expect("the row reads back")
}

/// Whether the row of `id` carries a disable, or `None` once it is gone.
pub(super) async fn disabled_state(pool: &PgPool, id: Uuid) -> Option<bool> {
    sqlx::query_scalar::<_, bool>("SELECT disabled_at IS NOT NULL FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .expect("the row reads back")
}

/// How many live bearer tokens `account` holds.
pub(super) async fn token_count(pool: &PgPool, account: &User) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_tokens WHERE user_id = $1")
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the token count reads back")
}

/// The moderation-log entries of one action, oldest first: sentence and data.
pub(super) async fn logged(pool: &PgPool, action: &str) -> Vec<(String, Value)> {
    sqlx::query_as::<_, (String, Value)>(
        "SELECT message, data FROM moderation_log WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(pool)
    .await
    .expect("the log reads back")
}

/// The single entry one write must have left: its sentence and the nicknames
/// its `subject` data map names.
pub(super) async fn one_entry(pool: &PgPool, action: &str) -> (String, Vec<String>) {
    let entries = logged(pool, action).await;
    let (message, data) = entries
        .first()
        .unwrap_or_else(|| panic!("one {action} entry was expected"));
    assert_eq!(
        entries.len(),
        1,
        "exactly one {action} entry, got {entries:?}"
    );
    let subjects = data["subject"]
        .as_array()
        .expect("the subject list")
        .iter()
        .filter_map(|found| found["nickname"].as_str())
        .map(str::to_owned)
        .collect();
    (message.clone(), subjects)
}

/// The arguments of every job on `queue` of `kind`, oldest first.
pub(super) async fn jobs_of(pool: &PgPool, queue: &str, kind: &str) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args FROM jobs WHERE queue = $1 AND kind = $2 ORDER BY id",
    )
    .bind(queue)
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("the queue reads back")
    .into_iter()
    .map(|(args,)| args)
    .collect()
}

/// The sweeps queued for one account.
pub(super) async fn sweeps_for(pool: &PgPool, account: &User) -> Vec<Value> {
    jobs_of(pool, BACKGROUND, DELETE_KIND)
        .await
        .into_iter()
        .filter(|args| args["user_id"] == json!(account.id.to_string()))
        .collect()
}

/// How many rows the job table holds, whatever their queue or kind.
pub(super) async fn job_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
        .fetch_one(pool)
        .await
        .expect("the job count reads back")
}

/// The letters the mailer queue holds, in the order they were posted.
pub(super) async fn letters(pool: &PgPool) -> Vec<Value> {
    jobs_of(pool, "mailer", "mailer.send").await
}

/// The stored state of one report, beside whether its action stamp is set.
pub(super) async fn report_state(pool: &PgPool, id: Uuid) -> (String, bool) {
    sqlx::query_as::<_, (String, bool)>(
        "SELECT state, action_taken_at IS NOT NULL FROM reports WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the report reads back")
}

/// Files `beeper`'s report about `target` through the model.
pub(super) async fn file_against(pool: &PgPool, beeper: &User, target: &User) -> Uuid {
    let filed = report::create(
        pool,
        BASE,
        beeper,
        target,
        &NewReport {
            comment: "they did the thing".to_owned(),
            category: Category::Other,
            forwarded: false,
            rule_ids: Vec::new(),
            status_ids: Vec::new(),
        },
    )
    .await
    .expect("the fixture report files");
    filed.id
}

/// The 403 body the login gate and the bearer gate answer an
/// approval-pending account with, and its machine-readable reason.
pub(super) const AWAITING: &str = "Your account is awaiting approval.";

/// The OAuth `password` grant body for `username`, over the app's credentials.
pub(super) fn grant_body(client_id: &str, client_secret: &str, username: &str) -> Value {
    json!({
        "grant_type": "password",
        "username": username,
        "password": PASSWORD,
        "scope": "read",
        "client_id": client_id,
        "client_secret": client_secret,
    })
}

/// The `password` grant for `username`.
pub(super) async fn login(
    app: &Router,
    client_id: &str,
    client_secret: &str,
    username: &str,
) -> common::TestResponse {
    TestRequest::post("/oauth/token")
        .json(&grant_body(client_id, client_secret, username))
        .send(app)
        .await
}

/// An ordinary request to the account's own credentials endpoint, the one
/// call the bearer gate answers with its role-and-flag refusals.
pub(super) async fn verify(app: &Router, token: &str) -> common::TestResponse {
    TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(token)
        .send(app)
        .await
}

/// Sends the write `suffix` names for account `id`: the `DELETE` of the
/// account itself for the empty suffix, a body-less `POST` for the three
/// state routes and the umbrella `POST` for `action`. `token` is `None` for
/// the anonymous request.
pub(super) async fn write_request(
    app: &Router,
    token: Option<&str>,
    id: Uuid,
    suffix: &str,
) -> common::TestResponse {
    let path = if suffix.is_empty() {
        account_path(id)
    } else {
        account_action_path(id, suffix)
    };
    let body = if suffix == "action" {
        json!({"type": "none"})
    } else {
        json!({})
    };
    match (suffix.is_empty(), token) {
        (true, None) => TestRequest::delete(&path).send(app).await,
        (true, Some(text)) => TestRequest::delete(&path).bearer(text).send(app).await,
        (false, None) => TestRequest::post(&path).json(&body).send(app).await,
        (false, Some(text)) => {
            TestRequest::post(&path)
                .bearer(text)
                .json(&body)
                .send(app)
                .await
        }
    }
}
