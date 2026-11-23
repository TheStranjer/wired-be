//! The read-section consts and the fixture the read submodules draw on: the
//! route prefixes and the bodies their refusals share, the accounts each
//! filter expects, the sixteen pinned entity keys, and the seeding, reading
//! and parsing helpers over them.

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

/// The v1 listing's prefix, and the show route's.
pub(super) const V1: &str = "/api/v1/admin/accounts";
/// The v2 listing's prefix.
pub(super) const V2: &str = "/api/v2/admin/accounts";
/// The origin the test configuration answers its URLs from.
pub(super) const BASE: &str = TEST_BASE_URL;
/// The 401 body when no token is presented.
pub(super) const NO_TOKEN: &str = "Invalid credentials";
/// The 401 body when a presented token does not resolve.
pub(super) const INVALID_TOKEN: &str = "The access token is invalid";
/// The 403 body for an app-only (user-unbound) token.
pub(super) const USER_REQUIRED: &str = "This API requires an authenticated user";
/// The 403 body of a token that grants neither `admin:read` nor
/// `admin:read:accounts`.
pub(super) const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 404 body of these routes' misses.
pub(super) const NOT_FOUND: &str = "Record not found";
/// The 400 body of a cursor that is no id.
pub(super) const INVALID_ID: &str = "Invalid id";

/// The accounts the fixture seeds, in the order both listings answer them:
/// newest row first (`UUIDv7` ids sort by creation, decision 4), the instance
/// actor absent. The fixture inserts in the order root, moddy, plain,
/// suspended, newbie, unminded, beeper, mirror, so the mirror is the newest row
/// and the admin the oldest.
pub(super) const LISTED: [&str; 8] = [
    "faraway@remote.example",
    "beeper",
    "unminded",
    "newbie",
    "suspended",
    "plain",
    "moddy",
    "root",
];
/// The seven accounts this instance owns.
pub(super) const LOCALS: [&str; 7] = [
    "beeper",
    "unminded",
    "newbie",
    "suspended",
    "plain",
    "moddy",
    "root",
];
/// The accounts that are active, approved and confirmed at once.
pub(super) const ACTIVE: [&str; 5] = ["faraway@remote.example", "beeper", "plain", "moddy", "root"];
/// Every account but the deactivated one.
pub(super) const ENABLED: [&str; 7] = [
    "faraway@remote.example",
    "beeper",
    "unminded",
    "newbie",
    "plain",
    "moddy",
    "root",
];
/// Every account but the one still awaiting approval.
pub(super) const APPROVED: [&str; 7] = [
    "faraway@remote.example",
    "beeper",
    "unminded",
    "suspended",
    "plain",
    "moddy",
    "root",
];
/// Every account but the two staff ones.
pub(super) const MEMBERS: [&str; 6] = [
    "faraway@remote.example",
    "beeper",
    "unminded",
    "newbie",
    "suspended",
    "plain",
];
/// The entity's sixteen keys, sorted the way [`keys`] reports them.
pub(super) const ENTITY_KEYS: [&str; 16] = [
    "account",
    "approved",
    "confirmed",
    "created_at",
    "disabled",
    "domain",
    "email",
    "id",
    "invite_request",
    "ip",
    "ips",
    "locale",
    "role",
    "silenced",
    "suspended",
    "username",
];

/// What the fixture leaves behind for one test.
pub(super) struct Seeded {
    /// The router built on the slot's pool.
    pub(super) app: Router,
    /// The admin account, whose tokens the scope tests mint.
    pub(super) root: User,
    /// The moderator, whose token the admin gate refuses.
    pub(super) moddy: User,
    /// The plain account a stranger's token is minted from.
    pub(super) plain: User,
    /// The remote mirror, whose handle the entity splits in two.
    pub(super) mirror: User,
    /// The instance actor, which no listing answers with.
    pub(super) actor: User,
    /// A bearer token for `root` granting the accounts read scope.
    pub(super) token: String,
}

/// Seeds one account per state the filters weigh, plus the instance actor, and
/// returns them beside the admin who reads them and the app to read them
/// through.
pub(super) async fn seed(db: &TestDb) -> Seeded {
    let pool = db.pool().clone();
    let root = create(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        email: Some("root@example.org".to_owned()),
        ..user
    })
    .await;
    let moddy = create(&pool, "moddy", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    let plain = create(&pool, "plain", |user| NewUser {
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
        ..user
    })
    .await;
    let mirror = common::create_remote_user(&pool, "faraway", "remote.example").await;
    let actor = ensure_instance_actor(&pool, &common::test_config())
        .await
        .expect("the instance actor is seeded");
    let token = common::create_app_and_token(&pool, &root, "admin:read:accounts").await;
    Seeded {
        app: common::test_app(pool),
        root,
        moddy,
        plain,
        mirror,
        actor,
        token,
    }
}

/// Inserts a local account named `nickname` with `customize` applied on top of
/// [`NewUser::local`].
pub(super) async fn create<F>(pool: &PgPool, nickname: &str, customize: F) -> User
where
    F: FnOnce(NewUser) -> NewUser,
{
    common::create_local_user_with(pool, nickname, customize).await
}

/// The same insert, spending the row: the tests below read these accounts back
/// through the API rather than from Rust, and only the four whose ids or
/// stamps they name are kept.
pub(super) async fn seeded<F>(pool: &PgPool, nickname: &str, customize: F)
where
    F: FnOnce(NewUser) -> NewUser,
{
    let _row: User = create(pool, nickname, customize).await;
}

/// `GET`s `path` as `token` and returns the parsed body, asserting the 200 an
/// admin read answers with.
pub(super) async fn read(app: &Router, token: &str, path: &str) -> Value {
    let response = TestRequest::get(path).bearer(token).send(app).await;
    common::assert_answer(&response, StatusCode::OK, "an admin read");
    response.json_value()
}

/// The v1 listing of `query` (an empty string is the bare route).
pub(super) async fn v1_page(app: &Router, token: &str, query: &str) -> Value {
    read(app, token, &route(V1, query)).await
}

/// The v2 listing of `query`.
pub(super) async fn v2_page(app: &Router, token: &str, query: &str) -> Value {
    read(app, token, &route(V2, query)).await
}

/// The route with its query string: an empty one leaves the `?` off.
pub(super) fn route(prefix: &str, query: &str) -> String {
    if query.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}?{query}")
    }
}

/// The handles one listing answer names, in the order it names them.
pub(super) fn listed(body: &Value) -> Vec<&str> {
    body.as_array()
        .expect("an array of admin accounts")
        .iter()
        .filter_map(|found| found["account"]["acct"].as_str())
        .collect()
}

/// The keys of one rendered object, sorted so a missing or an extra one shows
/// up as a diff rather than a lucky pass.
pub(super) fn keys(value: &Value) -> Vec<&str> {
    let mut found = value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<&str>>();
    found.sort_unstable();
    found
}

/// The entity of `acct` out of a listing answer.
pub(super) fn entity<'a>(body: &'a Value, acct: &str) -> &'a Value {
    body.as_array()
        .expect("an array of admin accounts")
        .iter()
        .find(|found| found["account"]["acct"] == json!(acct))
        .unwrap_or_else(|| panic!("the answer holds no {acct}"))
}
