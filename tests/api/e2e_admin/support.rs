//! Shared consts and helpers for every submodule of the `e2e_admin` area:
//! the route roots and canned bodies the scenarios assert, and the
//! [`World`] a scenario opens — its router, state, client, staff bearers
//! and scratch directories — with the `Account` type, the login, the
//! listing, posting and moderation-log reads every step rides, and the
//! config-write wire helpers. Everything shared here is `pub(super)`, and
//! a submodule imports exactly the members it calls; a helper only one
//! submodule uses lives there instead.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use wired_be::{
    config::{Config, TEST_BASE_URL},
    models::user::User,
    state::AppState,
};

use crate::common;

/// The password every scenario account is planted or registered with.
pub(super) const PASSWORD: &str = "correct horse battery staple";
/// The scopes every scenario client registers with, and so every token it
/// wins: the whole client set plus the admin pair.
pub(super) const CLIENT_SCOPES: &str = "read write follow push admin:read admin:write";
/// The instance origin [`common::test_config`] serves on.
pub(super) const BASE: &str = TEST_BASE_URL;
/// The Pleroma admin API root.
pub(super) const P_ADMIN: &str = "/api/v1/pleroma/admin";
/// The reporter's own reports mount.
pub(super) const V0_REPORTS: &str = "/api/v0/pleroma/reports";
/// The Mastodon admin reports root (WBE-214).
pub(super) const M_ADMIN_REPORTS: &str = "/api/v1/admin/reports";
/// The status write.
pub(super) const STATUSES: &str = "/api/v1/statuses";
/// The report filing.
pub(super) const REPORTS: &str = "/api/v1/reports";
/// The caller's own account read.
pub(super) const VERIFY: &str = "/api/v1/accounts/verify_credentials";
/// The anonymous public timeline.
pub(super) const PUBLIC_TIMELINE: &str = "/api/v1/timelines/public";
/// The admin config store.
pub(super) const CONFIG: &str = "/api/v1/pleroma/admin/config";
/// The admin users root.
pub(super) const USERS: &str = "/api/v1/pleroma/admin/users";
/// The moderation-log listing.
pub(super) const LOG: &str = "/api/v1/pleroma/admin/moderation_log";
/// The `Loopback` host the mock peer answers on, port-less the way the
/// MRF domain lists and the inbox door compare hosts.
pub(super) const LOOPBACK: &str = "127.0.0.1";
/// The mock peer's author account every MRF phase delivers as.
pub(super) const BOB: &str = "bob";
/// The 403 body of the staff routes for a non-staff account.
pub(super) const NOT_STAFF: &str = "User is not a staff member.";
/// The 403 body of the admin-only routes for a staff non-admin.
pub(super) const NOT_ADMIN: &str = "User is not an admin.";
/// The 403 body of a missing `admin:read` grant.
pub(super) const NO_ADMIN_READ: &str = "Insufficient permissions: admin:read.";
/// The 403 body of a missing `admin:write` grant.
pub(super) const NO_ADMIN_WRITE: &str = "Insufficient permissions: admin:write.";
/// The 403 body of the inbox door for a host the `simple` lists reject.
pub(super) const REJECTED: &str = "Rejected";
/// The 429 body of every throttled bucket.
pub(super) const THROTTLED: &str = "Throttled";
/// The 403 body of the bearer gate's refusal of a disabled account.
pub(super) const DISABLED: &str = "Your account is currently disabled";
/// The past announcement instant.
pub(super) const PAST: &str = "2000-06-01T12:00:00.000Z";
/// The far-future announcement instant.
pub(super) const FUTURE: &str = "2100-06-01T12:00:00.000Z";
/// The `PNG` signature plus a tail, compared verbatim: the media proxy
/// never decodes what it serves.
pub(super) const PROXY_PNG: &[u8] = b"\x89PNG\r\n\x1a\n-proxy-origin";
/// The multipart boundary the document and emoji uploads frame with.
pub(super) const BOUNDARY: &str = "wired-e2e-admin-boundary";

/// One scenario's world: the router every step rides, the [`AppState`]
/// behind it that [`common::run_all_jobs`] drains, the client accounts
/// register through, and the two staff bearers the admin API is driven
/// with. The scratch directories the uploads, emoji packs and instance
/// documents land in live as long as the world does.
pub(super) struct World {
    /// The application under test.
    pub(super) app: Router,
    /// The state the router rides and the jobs run on.
    pub(super) state: AppState,
    /// The scenario's OAuth client.
    pub(super) client: common::TestClient,
    /// The admin's bearer.
    pub(super) admin: String,
    /// The admin account [`admin`](Self::admin) belongs to.
    pub(super) admin_user: User,
    /// The moderator's bearer.
    pub(super) moderator: String,
    /// The moderator account [`moderator`](Self::moderator) belongs to.
    pub(super) moderator_user: User,
    /// The scratch uploads root; nothing reads it, dropping it is the job.
    pub(super) _uploads: TempDir,
    /// The scratch emoji-pack root, likewise.
    pub(super) _emoji: TempDir,
    /// The scratch instance-document root, likewise.
    pub(super) _docs: TempDir,
}

impl World {
    /// The world over `pool` and `config` with its three scratch
    /// directories mounted: the two staff accounts planted with the
    /// fixtures the end-to-end scenarios own (no public endpoint mints an
    /// admin or a moderator), their credentials won over HTTP by the
    /// password grant, and the client registered as the first call.
    pub(super) async fn start(pool: PgPool, mut config: Config) -> Self {
        let uploads = TempDir::new().expect("a scratch uploads dir");
        let emoji = TempDir::new().expect("a scratch emoji dir");
        let docs = TempDir::new().expect("a scratch docs dir");
        config.uploads_dir = uploads.path().to_path_buf();
        config.emoji_dir = emoji.path().to_path_buf();
        config.statics.documents.dir = docs.path().to_path_buf();
        let admin_user = common::create_admin_with_password(&pool, "root", PASSWORD).await;
        let moderator_user = common::create_moderator_with_password(&pool, "moddy", PASSWORD).await;
        let state = common::test_app_state(pool.clone(), config);
        let app = wired_be::app(state.clone());
        let client = common::register_client(&app, "Admin Scenario Client", CLIENT_SCOPES).await;
        let admin = login(&app, &client, "root").await;
        let moderator = login(&app, &client, "moddy").await;
        Self {
            app,
            state,
            client,
            admin,
            admin_user,
            moderator,
            moderator_user,
            _uploads: uploads,
            _emoji: emoji,
            _docs: docs,
        }
    }

    /// The world on the suite's stock test config.
    pub(super) async fn open(pool: PgPool) -> Self {
        Self::start(pool, common::test_config()).await
    }

    /// Drains the job queues the way the live worker would.
    pub(super) async fn drain(&self) -> usize {
        common::run_all_jobs(&self.state).await
    }

    /// Registers `username` over the public `POST /api/v1/accounts`.
    pub(super) async fn register(&self, username: &str) -> Account {
        let token = common::register_via_api(&self.app, &self.client, username, PASSWORD).await;
        Account::read(&self.app, username, token).await
    }
}

/// A scenario account: its id, nickname and the bearer it acts with.
pub(super) struct Account {
    /// The account id.
    pub(super) id: String,
    /// The nickname.
    pub(super) nickname: String,
    /// The bearer.
    pub(super) token: String,
}

impl Account {
    /// Reads `username`'s own account with `token` over
    /// `GET /api/v1/accounts/verify_credentials`.
    pub(super) async fn read(app: &Router, username: &str, token: String) -> Self {
        let response = common::api_get(app, Some(&token), VERIFY).await;
        common::assert_answer(&response, StatusCode::OK, "the account reads itself");
        Self {
            id: id_of(&response.json_value()),
            nickname: username.to_owned(),
            token,
        }
    }
}

/// The `password` grant for `username`, asserted to its bearer.
async fn login(app: &Router, client: &common::TestClient, username: &str) -> String {
    let response = common::login_password(app, client, username, PASSWORD).await;
    common::assert_answer(&response, StatusCode::OK, "the scenario login");
    common::access_token_of(&response)
}

/// The string `id` of an entity answer.
pub(super) fn id_of(entity: &Value) -> String {
    entity["id"]
        .as_str()
        .expect("the entity carries a string id")
        .to_owned()
}

/// `GET` of `path` as `token` (`None` anonymous), asserted 200 and
/// answered as a JSON array.
pub(super) async fn get_array(app: &Router, token: Option<&str>, path: &str) -> Vec<Value> {
    let response = common::api_get(app, token, path).await;
    common::assert_answer(&response, StatusCode::OK, &format!("the listing of {path}"));
    response
        .json_value()
        .as_array()
        .unwrap_or_else(|| panic!("{path} answers a JSON array: {}", response.text()))
        .clone()
}

/// The anonymous public-timeline ids.
pub(super) async fn public_ids(app: &Router) -> Vec<String> {
    get_array(app, None, PUBLIC_TIMELINE)
        .await
        .into_iter()
        .map(|entry| id_of(&entry))
        .collect()
}

/// Whether `entries` carry the entity whose `id` (or `uri`) is `id`.
pub(super) fn carries(entries: &[Value], key: &str, id: &str) -> bool {
    entries.iter().any(|entry| entry[key].as_str() == Some(id))
}

/// `POST /api/v1/statuses` with `text` as `acct`, answered as the entity.
pub(super) async fn post_status(world: &World, acct: &Account, text: &str) -> Value {
    let response = common::api_post(
        &world.app,
        Some(&acct.token),
        STATUSES,
        &json!({"status": text}),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the scenario post");
    response.json_value()
}

/// The whole moderation log as `(action, message)` pairs, newest first.
async fn log_entries(app: &Router, token: &str) -> Vec<(String, String)> {
    let response = common::api_get(app, Some(token), &format!("{LOG}?page_size=200")).await;
    common::assert_answer(&response, StatusCode::OK, "the moderation-log listing");
    response
        .json_value()
        .get("items")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("the log answers its items: {}", response.text()))
        .iter()
        .map(|entry| {
            (
                entry["data"]["action"]
                    .as_str()
                    .unwrap_or_else(|| panic!("an entry names its action: {entry}"))
                    .to_owned(),
                entry["message"]
                    .as_str()
                    .unwrap_or_else(|| panic!("an entry carries its message: {entry}"))
                    .to_owned(),
            )
        })
        .collect()
}

/// The moderation-log messages whose action is `action`, newest first.
async fn log_lines(app: &Router, token: &str, action: &str) -> Vec<String> {
    log_entries(app, token)
        .await
        .into_iter()
        .filter(|(entry, _)| entry == action)
        .map(|(_entry, message)| message)
        .collect()
}

/// The log's messages for `action` as a sorted set, compared to `expected`
/// sorted: the count is the assertion and every sentence is exact.
pub(super) async fn assert_log(app: &Router, token: &str, action: &str, expected: &[String]) {
    let mut lines = log_lines(app, token, action).await;
    lines.sort();
    let mut want = expected.to_vec();
    want.sort();
    assert_eq!(lines, want, "the {action} sentences in the log");
}

/// `POST {CONFIG}` with `items`, answered raw.
pub(super) async fn post_configs(app: &Router, token: &str, items: Value) -> common::TestResponse {
    common::api_post(app, Some(token), CONFIG, &json!({"configs": items})).await
}

/// The wire-form value of one `:pleroma` row: an array of
/// `{"tuple": [key, value]}` entries, Pleroma's `ConfigDB` spelling.
pub(super) fn wire(pairs: &[(&str, Value)]) -> Value {
    pairs
        .iter()
        .map(|(key, value)| json!({"tuple": [key, value]}))
        .collect()
}
