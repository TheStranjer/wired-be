//! Shared consts and fixture helpers for every submodule of the
//! `admin_users` area: the seeded account set behind the listing, the
//! canned error bodies the gates assert, the path builders of the admin
//! users routes and the thin wrappers around the HTTP harness and the
//! stored rows. Everything here is `pub(super)`, and a submodule imports
//! exactly the members it calls; a helper only one submodule uses lives
//! there instead.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    models::user::{Flag, NewUser, User},
    services::instance_actor::ensure_instance_actor,
    test_db::TestDb,
};

use crate::common::{self, TestRequest};

/// The listing route.
pub(super) const USERS: &str = "/api/v1/pleroma/admin/users";

/// The 401 body when no token is presented.
pub(super) const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body when a presented token does not resolve.
pub(super) const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only (user-unbound) token.
pub(super) const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body for a valid token whose user is not an admin.
pub(super) const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body of a token that grants neither `admin:read` nor
/// `admin:read:accounts`.
pub(super) const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 404 body of the admin API's misses.
pub(super) const NOT_FOUND: &str = "Not found";
/// The frontend's default avatar, which an account without one renders.
pub(super) const DEFAULT_AVATAR: &str = "/images/avi.png";

/// What the fixture leaves behind for one test.
pub(super) struct Seeded {
    /// The pool every row was inserted into.
    pub(super) pool: PgPool,
    /// The router built on that pool.
    pub(super) app: Router,
    /// The admin account, whose tokens the tests mint.
    pub(super) root: User,
    /// The remote mirror, whose handle the show route must resolve.
    pub(super) mirror: User,
    /// The instance actor, which the listing hides and the show finds.
    pub(super) actor: User,
    /// A bearer token for `root` granting the accounts read scope.
    pub(super) token: String,
}

/// Seeds one account per state the filters weigh, plus the instance actor,
/// and returns them beside the admin who reads them and the app to read
/// them through.
///
/// The `db` slot a test leases must outlive this: the test keeps its own
/// binding, as every test here does.
/// Inserts a local account named `nickname` with `customize` applied on top
/// of [`NewUser::local`], and spends the row: the tests below read these
/// accounts back through the API, not from Rust.
async fn seeded<F>(pool: &PgPool, nickname: &str, customize: F)
where
    F: FnOnce(NewUser) -> NewUser,
{
    let _row: User = common::create_local_user_with(pool, nickname, customize).await;
}

pub(super) async fn seed(db: &TestDb) -> Seeded {
    let pool = db.pool().clone();
    let root = common::create_local_user_with(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        email: Some("root@example.org".to_owned()),
        ..user
    })
    .await;
    seeded(&pool, "moddy", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    seeded(&pool, "plain", |user| NewUser {
        name: "Plain Person".to_owned(),
        email: Some("plain@example.org".to_owned()),
        ..user
    })
    .await;
    seeded(&pool, "suspended", |user| NewUser {
        is_active: Flag::new(false),
        ..user
    })
    .await;
    seeded(&pool, "newbie", |user| NewUser {
        approval_pending: Flag::new(true),
        registration_reason: Some("please let me in".to_owned()),
        ..user
    })
    .await;
    seeded(&pool, "unminded", |user| NewUser {
        email_confirmed: Flag::new(false),
        ..user
    })
    .await;
    seeded(&pool, "beeper", |user| NewUser {
        actor_type: "Service".to_owned(),
        tags: vec!["bots".to_owned()],
        avatar_url: Some(format!("{TEST_BASE_URL}/uploads/bot.png")),
        ..user
    })
    .await;
    let mirror = common::create_remote_user(&pool, "faraway", "remote.example").await;
    let actor = ensure_instance_actor(&pool, &common::test_config())
        .await
        .expect("the instance actor is seeded");
    let token = common::create_app_and_token(&pool, &root, "admin:read:accounts").await;
    Seeded {
        pool,
        app: common::test_app(db.pool().clone()),
        root,
        mirror,
        actor,
        token,
    }
}

/// `GET`s `path` as `token` and returns the parsed body, asserting the 200
/// an admin read answers with.
pub(super) async fn read(app: &Router, token: &str, path: &str) -> Value {
    let response = TestRequest::get(path).bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, "an admin read");
    response.json_value()
}

/// The listing of `query` (an empty string is the bare route) as `token`.
pub(super) async fn page(app: &Router, token: &str, query: &str) -> Value {
    let path = if query.is_empty() {
        USERS.to_owned()
    } else {
        format!("{USERS}?{query}")
    };
    read(app, token, &path).await
}

/// The show route of one path segment.
pub(super) fn show(segment: &str) -> String {
    format!("{USERS}/{segment}")
}

/// A bearer token for `user` granting `scopes`.
pub(super) async fn mint(pool: &PgPool, user: &User, scopes: &str) -> String {
    common::create_app_and_token(pool, user, scopes).await
}

/// The credentials routes of one account.
pub(super) fn credentials(nickname: &str) -> String {
    format!("{USERS}/{nickname}/credentials")
}

/// The permission-group read of one account.
pub(super) fn group(nickname: &str) -> String {
    format!("{USERS}/{nickname}/permission_group")
}

/// The permission-group read of one account, naming one group.
pub(super) fn group_of(nickname: &str, name: &str) -> String {
    format!("{}/{name}", group(nickname))
}

/// The nickname-list permission-group route for one group.
pub(super) fn group_bulk(name: &str) -> String {
    format!("{USERS}/permission_group/{name}")
}

/// The reset-token route of one account.
pub(super) fn issued_reset(nickname: &str) -> String {
    format!("{USERS}/{nickname}/password_reset")
}

/// The forced-reset PATCH route.
pub(super) const FORCE_RESET: &str = "/api/v1/pleroma/admin/users/password_reset";

/// The email-confirmation PATCH route.
pub(super) const CONFIRM: &str = "/api/v1/pleroma/admin/users/confirm_email";
/// The confirmation-resend PATCH route.
pub(super) const RESEND: &str = "/api/v1/pleroma/admin/users/resend_confirmation_email";
/// The MFA-removal PUT route.
pub(super) const DISABLE_MFA: &str = "/api/v1/pleroma/admin/users/disable_mfa";
/// The forced-follow POST route.
pub(super) const FOLLOW: &str = "/api/v1/pleroma/admin/users/follow";
/// The forced-unfollow POST route.
pub(super) const UNFOLLOW: &str = "/api/v1/pleroma/admin/users/unfollow";
/// The tag pair's route, `PUT` and `DELETE` on one path (WBE-197).
pub(super) const TAG: &str = "/api/v1/pleroma/admin/users/tag";
/// The suggestion-pool add route.
pub(super) const SUGGEST: &str = "/api/v1/pleroma/admin/users/suggest";
/// The suggestion-pool remove route.
pub(super) const UNSUGGEST: &str = "/api/v1/pleroma/admin/users/unsuggest";

/// The 403 body of a token that grants neither `admin:write` nor
/// `admin:write:accounts`.
pub(super) const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";

/// The 404 body of a `permission_group` segment that names no group.
pub(super) const NO_SUCH_GROUP: &str = "No such permission_group";

/// A config with `SMTP_HOST` set, so `confirmation::send` really queues
/// the letters the resend tests count (the suite default configures no
/// relay and every send would drop).
pub(super) fn mail_config() -> wired_be::config::Config {
    let mut config = common::test_config();
    config.smtp_host = Some("smtp.example.org".to_owned());
    config
}

/// The write-scoped token of the freshly seeded fixture's admin.
pub(super) async fn write_token(seeded: &Seeded) -> String {
    mint(&seeded.pool, &seeded.root, "admin:write:accounts").await
}

/// A `PATCH` of `path` as `token` with the JSON `body`.
pub(super) async fn patch_json(
    app: &Router,
    token: &str,
    path: &str,
    body: &Value,
) -> common::TestResponse {
    TestRequest::patch(path)
        .bearer(token)
        .json(body)
        .send(app)
        .await
}

/// Asserts the 204 the ticket pins, whose body is empty.
pub(super) fn assert_no_content(response: &common::TestResponse, who: &str) {
    common::assert_answer(response, StatusCode::NO_CONTENT, who);
    assert!(response.body.is_empty(), "{who}: the 204 body is empty");
}

/// The moderation-log rows of one action, oldest first: sentence and data.
pub(super) async fn logged(pool: &PgPool, action: &str) -> Vec<(String, Value)> {
    sqlx::query_as::<_, (String, Value)>(
        "SELECT message, data FROM moderation_log WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(pool)
    .await
    .expect("the log reads back")
}

/// The single entry one write action must have left: its sentence and the
/// nicknames its `subject` data map names.
pub(super) async fn one_entry(pool: &PgPool, action: &str) -> (String, Vec<String>) {
    let entries = logged(pool, action).await;
    let (message, data) = entries
        .first()
        .unwrap_or_else(|| panic!("one {action} entry was expected"));
    let subjects = data["subject"]
        .as_array()
        .expect("the subject list")
        .iter()
        .filter_map(|found| found["nickname"].as_str())
        .map(str::to_owned)
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "exactly one {action} entry, got {entries:?}"
    );
    (message.clone(), subjects)
}

/// The stored `password_reset_pending` of one account.
pub(super) async fn pending(pool: &PgPool, account: &User) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT password_reset_pending FROM users WHERE id = $1")
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the pending flag reads back")
}

/// The stored boolean of one named `users` column, asked by its literal
/// SQL and `account`'s id.
pub(super) async fn flag_of(pool: &PgPool, sql: &'static str, account: &User) -> bool {
    sqlx::query_scalar::<_, bool>(sql)
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the flag reads back")
}

/// The plain account the credentials tests write, re-read from the row
/// behind the fixture (the nickname the seed gave, nothing else).
pub(super) async fn plain(pool: &PgPool) -> User {
    wired_be::models::user::get_by_nickname(pool, "plain")
        .await
        .expect("the account reads back")
        .expect("the fixture seeded it")
}

/// The stored `tags` array of one account.
pub(super) async fn tags_of(pool: &PgPool, account: &User) -> Vec<String> {
    sqlx::query_scalar::<_, Vec<String>>("SELECT tags FROM users WHERE id = $1")
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the tags read back")
}

/// A `PUT` of `path` as `token`, with the JSON `body` when one is given.
pub(super) async fn put_json(
    app: &Router,
    token: &str,
    path: &str,
    body: Option<&Value>,
) -> common::TestResponse {
    let base = TestRequest::put(path);
    let with_body = match body {
        Some(value) => base.json(value),
        None => base,
    };
    with_body.bearer(token).send(app).await
}

/// A `DELETE` of `path` as `token`, with the JSON `body` when one is given.
pub(super) async fn delete_json(
    app: &Router,
    token: &str,
    path: &str,
    body: Option<&Value>,
) -> common::TestResponse {
    let base = TestRequest::delete(path);
    let with_body = match body {
        Some(value) => base.json(value),
        None => base,
    };
    with_body.bearer(token).send(app).await
}

/// A `POST` of `path` as `token`, with the JSON `body` when one is given.
pub(super) async fn post_json(
    app: &Router,
    token: &str,
    path: &str,
    body: Option<&Value>,
) -> common::TestResponse {
    let base = TestRequest::post(path);
    let with_body = match body {
        Some(value) => base.json(value),
        None => base,
    };
    with_body.bearer(token).send(app).await
}

/// The nicknames one answer's `users` array carries, in order.
pub(super) fn answered_users(body: &Value) -> Vec<&str> {
    body["users"]
        .as_array()
        .expect("the users array")
        .iter()
        .filter_map(|found| found["nickname"].as_str())
        .collect()
}

/// The account the shared fixture seeded, re-read from behind the row.
pub(super) async fn account_named(pool: &PgPool, nickname: &str) -> User {
    wired_be::models::user::get_by_nickname(pool, nickname)
        .await
        .expect("the account reads back")
        .expect("the fixture seeded it")
}

/// A password every lifecycle fixture and created account uses, over the
/// test config's minimum length.
pub(super) const PASSWORD: &str = "a long enough pass";

/// A `users` body creating one account per `(nickname, email, password)`
/// triple, in the order stated.
pub(super) fn create_body(items: &[(&str, &str, &str)]) -> Value {
    json!({
        "users": items
            .iter()
            .map(|(nickname, email, password)| json!({
                "nickname": nickname,
                "email": email,
                "password": password,
            }))
            .collect::<Vec<Value>>(),
    })
}

/// The OAuth `password` grant body for `username`, over the app's
/// credentials.
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

/// The three lifecycle flags one account carries: activation, confirmation
/// and the staff gate.
pub(super) async fn lifecycle_flags(pool: &PgPool, nickname: &str) -> (bool, bool, bool) {
    sqlx::query_as::<_, (bool, bool, bool)>(
        "SELECT is_active, email_confirmed, approval_pending FROM users WHERE nickname = $1",
    )
    .bind(nickname)
    .fetch_one(pool)
    .await
    .expect("the flags read back")
}

/// Whether any account answers to `nickname`.
pub(super) async fn exists(pool: &PgPool, nickname: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM users WHERE nickname = $1)")
        .bind(nickname)
        .fetch_one(pool)
        .await
        .expect("the existence read runs")
}

/// The arguments of every job on `queue` of `kind`, oldest first.
pub(super) async fn jobs_of(pool: &PgPool, queue: &str, kind: &str) -> Vec<Value> {
    sqlx::query_as::<_, (Value,)>(
        "SELECT args FROM jobs WHERE queue = $1 AND kind = $2 ORDER BY inserted_at",
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

/// How many live bearer tokens `account` holds.
pub(super) async fn token_count(pool: &PgPool, account: &User) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_tokens WHERE user_id = $1")
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the token count reads back")
}
