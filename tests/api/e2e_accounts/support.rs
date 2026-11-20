//! Shared scaffolding for every submodule of the `e2e_accounts` area: the
//! route roots, canned bodies and the scenario password every step quotes,
//! the [`World`] a journey opens — its router, state, registered client
//! and the registrations, logins and queue drains every step rides — the
//! `Account` type, the relationship, `Link`-cursor, listing and mailbox
//! readers the assertions share, the registration-mode and mail configs,
//! and the loopback `MockPeer` whose actor the domain-block leg resolves.
//! Everything shared here is `pub(super)`, and a submodule imports exactly
//! the members it calls; a helper only one submodule uses lives there.

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse as _, Response},
    routing::get,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};
use wired_be::{
    config::{Config, Toggle},
    state::AppState,
    util::totp,
};

use crate::common::{self, TestClient, TestResponse};

/// The password every scenario account registers with.
pub(super) const PASSWORD: &str = "correct horse battery staple";
/// The password the onboarding scenario changes to.
pub(super) const NEW_PASSWORD: &str = "a new and longer password";
/// The scopes every scenario client registers with, and so every token it
/// hands out: the whole client set plus the admin pair, so one client
/// covers every step of every journey.
pub(super) const CLIENT_SCOPES: &str = "read write follow push admin:read admin:write";
/// The instance origin [`common::test_config`] serves on.
pub(super) const BASE: &str = wired_be::config::TEST_BASE_URL;
/// The account root every read and relationship write hangs under.
pub(super) const ACCOUNTS: &str = "/api/v1/accounts";
/// The caller's own account, as `verify_credentials` answers it.
pub(super) const VERIFY: &str = "/api/v1/accounts/verify_credentials";
/// The profile write's own route.
pub(super) const CREDENTIALS: &str = "/api/v1/accounts/update_credentials";
/// The status route, as the scope gate's probe.
pub(super) const STATUSES: &str = "/api/v1/statuses";
/// The token endpoint every grant and rotation runs through.
pub(super) const TOKEN: &str = "/oauth/token";
/// The revocation endpoint.
pub(super) const REVOKE: &str = "/oauth/revoke";
/// The consent endpoint pair.
pub(super) const AUTHORIZE: &str = "/oauth/authorize";
/// The second factor's challenge endpoint.
pub(super) const CHALLENGE: &str = "/oauth/mfa/challenge";
/// The MFA settings root.
pub(super) const MFA: &str = "/api/pleroma/accounts/mfa";
/// The Pleroma admin users root.
pub(super) const ADMIN_USERS: &str = "/api/v1/pleroma/admin/users";
/// The settings store of the scenario's own client, keyed by app name.
pub(super) const SETTINGS: &str = "/api/v1/pleroma/settings/mobile";
/// The 401 body of a credential that no longer resolves.
pub(super) const INVALID_TOKEN: &str = "The access token is invalid";
/// The 400 body every credential and code failure shares.
pub(super) const INVALID_CREDENTIALS: &str = "Invalid credentials";
/// The 403 body of the bearer gate's own refusal of a disabled account.
pub(super) const DISABLED: &str = "Your account is currently disabled";
/// The 404 body of the client API's misses.
pub(super) const NOT_FOUND: &str = "Record not found";
/// The refusal wording of a challenge that cannot be passed.
pub(super) const INVALID_CODE: &str = "Invalid code";
/// The multipart boundary the profile-image step frames its body with.
pub(super) const BOUNDARY: &str = "wired-e2e-accounts-boundary";
/// A `PNG`: the signature the upload detector names, and nothing more —
/// a profile image is never decoded.
pub(super) const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
/// The seconds the timed mute asks for: one hour, which the listing hands
/// back as an expiry an hour out.
pub(super) const MUTE_SECONDS: u32 = 3600;
/// The private note one account writes on another.
pub(super) const NOTE: &str = "reads every thread twice";
/// The title of the list the social-graph scenario creates.
pub(super) const LIST: &str = "Circles";
/// A well-formed id that names no account: the member the list refuses.
pub(super) const UNKNOWN_ID: &str = "00000000-0000-4000-8000-000000000000";

/// One scenario's world: the router every step rides, the [`AppState`]
/// behind it (which the job drain and the memory mailbox read), and the
/// client the scenario registered over HTTP as its very first call.
pub(super) struct World {
    /// The application under test.
    pub(super) app: Router,
    /// The state the router and [`drain`](Self::drain) share.
    pub(super) state: AppState,
    /// The OAuth client `POST /api/v1/apps` answered.
    pub(super) client: TestClient,
    /// The scratch uploads directory the profile images land in; the
    /// world's presence is what keeps the files readable. Named with a
    /// leading underscore because nothing reads it — dropping it is the
    /// whole job.
    _uploads: TempDir,
}

impl World {
    /// The world over `pool` with `config`: uploads moved into a scratch
    /// directory, and the client registered as the scenario's first move.
    pub(super) async fn start(pool: PgPool, mut config: Config) -> Self {
        let uploads = TempDir::new().expect("temp dir for uploads");
        config.uploads_dir = uploads.path().to_path_buf();
        let state = common::test_app_state(pool, config);
        let app = wired_be::app(state.clone());
        let client = common::register_client(&app, "Scenario Client", CLIENT_SCOPES).await;
        Self {
            app,
            state,
            client,
            _uploads: uploads,
        }
    }

    /// The world on the suite's default, open-instance config.
    pub(super) async fn open(pool: PgPool) -> Self {
        Self::start(pool, common::test_config()).await
    }

    /// The app-only bearer of [`Self::client`]: the credential
    /// `POST /api/v1/accounts` presents.
    pub(super) async fn app_token(&self) -> String {
        common::client_credentials_token(&self.app, &self.client).await
    }

    /// Registers `username` through the public API and hands back the
    /// account every later step acts as.
    pub(super) async fn register(&self, username: &str) -> Account {
        let token = common::register_via_api(&self.app, &self.client, username, PASSWORD).await;
        let read = common::api_get(&self.app, Some(&token), VERIFY).await;
        common::assert_answer(&read, StatusCode::OK, "the new account reads itself");
        Account {
            id: common::entity_id(&read),
            token,
        }
    }

    /// Registers `username` and then locks its account, exactly as the
    /// client that means to approve its followers does.
    pub(super) async fn register_locked(&self, username: &str) -> Account {
        let account = self.register(username).await;
        let response = common::api_patch(
            &self.app,
            Some(&account.token),
            CREDENTIALS,
            &json!({"locked": true}),
        )
        .await;
        common::assert_answer(&response, StatusCode::OK, "the lock write");
        assert_eq!(
            response.json_value()["locked"],
            json!(true),
            "the account answers locked: {}",
            response.json_value()
        );
        account
    }

    /// Logs `username` in with the `password` grant, asserting the pair.
    pub(super) async fn login_with(&self, username: &str, password: &str) -> String {
        let response = common::login_password(&self.app, &self.client, username, password).await;
        common::assert_answer(&response, StatusCode::OK, "the scenario login");
        common::access_token_of(&response)
    }

    /// Logs `username` in with [`PASSWORD`].
    pub(super) async fn login(&self, username: &str) -> String {
        self.login_with(username, PASSWORD).await
    }

    /// Drains the job queues the way the live worker would and reports how
    /// many jobs it ran.
    pub(super) async fn drain(&self) -> usize {
        common::run_all_jobs(&self.state).await
    }

    /// Every letter the drained mailer delivered, oldest first, each spelled
    /// `recipient subject body` — what the memory mailbox behind
    /// [`Self::state`] holds. The recipient leads because the queue is the
    /// database's and a leg's drain can deliver a letter an earlier leg
    /// queued for another account; the steps filter on it.
    pub(super) fn mailbox(&self) -> Vec<String> {
        self.state
            .mailbox()
            .expect("the test state carries a memory mailbox")
            .messages()
            .iter()
            .map(|letter| {
                format!(
                    "{} {} {}",
                    letter.to(),
                    letter.subject(),
                    letter.text_body()
                )
            })
            .collect()
    }
}

/// An account a scenario registered and acts as.
pub(super) struct Account {
    /// The id the API answers for it (decision 4's uuid text).
    pub(super) id: String,
    /// The bearer token it acts with.
    pub(super) token: String,
}

/// The address every scenario registration derives for `username`, the
/// spelling the mail steps quote when they ask for a letter by address.
pub(super) fn address_of(username: &str) -> String {
    format!("{username}@example.org")
}

/// The string field `key` of a JSON body.
///
/// # Panics
///
/// If the key is absent or holds something other than a string.
pub(super) fn string_field(body: &Value, key: &str) -> String {
    body[key]
        .as_str()
        .unwrap_or_else(|| panic!("the answer carries a string {key}: {body}"))
        .to_owned()
}

/// A refused answer asserted to carry `status` with `message`, for `who`.
pub(super) fn assert_refused(
    response: &TestResponse,
    status: StatusCode,
    message: &str,
    who: &str,
) {
    common::assert_answer(response, status, who);
    common::assert_error(response, message, who);
}

/// The current 6-digit TOTP code of `secret`, as an authenticator app
/// would produce it right now.
pub(super) fn current_code(secret: &str) -> String {
    totp::code_at(secret, chrono::Utc::now().timestamp()).expect("a generated secret decodes")
}

/// The `rel` of a `Link` header, as the request path it names.
///
/// # Panics
///
/// If the header is absent or carries no entry for `rel`.
pub(super) fn link_cursor(response: &TestResponse, rel: &str) -> String {
    let link = response
        .header("link")
        .unwrap_or_else(|| panic!("a paged answer carries a Link header: {rel}"))
        .to_owned();
    let tail = format!("rel=\"{rel}\"");
    let entry = link
        .split(", ")
        .find(|entry| entry.ends_with(&tail))
        .unwrap_or_else(|| panic!("the Link header names {rel}: {link}"));
    let url = entry
        .strip_prefix('<')
        .and_then(|rest| rest.split('>').next())
        .unwrap_or_else(|| panic!("a Link entry is a bracketed URL: {entry}"));
    url.strip_prefix(BASE)
        .map_or_else(|| url.to_owned(), str::to_owned)
}

/// The relationship `token`'s account holds to the account `target` names,
/// read through `GET /api/v1/accounts/relationships` and asserted 200.
///
/// # Panics
///
/// If the read is refused or answers nothing for `target`.
pub(super) async fn relationship(world: &World, token: &str, target: &str) -> Value {
    let response = common::api_get(
        &world.app,
        Some(token),
        &format!("{ACCOUNTS}/relationships?id%5B%5D={target}"),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the relationship read");
    let body = response.json_value();
    let entries = body
        .as_array()
        .unwrap_or_else(|| panic!("relationships are an array: {body}"));
    entries
        .iter()
        .find(|entry| entry["id"] == json!(target))
        .cloned()
        .unwrap_or_else(|| panic!("the answer names {target}: {body}"))
}

/// The relationship flags `expected` names, checked against `actual`, for
/// `who`. Every other flag is left alone: each step states the flags its
/// write moved, which is what a client reads.
///
/// # Panics
///
/// If a named flag differs, or `expected` is not an object.
pub(super) fn assert_flags(actual: &Value, expected: &Value, who: &str) {
    let flags = expected
        .as_object()
        .expect("the expected flags are a JSON object");
    for (key, want) in flags {
        assert_eq!(&actual[key], want, "{who}: the {key} flag is {actual}");
    }
}

/// The letters addressed to `address`, oldest first.
pub(super) fn letters_to(letters: &[String], address: &str) -> Vec<String> {
    letters
        .iter()
        .filter(|letter| letter.starts_with(address))
        .cloned()
        .collect()
}

/// The path of the first link in the letter whose text mentions `needle`.
///
/// # Panics
///
/// If no letter mentions `needle`, or the one that does carries no URL on
/// this instance's own origin.
pub(super) fn letter_path(mailbox: &[String], needle: &str) -> String {
    let letter = mailbox
        .iter()
        .find(|letter| letter.contains(needle))
        .unwrap_or_else(|| panic!("a letter should mention {needle}: {mailbox:?}"));
    let after_base = letter
        .split(BASE)
        .nth(1)
        .unwrap_or_else(|| panic!("the letter carries a {BASE} link: {letter}"));
    after_base
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect()
}

/// The same ids in lexicographic order: what a listing holds, whatever
/// order it answers in.
pub(super) fn sorted(ids: &[String]) -> Vec<String> {
    let mut copy = ids.to_vec();
    copy.sort();
    copy
}

/// The ids of the entities of an answer's array, in answer order.
///
/// # Panics
///
/// If the body is not an array of entities carrying string ids.
pub(super) fn entity_ids(response: &TestResponse) -> Vec<String> {
    let body = response.json_value();
    let entries = body
        .as_array()
        .unwrap_or_else(|| panic!("the listing is a JSON array: {body}"));
    entries
        .iter()
        .map(|entry| string_field(entry, "id"))
        .collect()
}

/// The config of an instance in the two registration modes `approval` and
/// `activation` names, with a relay configured so a letter really queues
/// and the drained mailbox really holds it.
pub(super) fn mode_config(approval: Toggle, activation: Toggle) -> Config {
    Config {
        registrations_approval_required: approval,
        account_activation_required: activation,
        smtp_host: Some("smtp.example.org".to_owned()),
        ..common::test_config()
    }
}

/// The scenario config with a relay configured: the switch that makes
/// [`deliver_later`](wired_be::mailer::deliver_later) queue a letter
/// instead of dropping it, which the mail-driven steps then drain.
pub(super) fn mail_config() -> Config {
    mode_config(Toggle::new(false), Toggle::new(false))
}

// ──────────────────────────  The loopback mock peer  ──────────────────────────

/// A mock `ActivityPub` peer on loopback serving one actor document: the
/// actor the domain-block step resolves and mirrors through the public API,
/// on a domain the scenario can then name.
pub(super) struct MockPeer {
    /// The `http://{addr}` origin it answers on.
    base: String,
    /// The task serving it, aborted when the scenario is done.
    server: JoinHandle<()>,
}

/// `zoe`'s self-identifying actor document at `id`, the shape the fetch
/// parser requires.
fn actor_document(id: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": id,
        "preferredUsername": "zoe",
        "name": "Zoe",
        "inbox": format!("{id}/inbox"),
        "publicKey": {
            "id": format!("{id}#main-key"),
            "owner": id,
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nPEM\n-----END PUBLIC KEY-----\n",
        },
    })
}

/// Answers the actor document with an `ActivityPub` content type.
async fn serve_actor(State(peer): State<String>) -> Response {
    ([(CONTENT_TYPE, "application/activity+json")], peer).into_response()
}

impl MockPeer {
    /// Binds a port and serves `zoe`'s actor at `{base}/users/zoe`.
    pub(super) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock peer binds a port");
        let base = format!("http://{}", listener.local_addr().expect("a bound address"));
        let document = actor_document(&format!("{base}/users/zoe")).to_string();
        let server = tokio::spawn(async move {
            let app = Router::new()
                .route("/users/zoe", get(serve_actor))
                .with_state(document);
            if let Err(err) = axum::serve(listener, app).await {
                tracing::debug!("mock peer stopped: {err}");
            }
        });
        Self { base, server }
    }

    /// The actor URL this peer serves.
    pub(super) fn actor_url(&self) -> String {
        format!("{}/users/zoe", self.base)
    }

    /// The `host:port` authority of [`Self::actor_url`]: the domain the
    /// mirror of this peer's actor is stored under, port included.
    pub(super) fn authority(&self) -> String {
        self.base
            .strip_prefix("http://")
            .expect("the mock origin is loopback http")
            .to_owned()
    }
}

impl Drop for MockPeer {
    fn drop(&mut self) {
        self.server.abort();
    }
}
