//! End-to-end account, auth and social-graph scenarios (WBE-222): seven
//! lifecycles walked through the public HTTP API alone. Every step is a
//! request through the test router and every check is what a client can
//! see. The one fixture a scenario plants with a model call is the first
//! admin account — no public endpoint mints an admin — and even its
//! credential is then won over HTTP by the password grant.
//!
//! The scenarios:
//!
//! 1. **Onboarding**: `POST /api/v1/apps` → the `client_credentials`
//!    token → `POST /api/v1/accounts` → `verify_credentials` →
//!    `update_credentials` (display name, bio, one metadata field and an
//!    uploaded avatar) → the per-app settings store the account carries →
//!    the public account read showing it all → the password change that
//!    revokes the account's other credential → a login with the new one.
//! 2. **The OAuth code flow**: the consent page, the approved
//!    submission's code, that code's exchange, the rotation, the
//!    revocation, and the approved scope deciding what the token may do.
//! 3. **MFA**: TOTP setup, confirmation and backup codes, the
//!    `mfa_required` challenge the password grant answers with, the
//!    challenge that hands out the withheld pair, and the disable that
//!    opens the plain grant again.
//! 4. **The social graph**: an open follow, a locked account's pending
//!    request and its authorization, the block and its unblock, a timed
//!    mute, a domain block, an endorsement, a private note, a list with
//!    members, and the following page walked by its `Link` cursors.
//! 5. **The registration modes**: approval required until staff approve
//!    through the admin API, an invite token opening a closed instance,
//!    the confirmation letter the drained mailer leaves in the memory
//!    mailbox, and a password reset through its emailed token.
//! 6. **Admin user management**: create, tag, promote, rewrite the
//!    credentials of, deactivate, re-activate and delete an account, each
//!    effect read back through the user-facing API.
//! 7. **CSV imports**: the follow, block and mute lists the import
//!    endpoints queue, run through [`common::run_all_jobs`], landing the
//!    relationships they promise.
//!
//! The per-endpoint refusals behind these journeys are pinned by the area
//! files (`account_registration.rs`, `oauth.rs`, `mfa.rs`,
//! `registration_modes.rs`, `admin_users.rs`, `pleroma_import.rs`, …);
//! like `e2e_scheduled.rs`, this module owns the *seams* — the state one
//! endpoint leaves behind for the next, and what the API answers about it.

#![cfg(test)]

use axum::{
    Router,
    body::Bytes,
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
const PASSWORD: &str = "correct horse battery staple";
/// The password the onboarding scenario changes to.
const NEW_PASSWORD: &str = "a new and longer password";
/// The scopes every scenario client registers with, and so every token it
/// hands out: the whole client set plus the admin pair, so one client
/// covers every step of every journey.
const CLIENT_SCOPES: &str = "read write follow push admin:read admin:write";
/// The instance origin [`common::test_config`] serves on.
const BASE: &str = wired_be::config::TEST_BASE_URL;
/// The account root every read and relationship write hangs under.
const ACCOUNTS: &str = "/api/v1/accounts";
/// The caller's own account, as `verify_credentials` answers it.
const VERIFY: &str = "/api/v1/accounts/verify_credentials";
/// The profile write's own route.
const CREDENTIALS: &str = "/api/v1/accounts/update_credentials";
/// The status route, as the scope gate's probe.
const STATUSES: &str = "/api/v1/statuses";
/// The token endpoint every grant and rotation runs through.
const TOKEN: &str = "/oauth/token";
/// The revocation endpoint.
const REVOKE: &str = "/oauth/revoke";
/// The consent endpoint pair.
const AUTHORIZE: &str = "/oauth/authorize";
/// The second factor's challenge endpoint.
const CHALLENGE: &str = "/oauth/mfa/challenge";
/// The MFA settings root.
const MFA: &str = "/api/pleroma/accounts/mfa";
/// The Pleroma admin users root.
const ADMIN_USERS: &str = "/api/v1/pleroma/admin/users";
/// The settings store of the scenario's own client, keyed by app name.
const SETTINGS: &str = "/api/v1/pleroma/settings/mobile";
/// The 401 body of a credential that no longer resolves.
const INVALID_TOKEN: &str = "The access token is invalid";
/// The 400 body every credential and code failure shares.
const INVALID_CREDENTIALS: &str = "Invalid credentials";
/// The 403 body of the bearer gate's own refusal of a disabled account.
const DISABLED: &str = "Your account is currently disabled";
/// The 404 body of the client API's misses.
const NOT_FOUND: &str = "Record not found";
/// The refusal wording of a challenge that cannot be passed.
const INVALID_CODE: &str = "Invalid code";
/// The multipart boundary the profile-image step frames its body with.
const BOUNDARY: &str = "wired-e2e-accounts-boundary";
/// A `PNG`: the signature the upload detector names, and nothing more —
/// a profile image is never decoded.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
/// The seconds the timed mute asks for: one hour, which the listing hands
/// back as an expiry an hour out.
const MUTE_SECONDS: u32 = 3600;
/// The private note one account writes on another.
const NOTE: &str = "reads every thread twice";
/// The title of the list the social-graph scenario creates.
const LIST: &str = "Circles";
/// A well-formed id that names no account: the member the list refuses.
const UNKNOWN_ID: &str = "00000000-0000-4000-8000-000000000000";

/// One scenario's world: the router every step rides, the [`AppState`]
/// behind it (which the job drain and the memory mailbox read), and the
/// client the scenario registered over HTTP as its very first call.
struct World {
    /// The application under test.
    app: Router,
    /// The state the router and [`drain`](Self::drain) share.
    state: AppState,
    /// The OAuth client `POST /api/v1/apps` answered.
    client: TestClient,
    /// The scratch uploads directory the profile images land in; the
    /// world's presence is what keeps the files readable. Named with a
    /// leading underscore because nothing reads it — dropping it is the
    /// whole job.
    _uploads: TempDir,
}

impl World {
    /// The world over `pool` with `config`: uploads moved into a scratch
    /// directory, and the client registered as the scenario's first move.
    async fn start(pool: PgPool, mut config: Config) -> Self {
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
    async fn open(pool: PgPool) -> Self {
        Self::start(pool, common::test_config()).await
    }

    /// The app-only bearer of [`Self::client`]: the credential
    /// `POST /api/v1/accounts` presents.
    async fn app_token(&self) -> String {
        common::client_credentials_token(&self.app, &self.client).await
    }

    /// Registers `username` through the public API and hands back the
    /// account every later step acts as.
    async fn register(&self, username: &str) -> Account {
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
    async fn register_locked(&self, username: &str) -> Account {
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
    async fn login_with(&self, username: &str, password: &str) -> String {
        let response = common::login_password(&self.app, &self.client, username, password).await;
        common::assert_answer(&response, StatusCode::OK, "the scenario login");
        common::access_token_of(&response)
    }

    /// Logs `username` in with [`PASSWORD`].
    async fn login(&self, username: &str) -> String {
        self.login_with(username, PASSWORD).await
    }

    /// Drains the job queues the way the live worker would and reports how
    /// many jobs it ran.
    async fn drain(&self) -> usize {
        common::run_all_jobs(&self.state).await
    }

    /// Every letter the drained mailer delivered, oldest first, each spelled
    /// `recipient subject body` — what the memory mailbox behind
    /// [`Self::state`] holds. The recipient leads because the queue is the
    /// database's and a leg's drain can deliver a letter an earlier leg
    /// queued for another account; the steps filter on it.
    fn mailbox(&self) -> Vec<String> {
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
struct Account {
    /// The id the API answers for it (decision 4's uuid text).
    id: String,
    /// The bearer token it acts with.
    token: String,
}

/// The address every scenario registration derives for `username`, the
/// spelling the mail steps quote when they ask for a letter by address.
fn address_of(username: &str) -> String {
    format!("{username}@example.org")
}

/// The string field `key` of a JSON body.
///
/// # Panics
///
/// If the key is absent or holds something other than a string.
fn string_field(body: &Value, key: &str) -> String {
    body[key]
        .as_str()
        .unwrap_or_else(|| panic!("the answer carries a string {key}: {body}"))
        .to_owned()
}

/// A refused answer asserted to carry `status` with `message`, for `who`.
fn assert_refused(response: &TestResponse, status: StatusCode, message: &str, who: &str) {
    common::assert_answer(response, status, who);
    common::assert_error(response, message, who);
}

/// The current 6-digit TOTP code of `secret`, as an authenticator app
/// would produce it right now.
fn current_code(secret: &str) -> String {
    totp::code_at(secret, chrono::Utc::now().timestamp()).expect("a generated secret decodes")
}

/// The `rel` of a `Link` header, as the request path it names.
///
/// # Panics
///
/// If the header is absent or carries no entry for `rel`.
fn link_cursor(response: &TestResponse, rel: &str) -> String {
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
async fn relationship(world: &World, token: &str, target: &str) -> Value {
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
fn assert_flags(actual: &Value, expected: &Value, who: &str) {
    let flags = expected
        .as_object()
        .expect("the expected flags are a JSON object");
    for (key, want) in flags {
        assert_eq!(&actual[key], want, "{who}: the {key} flag is {actual}");
    }
}

/// The letters addressed to `address`, oldest first.
fn letters_to(letters: &[String], address: &str) -> Vec<String> {
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
fn letter_path(mailbox: &[String], needle: &str) -> String {
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
fn sorted(ids: &[String]) -> Vec<String> {
    let mut copy = ids.to_vec();
    copy.sort();
    copy
}

/// The ids of the entities of an answer's array, in answer order.
///
/// # Panics
///
/// If the body is not an array of entities carrying string ids.
fn entity_ids(response: &TestResponse) -> Vec<String> {
    let body = response.json_value();
    let entries = body
        .as_array()
        .unwrap_or_else(|| panic!("the listing is a JSON array: {body}"));
    entries
        .iter()
        .map(|entry| string_field(entry, "id"))
        .collect()
}

// ─────────────────────  Scenario 1: the onboarding walk  ─────────────────────

/// The onboarding scenario end to end: a client registers itself, opens an
/// account with the credential that answers, finishes the profile — display
/// name, note, one metadata field, an uploaded avatar — sees the whole
/// result on the public read, changes the password, which revokes the
/// account's *other* credential, and signs in again with the new password.
#[tokio::test]
async fn onboarding_walks_from_app_registration_to_a_new_password() {
    let db = common::test_db().await;
    let world = World::open(db.pool().clone()).await;

    // Step 1: the client's own credential reads itself, without its secret.
    app_reads_itself(&world).await;

    // Step 2: registration over the app-only bearer, and the credential it
    // hands out already authenticates the new account.
    let alice = world.register("alice").await;
    let fresh = common::api_get(&world.app, Some(&alice.token), VERIFY).await;
    assert_eq!(
        string_field(&fresh.json_value(), "username"),
        "alice",
        "the token is the new account's own"
    );
    assert_eq!(
        fresh.json_value()["note"],
        "",
        "and the profile is still blank"
    );

    // Step 3: the profile write, in one multipart body of text and file.
    let written = finish_profile(&world, &alice).await;

    // Step 4: the client's own settings store, which the account carries.
    settings_round_trip(&world, &alice).await;

    // Step 5: the public read shows exactly what the owner stored.
    profile_is_public(&world, &alice, &written).await;

    // Step 6: the password change revokes the other session, keeps this
    // one, and the old password stops opening anything.
    password_change_revokes_the_other_session(&world, &alice).await;

    // Step 7: the new password logs in.
    let again = world.login_with("alice", NEW_PASSWORD).await;
    let read = common::api_get(&world.app, Some(&again), VERIFY).await;
    common::assert_answer(&read, StatusCode::OK, "the login with the new password");
    assert_eq!(
        string_field(&read.json_value(), "username"),
        "alice",
        "the same account, freshly signed in"
    );
}

/// `GET /api/v1/apps/verify_credentials` with the client's own
/// `client_credentials` bearer: the application behind the token, with no
/// secret on the wire.
async fn app_reads_itself(world: &World) {
    let bearer = world.app_token().await;
    let response =
        common::api_get(&world.app, Some(&bearer), "/api/v1/apps/verify_credentials").await;
    common::assert_answer(&response, StatusCode::OK, "the app's own read");
    let body = response.json_value();
    assert_eq!(
        body["name"], "Scenario Client",
        "the entity names the client the registration stated: {body}"
    );
    assert!(
        body.get("client_secret").is_none(),
        "and `verify_credentials` keeps the secret off the wire: {body}"
    );
}

/// The `PATCH /api/v1/accounts/update_credentials` walk: display name,
/// note, one metadata field and an uploaded avatar in a single multipart
/// body, answered as the owner entity that now carries all four.
async fn finish_profile(world: &World, alice: &Account) -> Value {
    let response = common::TestRequest::patch(CREDENTIALS)
        .bearer(&alice.token)
        .multipart(
            BOUNDARY,
            &[
                common::MultipartPart::text("display_name", "Ada Lovelace"),
                common::MultipartPart::text("note", "analysing the engine"),
                common::MultipartPart::text("fields_attributes[0][name]", "Web"),
                common::MultipartPart::text("fields_attributes[0][value]", "https://ada.example"),
                common::MultipartPart::file(
                    "avatar",
                    "ada.png",
                    "image/png",
                    Bytes::from_static(PNG),
                ),
            ],
        )
        .send(&world.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the profile write");
    let entity = response.json_value();
    assert_eq!(
        entity["display_name"], "Ada Lovelace",
        "the display name lands: {entity}"
    );
    assert_eq!(
        entity["note"], "<p>analysing the engine</p>",
        "the note is stored and rendered"
    );
    assert_eq!(
        entity["fields"][0]["name"], "Web",
        "the metadata field arrives with its label"
    );
    assert_eq!(
        entity["source"]["fields"][0]["value"], "https://ada.example",
        "and its raw text reads back beside the render"
    );
    let avatar = string_field(&entity, "avatar");
    assert!(
        avatar.starts_with(&format!("{BASE}/media/")),
        "the uploaded avatar is served from this instance: {avatar}"
    );
    entity
}

/// The client's own settings store, walked as `PATCH`/`GET
/// /api/v1/pleroma/settings/{app}`: nothing stored at first, the merged
/// object after each write, and the whole per-app store on the account's
/// own `verify_credentials`.
async fn settings_round_trip(world: &World, alice: &Account) {
    let empty = common::api_get(&world.app, Some(&alice.token), SETTINGS).await;
    common::assert_answer(&empty, StatusCode::OK, "the first settings read");
    assert_eq!(
        empty.json_value(),
        json!({}),
        "a new account stores nothing"
    );

    let patched = common::api_patch(
        &world.app,
        Some(&alice.token),
        SETTINGS,
        &json!({"theme": "dark", "timeline": {"show_reblogs": false}}),
    )
    .await;
    common::assert_answer(&patched, StatusCode::OK, "the first settings write");
    assert_eq!(
        patched.json_value(),
        json!({"theme": "dark", "timeline": {"show_reblogs": false}}),
        "the write answers the object it merged"
    );

    let merged = common::api_patch(
        &world.app,
        Some(&alice.token),
        SETTINGS,
        &json!({"timeline": {"muted_words": ["now"]}}),
    )
    .await;
    common::assert_answer(&merged, StatusCode::OK, "the deep merge");
    assert_eq!(
        merged.json_value(),
        json!({"theme": "dark", "timeline": {"show_reblogs": false, "muted_words": ["now"]}}),
        "a nested write joins what is stored instead of replacing it"
    );
    let read = common::api_get(&world.app, Some(&alice.token), SETTINGS).await;
    common::assert_answer(&read, StatusCode::OK, "the settings read back");
    assert_eq!(read.json_value(), merged.json_value(), "the same object");

    let owner = common::api_get(&world.app, Some(&alice.token), VERIFY).await;
    common::assert_answer(&owner, StatusCode::OK, "the owner's account read");
    assert_eq!(
        owner.json_value()["pleroma"]["settings_store"]["mobile"],
        merged.json_value(),
        "and the store rides the account entity under its app"
    );
}

/// What the profile write stored is what anyone reads: the public account
/// route carries the same name, bio and field, and the avatar URL it names
/// serves the uploaded bytes.
async fn profile_is_public(world: &World, alice: &Account, written: &Value) {
    let shown = common::api_get(&world.app, None, &format!("{ACCOUNTS}/{}", alice.id)).await;
    common::assert_answer(&shown, StatusCode::OK, "the public account read");
    let entity = shown.json_value();
    assert_eq!(entity["display_name"], written["display_name"], "the name");
    assert_eq!(entity["note"], written["note"], "the bio");
    assert_eq!(entity["fields"], written["fields"], "the metadata");
    let avatar = string_field(&entity, "avatar");
    let served = common::api_get(
        &world.app,
        None,
        avatar.strip_prefix(BASE).unwrap_or(&avatar),
    )
    .await;
    common::assert_answer(&served, StatusCode::OK, "the stored avatar file");
    assert_eq!(served.body.as_ref(), PNG, "the uploaded bytes");
}

/// The password change and what it costs the account's other credentials:
/// a second session's token dies, the credential that ran the change keeps
/// working (Pleroma's whitelist: that session's password is what the user
/// just typed), and the old password opens nothing.
async fn password_change_revokes_the_other_session(world: &World, alice: &Account) {
    let other = world.login("alice").await;
    let changed = common::api_post(
        &world.app,
        Some(&alice.token),
        "/api/pleroma/change_password",
        &json!({
            "password": PASSWORD,
            "new_password": NEW_PASSWORD,
            "new_password_confirmation": NEW_PASSWORD,
        }),
    )
    .await;
    common::assert_answer(&changed, StatusCode::OK, "the password change");
    assert_eq!(
        changed.json_value()["status"],
        "success",
        "Pleroma's success body"
    );

    let revoked = common::api_get(&world.app, Some(&other), VERIFY).await;
    assert_refused(
        &revoked,
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "the other session's token",
    );
    let keeper = common::api_get(&world.app, Some(&alice.token), VERIFY).await;
    common::assert_answer(&keeper, StatusCode::OK, "the credential the change ran on");
    let stale = common::login_password(&world.app, &world.client, "alice", PASSWORD).await;
    assert_refused(
        &stale,
        StatusCode::BAD_REQUEST,
        INVALID_CREDENTIALS,
        "the old password after the change",
    );
}

// ───────────────────  Scenario 2: the OAuth code flow  ───────────────────

/// The authorization-code flow end to end: the consent page, the approved
/// submission's redirect, the code's exchange for a pair carrying the
/// approved scopes, the spent code's refusal, the rotation that replaces
/// the pair, and the revocation that ends it — with the scope gate proved
/// on the way: a `read` token may read and may not post.
#[tokio::test]
async fn the_authorization_code_flow_issues_rotates_and_retires_a_token() {
    let db = common::test_db().await;
    let world = World::open(db.pool().clone()).await;
    let _bob = world.register("bob").await;

    // Step 1: the consent page renders for a valid request.
    let page = common::api_get(&world.app, None, &consent_url(&world.client, "read+write")).await;
    common::assert_answer(&page, StatusCode::OK, "the consent page");
    let html = page.text();
    assert!(
        html.contains("<form method=\"post\" action=\"/oauth/authorize\">"),
        "the page posts its consent back to the same route: {html}"
    );

    // Step 2: the approved submission redirects to the client with a code.
    let code = consent(&world, "bob", &["read"]).await;

    // Step 3: the code is redeemed for a pair acting as its authorizer,
    // with the *approved* scopes and nothing else.
    let exchanged = grant(
        &world,
        &json!({"grant_type": "authorization_code", "code": code}),
    )
    .await;
    common::assert_answer(&exchanged, StatusCode::OK, "the code exchange");
    let pair = exchanged.json_value();
    assert_eq!(pair["scope"], "read", "only the box that was checked");
    assert!(
        !pair["refresh_token"].is_null(),
        "a user-bound pair carries its refresh token: {pair}"
    );
    let access = common::access_token_of(&exchanged);

    // Step 4: that token reads the account it was granted for.
    let read = common::api_get(&world.app, Some(&access), VERIFY).await;
    common::assert_answer(&read, StatusCode::OK, "the code token reads");
    assert_eq!(
        string_field(&read.json_value(), "username"),
        "bob",
        "as the account that consented"
    );

    // Step 5: and it may not post, because `read` is the whole grant.
    let refused = common::api_post(
        &world.app,
        Some(&access),
        STATUSES,
        &json!({"status": "should not land"}),
    )
    .await;
    assert_refused(
        &refused,
        StatusCode::FORBIDDEN,
        "Insufficient permissions: write:statuses.",
        "a read-only token posting a status",
    );

    // Step 6: the same code is spent.
    let replay = grant(
        &world,
        &json!({"grant_type": "authorization_code", "code": code}),
    )
    .await;
    assert_refused(
        &replay,
        StatusCode::BAD_REQUEST,
        INVALID_CREDENTIALS,
        "a replayed code",
    );

    // Step 7: the refresh rotates the pair, and the token it replaces dies.
    let rotated = grant(
        &world,
        &json!({"grant_type": "refresh_token", "refresh_token": pair["refresh_token"]}),
    )
    .await;
    common::assert_answer(&rotated, StatusCode::OK, "the refresh grant");
    let next = common::access_token_of(&rotated);
    assert_ne!(next, access, "a fresh access token, not the old one");
    let dead = common::api_get(&world.app, Some(&access), VERIFY).await;
    assert_refused(
        &dead,
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "the rotated-out access token",
    );

    // Step 8: revoking the live one ends it too.
    let revoked = grant_at(&world, REVOKE, &json!({"token": next})).await;
    common::assert_answer(&revoked, StatusCode::OK, "the revocation");
    assert_eq!(revoked.json_value(), json!({}), "RFC 7009's empty body");
    let gone = common::api_get(&world.app, Some(&next), VERIFY).await;
    assert_refused(
        &gone,
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "the revoked access token",
    );
}

/// The consent URL for `client`, asking `scope` and delivering to the
/// redirect URI it registered.
fn consent_url(client: &TestClient, scope: &str) -> String {
    let redirect = url::form_urlencoded::byte_serialize(common::TEST_REDIRECT_URI.as_bytes())
        .collect::<String>();
    format!(
        "{AUTHORIZE}?response_type=code&client_id={}&redirect_uri={redirect}&scope={scope}",
        client.client_id
    )
}

/// `POST /oauth/token` (or [`REVOKE`]) with `body` and the client's own
/// credentials merged in.
async fn grant(world: &World, body: &Value) -> TestResponse {
    grant_at(world, TOKEN, body).await
}

/// `POST`s `path` with the client credentials beside `body` — the shape
/// every `/oauth` endpoint but the consent form takes.
async fn grant_at(world: &World, path: &str, body: &Value) -> TestResponse {
    common::TestRequest::post(path)
        .json(&world.client.with_credentials(body.clone()))
        .send(&world.app)
        .await
}

/// The approved `POST /oauth/authorize` for `username`, checking the boxes
/// in `scopes`, answered as the code its redirect carries.
///
/// The form carries the login, not a bearer: this API has no browser
/// session, so the consent submission authenticates the account it
/// approves — the same [`PASSWORD`] the scenario registered it with.
///
/// # Panics
///
/// If the submission is not the 302, or its redirect carries no `code`.
async fn consent(world: &World, username: &str, scopes: &[&str]) -> String {
    let mut fields = vec![
        ("authorization[client_id]", world.client.client_id.as_str()),
        ("authorization[redirect_uri]", common::TEST_REDIRECT_URI),
        ("authorization[name]", username),
        ("authorization[password]", PASSWORD),
    ];
    for scope in scopes {
        fields.push(("authorization[scope][]", scope));
    }
    let response = common::TestRequest::post(AUTHORIZE)
        .form(&fields)
        .send(&world.app)
        .await;
    common::assert_answer(&response, StatusCode::FOUND, "the consent submission");
    let location = response
        .header("location")
        .expect("the approval redirects to the client")
        .to_owned();
    let (_, query) = location.split_once('?').unwrap_or((location.as_str(), ""));
    let pairs = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<Vec<(String, String)>>();
    let (_key, code) = pairs
        .iter()
        .find(|(key, _)| key == "code")
        .unwrap_or_else(|| panic!("the redirect carries a code: {location}"));
    code.clone()
}

// ──────────────────────────  Scenario 3: MFA  ──────────────────────────

/// The MFA lifecycle end to end: nothing is enabled, setup mints a secret,
/// the authenticator's code confirms it, five backup codes are issued, the
/// password grant then answers the `mfa_required` challenge instead of a
/// token, the challenge with a computed code hands out the pair the grant
/// withheld — a recovery code does the same and spends itself — and
/// removing TOTP opens the plain grant again.
#[tokio::test]
async fn mfa_gates_the_password_grant_until_it_is_disabled() {
    let db = common::test_db().await;
    let world = World::open(db.pool().clone()).await;
    let carol = world.register("carol").await;

    // Step 1: a fresh account has no second factor.
    let before = mfa_settings(&world, &carol).await;
    assert_eq!(
        before["settings"],
        json!({"enabled": false, "totp": false}),
        "MFA starts off and unconfirmed"
    );

    // Step 2: setup mints the secret, and the code the secret gives now
    // confirms it.
    let secret = mfa_setup(&world, &carol).await;
    let confirmed = common::TestRequest::post(&format!("{MFA}/confirm/totp"))
        .bearer(&carol.token)
        .form(&[("password", PASSWORD), ("code", &current_code(&secret))])
        .send(&world.app)
        .await;
    common::assert_answer(&confirmed, StatusCode::OK, "the TOTP confirmation");
    let after = mfa_settings(&world, &carol).await;
    assert_eq!(
        after["settings"],
        json!({"enabled": true, "totp": true}),
        "the method is enabled and confirmed"
    );

    // Step 3: recovery codes, five of them, new to this issuance.
    let codes = backup_codes(&world, &carol).await;
    assert_eq!(codes.len(), 5, "Pleroma issues five backup codes");

    // Step 4: the password grant now answers the challenge, not a token.
    let challenge = mfa_challenge(&world, "carol").await;

    // Step 5: the second factor answers the pair the grant withheld.
    let token = pass_challenge(&world, &challenge, "totp", &current_code(&secret)).await;
    let read = common::api_get(&world.app, Some(&token), VERIFY).await;
    common::assert_answer(&read, StatusCode::OK, "the challenged token");
    assert_eq!(
        string_field(&read.json_value(), "username"),
        "carol",
        "acting as its owner"
    );

    // Step 6: a recovery code carries a challenge too, and is spent by it.
    let second = mfa_challenge(&world, "carol").await;
    let recovered = pass_challenge(&world, &second, "recovery", &codes[0]).await;
    let again = common::api_get(&world.app, Some(&recovered), VERIFY).await;
    common::assert_answer(&again, StatusCode::OK, "the backup-code token");
    let reused = pass_challenge_attempt(&world, &second, "recovery", &codes[0]).await;
    assert_refused(
        &reused,
        StatusCode::BAD_REQUEST,
        INVALID_CODE,
        "a spent backup code",
    );

    // Step 7: removing TOTP closes the challenge and opens the grant.
    let disabled = common::api_delete(
        &world.app,
        Some(&carol.token),
        &format!("{MFA}/totp"),
        &json!({"password": PASSWORD}),
    )
    .await;
    common::assert_answer(&disabled, StatusCode::OK, "the TOTP removal");
    let quiet = mfa_settings(&world, &carol).await;
    assert_eq!(
        quiet["settings"]["enabled"], false,
        "the switch is back off, and no method is confirmed"
    );
    let open = world.login("carol").await;
    let unguarded = common::api_get(&world.app, Some(&open), VERIFY).await;
    common::assert_answer(&unguarded, StatusCode::OK, "the unguarded login");
    assert_eq!(
        string_field(&unguarded.json_value(), "username"),
        "carol",
        "the same account, no challenge"
    );
}

/// `GET /api/pleroma/accounts/mfa` as `account`, answered as its settings.
///
/// # Panics
///
/// If the read is refused.
async fn mfa_settings(world: &World, account: &Account) -> Value {
    let response = common::api_get(&world.app, Some(&account.token), MFA).await;
    common::assert_answer(&response, StatusCode::OK, "the MFA settings read");
    response.json_value()
}

/// `GET …/setup/totp` as `account`, answered as the minted secret key.
///
/// # Panics
///
/// If setup is refused or answers no key.
async fn mfa_setup(world: &World, account: &Account) -> String {
    let response = common::api_get(
        &world.app,
        Some(&account.token),
        &format!("{MFA}/setup/totp"),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the TOTP setup");
    string_field(&response.json_value(), "key")
}

/// `GET …/backup_codes` as `account`, answered as the issued codes.
///
/// # Panics
///
/// If the read is refused or answers no list of strings.
async fn backup_codes(world: &World, account: &Account) -> Vec<String> {
    let response = common::api_get(
        &world.app,
        Some(&account.token),
        &format!("{MFA}/backup_codes"),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the backup-code issue");
    serde_json::from_value(response.json_value()["backup_codes"].clone())
        .expect("backup_codes is a list of strings")
}

/// The `password` grant for `username` while MFA is on: the 403 challenge,
/// answered as its `mfa_token`.
///
/// # Panics
///
/// If the grant answers anything other than the challenge.
async fn mfa_challenge(world: &World, username: &str) -> String {
    let refused = common::login_password(&world.app, &world.client, username, PASSWORD).await;
    assert_refused(
        &refused,
        StatusCode::FORBIDDEN,
        "mfa_required",
        "the password grant of an MFA account",
    );
    let body = refused.json_value();
    assert_eq!(
        body["supported_challenge_types"], "totp",
        "totp is the only method this server speaks: {body}"
    );
    assert!(
        body.get("access_token").is_none(),
        "and no token rides the challenge: {body}"
    );
    string_field(&body, "mfa_token")
}

/// `POST /oauth/mfa/challenge` with `code` of type `kind`, asserted 200 and
/// answered as the token pair.
///
/// # Panics
///
/// If the challenge is refused.
async fn pass_challenge(world: &World, challenge: &str, kind: &str, code: &str) -> String {
    let response = pass_challenge_attempt(world, challenge, kind, code).await;
    common::assert_answer(&response, StatusCode::OK, "the MFA challenge");
    common::access_token_of(&response)
}

/// `POST /oauth/mfa/challenge`, answered raw: the same call, no assertion.
async fn pass_challenge_attempt(
    world: &World,
    challenge: &str,
    kind: &str,
    code: &str,
) -> TestResponse {
    common::TestRequest::post(CHALLENGE)
        .form(&[
            ("mfa_token", challenge),
            ("challenge_type", kind),
            ("code", code),
            ("client_id", world.client.client_id.as_str()),
            ("client_secret", world.client.client_secret.as_str()),
        ])
        .send(&world.app)
        .await
}

// ───────────────────────  Scenario 4: the social graph  ───────────────────────

/// The social graph end to end: an open follow granted on the spot, a
/// locked account's request answered from its own queue, a block that
/// severs and an unblock that does not restore, a mute that carries its
/// expiry, a domain block that reaches a remote account alone, an
/// endorsement, a private note, a list with its members, and the follower
/// and following pages walked by the `Link` cursors they answer with.
#[tokio::test]
async fn the_social_graph_walks_follows_blocks_mutes_lists_and_pages() {
    let db = common::test_db().await;
    let world = World::open(db.pool().clone()).await;
    let alice = world.register("alice").await;
    let bob = world.register("bob").await;
    let dave = world.register("dave").await;
    let carol = world.register_locked("carol").await;

    // Step 1: an unlocked account grants, a locked one receives a request.
    follow_state(&world, &alice, &bob, json!({"following": true})).await;
    follow_state(&world, &alice, &dave, json!({"following": true})).await;
    follow_state(&world, &alice, &carol, json!({"requested": true})).await;

    // Step 2: carol reads her queue and answers it; alice now follows her.
    requested_then_granted(&world, &alice, &carol).await;

    // Step 3: the block severs the open follow; the unblock is not its undo.
    block_then_unblock(&world, &alice, &bob).await;

    // Step 4: a mute with a duration carries its expiry on the listing.
    timed_mute(&world, &alice, &bob).await;

    // Step 5: the endorsement and the private note, on the followed account.
    endorsed_then_noted(&world, &alice, &carol).await;

    // Step 6: a list, and its members both ways round.
    list_with_members(&world, &alice, &carol).await;

    // Step 7: the domain block, whose flag only a remote account raises.
    domain_block(&world, &alice, &bob).await;

    // Step 8: the follower and following pages under their `Link` cursors.
    follower_pages(&world, &alice, &bob, &carol, &dave).await;
}

/// The request carol holds from alice, and her answer to it: the queue
/// names alice alone, the authorization answers `followed_by`, and alice's
/// own read of the pair is a granted follow from then on.
async fn requested_then_granted(world: &World, alice: &Account, carol: &Account) {
    let queue = common::api_get(&world.app, Some(&carol.token), "/api/v1/follow_requests").await;
    common::assert_answer(&queue, StatusCode::OK, "the follow-request queue");
    assert_eq!(
        entity_ids(&queue),
        vec![alice.id.clone()],
        "the locked account's queue holds alice's request and nothing else"
    );
    let answered = common::api_post(
        &world.app,
        Some(&carol.token),
        &format!("/api/v1/follow_requests/{}/authorize", alice.id),
        &Value::Null,
    )
    .await;
    common::assert_answer(&answered, StatusCode::OK, "the authorization");
    assert_eq!(
        answered.json_value()["followed_by"],
        json!(true),
        "carol is followed by alice now"
    );
    follow_state(world, alice, carol, json!({"following": true})).await;
}

/// The block, its listing, and the unblock that leaves the severed follow
/// severed: Mastodon's relationship flags are what a client sees, and the
/// pair's `following` flag does not come back on its own.
async fn block_then_unblock(world: &World, alice: &Account, bob: &Account) {
    let blocked = write(world, alice, bob, "block").await;
    common::assert_answer(&blocked, StatusCode::OK, "the block write");
    assert_flags(
        &blocked.json_value(),
        &json!({"blocking": true, "following": false}),
        "alice's relationship to the account she blocked",
    );
    let blocks = common::api_get(&world.app, Some(&alice.token), "/api/v1/blocks").await;
    common::assert_answer(&blocks, StatusCode::OK, "the blocks listing");
    assert_eq!(
        entity_ids(&blocks),
        vec![bob.id.clone()],
        "the listing names the blocked account"
    );
    let unblocked = write(world, alice, bob, "unblock").await;
    common::assert_answer(&unblocked, StatusCode::OK, "the unblock write");
    assert_flags(
        &unblocked.json_value(),
        &json!({"blocking": false, "following": false}),
        "unblocked, and the follow stayed severed",
    );
}

/// The follow write as `follower`, asserted to answer the flags in
/// `granted` beside the pair's default shape.
async fn follow_state(world: &World, follower: &Account, target: &Account, granted: Value) {
    let response = write(world, follower, target, "follow").await;
    common::assert_answer(&response, StatusCode::OK, "the follow write");
    assert_flags(
        &response.json_value(),
        &granted,
        &format!("the follow of {}", target.id),
    );
    let read = relationship(world, &follower.token, &target.id).await;
    assert_flags(
        &read,
        &granted,
        "the same pair read back through `relationships`",
    );
}

/// The body-less relationship write `verb` on `target` as `actor`.
async fn write(world: &World, actor: &Account, target: &Account, verb: &str) -> TestResponse {
    common::api_post(
        &world.app,
        Some(&actor.token),
        &format!("{ACCOUNTS}/{}/{verb}", target.id),
        &Value::Null,
    )
    .await
}

/// The mute with a `duration` and the notifications parameter off, and the
/// expiry it leaves on the caller's mutes page.
async fn timed_mute(world: &World, alice: &Account, bob: &Account) {
    let muted = common::api_post(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/mute", bob.id),
        &json!({"duration": MUTE_SECONDS, "notifications": false}),
    )
    .await;
    common::assert_answer(&muted, StatusCode::OK, "the timed mute");
    assert_flags(
        &muted.json_value(),
        &json!({"muting": true, "muting_notifications": false}),
        "the mute's own relationship answer, with its parameters read",
    );
    let listing = common::api_get(&world.app, Some(&alice.token), "/api/v1/mutes").await;
    common::assert_answer(&listing, StatusCode::OK, "the mutes listing");
    assert_eq!(
        entity_ids(&listing),
        vec![bob.id.clone()],
        "the listing names the muted account"
    );
    assert_ne!(
        listing.json_value()[0]["mute_expires_at"],
        Value::Null,
        "and carries the expiry the duration computed"
    );
    let unmuted = write(world, alice, bob, "unmute").await;
    common::assert_answer(&unmuted, StatusCode::OK, "the mute withdrawal");
    assert_flags(
        &unmuted.json_value(),
        &json!({"muting": false, "muting_notifications": false}),
        "the withdrawn mute",
    );
}

/// The endorsement of a followed account and its listing, the private note
/// and its clearing, each read back from the relationship the writes answer.
async fn endorsed_then_noted(world: &World, alice: &Account, carol: &Account) {
    let endorsed = write(world, alice, carol, "pin").await;
    common::assert_answer(&endorsed, StatusCode::OK, "the endorsement");
    assert_eq!(
        endorsed.json_value()["endorsed"],
        json!(true),
        "the relationship answers pinned"
    );
    let pinned = common::api_get(&world.app, Some(&alice.token), "/api/v1/endorsements").await;
    common::assert_answer(&pinned, StatusCode::OK, "the endorsement listing");
    assert_eq!(
        entity_ids(&pinned),
        vec![carol.id.clone()],
        "the profile's own list names the pinned account"
    );

    let noted = common::api_post(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/note", carol.id),
        &json!({"comment": NOTE}),
    )
    .await;
    common::assert_answer(&noted, StatusCode::OK, "the note write");
    assert_eq!(
        noted.json_value()["note"],
        NOTE,
        "the relationship carries it"
    );
    let read = relationship(world, &alice.token, &carol.id).await;
    assert_eq!(read["note"], NOTE, "and so does the batch read");

    let withdrawn = write(world, alice, carol, "unpin").await;
    common::assert_answer(&withdrawn, StatusCode::OK, "the endorsement withdrawal");
    assert_eq!(
        withdrawn.json_value()["endorsed"],
        json!(false),
        "unpinning takes the endorsement back"
    );
    let cleared = common::api_post(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/note", carol.id),
        &json!({"comment": ""}),
    )
    .await;
    common::assert_answer(&cleared, StatusCode::OK, "the note clearing");
    assert_eq!(
        cleared.json_value()["note"],
        "",
        "and a blank comment clears the note"
    );
}

/// A list with a member: the create, the add, the member page, the
/// account's own membership read, and the removal. A member that names no
/// account is the shared 404 the whole `/lists` family answers with.
async fn list_with_members(world: &World, alice: &Account, carol: &Account) {
    let created = common::api_post(
        &world.app,
        Some(&alice.token),
        "/api/v1/lists",
        &json!({"title": LIST}),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "the list create");
    assert_eq!(created.json_value()["title"], LIST, "the entity carries it");
    let list_id = common::entity_id(&created);
    let members = format!("/api/v1/lists/{list_id}/accounts");

    let added = common::api_post(
        &world.app,
        Some(&alice.token),
        &members,
        &json!({"account_ids": [carol.id]}),
    )
    .await;
    common::assert_answer(&added, StatusCode::OK, "the member add");
    let listed = common::api_get(&world.app, Some(&alice.token), &members).await;
    common::assert_answer(&listed, StatusCode::OK, "the member page");
    assert_eq!(
        entity_ids(&listed),
        vec![carol.id.clone()],
        "the member page names the added account"
    );
    let belongs = common::api_get(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/lists", carol.id),
    )
    .await;
    common::assert_answer(&belongs, StatusCode::OK, "the membership read");
    assert_eq!(
        entity_ids(&belongs),
        vec![list_id.clone()],
        "and the account's own membership read names the list"
    );

    let stranger = common::api_post(
        &world.app,
        Some(&alice.token),
        &members,
        &json!({"account_ids": [UNKNOWN_ID]}),
    )
    .await;
    assert_refused(
        &stranger,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a member that names no account",
    );
    let removed = common::api_delete(
        &world.app,
        Some(&alice.token),
        &members,
        &json!({"account_ids": [carol.id]}),
    )
    .await;
    common::assert_answer(&removed, StatusCode::OK, "the member removal");
    let empty = common::api_get(&world.app, Some(&alice.token), &members).await;
    common::assert_answer(&empty, StatusCode::OK, "the emptied member page");
    assert_eq!(empty.json_value(), json!([]), "the page is empty again");
}

/// The domain block over the whole API: the remote actor resolved from a
/// peer on loopback and followed, the block that flags it and only it, the
/// caller's own list of blocked domains, and the unblock that clears both.
async fn domain_block(world: &World, alice: &Account, bob: &Account) {
    let peer = MockPeer::start().await;
    let followed = common::api_post(
        &world.app,
        Some(&alice.token),
        "/api/v1/follows",
        &json!({"uri": peer.actor_url()}),
    )
    .await;
    common::assert_answer(&followed, StatusCode::OK, "the follow of a remote actor");
    let mirror_id = string_field(&followed.json_value(), "id");
    let before = relationship(world, &alice.token, &mirror_id).await;
    assert_eq!(
        before["domain_blocking"],
        json!(false),
        "the mirror is not domain-blocked yet: {before}"
    );

    let blocked = common::api_post(
        &world.app,
        Some(&alice.token),
        "/api/v1/domain_blocks",
        &json!({"domain": peer.authority()}),
    )
    .await;
    common::assert_answer(&blocked, StatusCode::OK, "the domain block");
    assert_eq!(blocked.json_value(), json!({}), "Pleroma's empty object");
    let listing = common::api_get(&world.app, Some(&alice.token), "/api/v1/domain_blocks").await;
    common::assert_answer(&listing, StatusCode::OK, "the blocked domains listing");
    assert_eq!(
        listing.json_value(),
        json!([peer.authority()]),
        "the blocked domains are the caller's own list"
    );
    let remote = relationship(world, &alice.token, &mirror_id).await;
    assert_eq!(
        remote["domain_blocking"],
        json!(true),
        "the remote mirror of that domain is now blocked: {remote}"
    );
    let local = relationship(world, &alice.token, &bob.id).await;
    assert_eq!(
        local["domain_blocking"],
        json!(false),
        "while a local account never is: {local}"
    );

    let lifted = common::api_delete(
        &world.app,
        Some(&alice.token),
        &format!("/api/v1/domain_blocks?domain={}", peer.authority()),
        &Value::Null,
    )
    .await;
    common::assert_answer(&lifted, StatusCode::OK, "the domain unblock");
    let after = common::api_get(&world.app, Some(&alice.token), "/api/v1/domain_blocks").await;
    common::assert_answer(&after, StatusCode::OK, "the emptied domain list");
    assert_eq!(after.json_value(), json!([]), "the list is empty again");
    let clear = relationship(world, &alice.token, &mirror_id).await;
    assert_eq!(
        clear["domain_blocking"],
        json!(false),
        "and the flag went with the block: {clear}"
    );
}

/// The following page walked with `limit=1` and the `Link` header's own
/// cursors, the follower page of the account that was followed, and the
/// empty page that ends the walk without a header at all.
async fn follower_pages(
    world: &World,
    alice: &Account,
    bob: &Account,
    carol: &Account,
    dave: &Account,
) {
    let following = format!("{ACCOUNTS}/{}/following", alice.id);
    let first = common::api_get(
        &world.app,
        Some(&alice.token),
        &format!("{following}?limit=1"),
    )
    .await;
    common::assert_answer(&first, StatusCode::OK, "the first following page");
    let newest = entity_ids(&first);
    assert_eq!(newest.len(), 1, "one row per page");
    let next = link_cursor(&first, "next");
    assert!(
        next.contains("max_id="),
        "the next cursor pages older: {next}"
    );
    assert!(
        link_cursor(&first, "prev").contains("min_id="),
        "and the prev cursor pages newer"
    );
    let second = common::api_get(&world.app, Some(&alice.token), &next).await;
    common::assert_answer(&second, StatusCode::OK, "the second following page");
    let older = entity_ids(&second);
    assert_eq!(older.len(), 1, "the walk reaches the second row");
    assert_ne!(newest, older, "and never repeats a row");
    let mut walked = newest;
    walked.extend(older);
    assert_eq!(
        sorted(&walked),
        sorted(&[carol.id.clone(), dave.id.clone()]),
        "the two pages together are the two follows — the severed one of \
         bob's is not among them"
    );

    let followers = common::api_get(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/followers", carol.id),
    )
    .await;
    common::assert_answer(&followers, StatusCode::OK, "the follower listing");
    assert_eq!(
        entity_ids(&followers),
        vec![alice.id.clone()],
        "the locked account was followed once, and answers that follower"
    );
    let empty = common::api_get(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/followers", bob.id),
    )
    .await;
    common::assert_answer(&empty, StatusCode::OK, "the empty follower listing");
    assert_eq!(
        empty.json_value(),
        json!([]),
        "the blocked-and-unblocked pair is over"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "and an empty page sends no Link header at all"
    );
}

// ───────────────────  Scenario 5: the registration modes  ───────────────────

/// The registration modes end to end, one instance per mode over the same
/// database: approval required until staff approve through the admin API,
/// an invite token opening a closed instance, the confirmation letter the
/// drained mailer leaves in the memory mailbox, and a password reset
/// through its emailed token — each waited on by the `password` grant and
/// each opened by the answer that ends the wait.
#[tokio::test]
async fn registration_modes_hold_the_door_until_their_wait_is_over() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let root = common::create_admin_with_password(&pool, "root", PASSWORD).await;
    assert!(root.is_admin.get(), "the one fixture is an admin");

    approval_mode_releases_on_the_admin_write(&pool).await;
    invite_token_mode_opens_a_closed_instance(&pool).await;
    confirmation_letter_opens_a_mail_gated_account(&pool).await;
    emailed_reset_token_sets_a_new_password(&pool).await;
}

/// The config of an instance in the two registration modes `approval` and
/// `activation` names, with a relay configured so a letter really queues
/// and the drained mailbox really holds it.
fn mode_config(approval: Toggle, activation: Toggle) -> Config {
    Config {
        registrations_approval_required: approval,
        account_activation_required: activation,
        smtp_host: Some("smtp.example.org".to_owned()),
        ..common::test_config()
    }
}

/// `POST /api/v1/accounts` over `bearer`'s app-only credential, with
/// `extra` merged over the four documented fields.
async fn register_with(world: &World, bearer: &str, username: &str, extra: &Value) -> TestResponse {
    let mut body = json!({
        "username": username,
        "email": address_of(username),
        "password": PASSWORD,
        "agreement": true,
    });
    let map = body.as_object_mut().expect("`body` is an object");
    for (key, value) in extra
        .as_object()
        .expect("the extra registration fields are an object")
    {
        let _replaced = map.insert(key.clone(), value.clone());
    }
    common::api_post(&world.app, Some(bearer), ACCOUNTS, &body).await
}

/// The 200 `message`/`identifier` answer of a registration that has to
/// wait, and the absence of any credential beside it.
fn assert_waiting(answer: &TestResponse, identifier: &str, who: &str) {
    common::assert_answer(answer, StatusCode::OK, who);
    let body = answer.json_value();
    assert_eq!(
        body["identifier"], identifier,
        "the machine-readable reason"
    );
    assert!(
        body.get("access_token").is_none(),
        "and no credential while the account waits: {body}"
    );
}

/// Approval required: the registration answers the wait, the login is
/// refused with the same identifier, an admin's approval through the Pleroma
/// admin API ends the wait, and the login then works.
async fn approval_mode_releases_on_the_admin_write(pool: &PgPool) {
    let world = World::start(
        pool.clone(),
        mode_config(Toggle::new(true), Toggle::new(false)),
    )
    .await;
    let bearer = world.app_token().await;

    // Step 1: the applicant is registered, and told to wait.
    let answer = register_with(&world, &bearer, "wanting", &json!({"reason": "let me in"})).await;
    assert_waiting(
        &answer,
        "awaiting_approval",
        "a registration in approval mode",
    );

    // Step 2: every login door is shut on the same reason.
    let refused = common::login_password(&world.app, &world.client, "wanting", PASSWORD).await;
    assert_refused(
        &refused,
        StatusCode::FORBIDDEN,
        "Your account is awaiting approval.",
        "the login of an unapproved account",
    );
    assert_eq!(
        refused.json_value()["identifier"],
        "awaiting_approval",
        "with the machine-readable reason beside it"
    );

    // Step 3: staff sign in over HTTP and approve the account.
    let admin = world.login("root").await;
    let approved = common::api_patch(
        &world.app,
        Some(&admin),
        &format!("{ADMIN_USERS}/approve"),
        &json!({"nicknames": ["wanting"]}),
    )
    .await;
    common::assert_answer(&approved, StatusCode::OK, "the admin approval");
    assert_eq!(
        approved.json_value()["users"][0]["is_approved"],
        json!(true),
        "the answered entity is approved"
    );

    // Step 4: the wait is over, and the password grant opens.
    let token = world.login("wanting").await;
    let read = common::api_get(&world.app, Some(&token), VERIFY).await;
    common::assert_answer(&read, StatusCode::OK, "the approved account reads itself");
    assert_eq!(
        string_field(&read.json_value(), "username"),
        "wanting",
        "the account staff let in"
    );
}

/// Invite tokens: the instance is closed, a registration without a token
/// is refused, an invite an admin minted over the admin API opens it, and
/// the one-time token is spent by that registration.
async fn invite_token_mode_opens_a_closed_instance(pool: &PgPool) {
    let mut config = common::test_config();
    config.registrations_open = Toggle::new(false);
    config.invites_enabled = Toggle::new(true);
    let world = World::start(pool.clone(), config).await;
    let bearer = world.app_token().await;

    // Step 1: closed means closed: no token, no account.
    let refused = register_with(&world, &bearer, "shutout", &json!({})).await;
    assert_refused(
        &refused,
        StatusCode::BAD_REQUEST,
        "Invalid token",
        "an uninvited registration on an invite-only instance",
    );

    // Step 2: an admin mints a one-time invite through the admin API.
    let admin = world.login("root").await;
    let minted = common::api_post(
        &world.app,
        Some(&admin),
        &format!("{ADMIN_USERS}/invite_token"),
        &json!({}),
    )
    .await;
    common::assert_answer(&minted, StatusCode::OK, "the invite mint");
    let invite = string_field(&minted.json_value(), "token");

    // Step 3: the invited registration lands and answers a credential.
    let answer = register_with(&world, &bearer, "invited", &json!({"token": invite})).await;
    common::assert_answer(&answer, StatusCode::OK, "the invited registration");
    let token = common::access_token_of(&answer);
    assert_eq!(
        string_field(
            &common::api_get(&world.app, Some(&token), VERIFY)
                .await
                .json_value(),
            "username"
        ),
        "invited",
        "the credential belongs to the invited account"
    );

    // Step 4: a one-time invite is spent, and refuses the next applicant.
    let spent = register_with(&world, &bearer, "second", &json!({"token": invite})).await;
    assert_refused(
        &spent,
        StatusCode::BAD_REQUEST,
        "Invalid token",
        "the same invite a second time",
    );
}

/// Email confirmation: the registration answers the wait, the drained
/// queue leaves the letter in the memory mailbox, the link it carries
/// confirms the address, and the login opens.
async fn confirmation_letter_opens_a_mail_gated_account(pool: &PgPool) {
    let world = World::start(
        pool.clone(),
        mode_config(Toggle::new(false), Toggle::new(true)),
    )
    .await;
    let bearer = world.app_token().await;

    // Step 1: the account is registered and parked on its address.
    let answer = register_with(&world, &bearer, "mailed", &json!({})).await;
    assert_waiting(
        &answer,
        "missing_confirmed_email",
        "a registration in activation mode",
    );
    let refused = common::login_password(&world.app, &world.client, "mailed", PASSWORD).await;
    assert_refused(
        &refused,
        StatusCode::FORBIDDEN,
        "Your login is missing a confirmed e-mail address",
        "the login of an unconfirmed account",
    );

    // Step 2: the drained mailer delivers the confirmation letter.
    let ran = world.drain().await;
    assert!(
        ran >= 1,
        "the confirmation job was queued and ran, got {ran}"
    );
    let addressed = letters_to(&world.mailbox(), &address_of("mailed"));
    assert_eq!(addressed.len(), 1, "one letter, to the new account");
    let path = letter_path(&addressed, "api/account/confirm_email");

    // Step 3: the link confirms the address and redirects home.
    let visit = common::api_get(&world.app, None, &path).await;
    common::assert_answer(&visit, StatusCode::FOUND, "the confirmation link");
    assert_eq!(
        visit.header("location"),
        Some(format!("{BASE}/").as_str()),
        "Pleroma's redirect to the instance root"
    );

    // Step 4: the confirmed account signs in.
    let token = world.login("mailed").await;
    let read = common::api_get(&world.app, Some(&token), VERIFY).await;
    assert_eq!(
        string_field(&read.json_value(), "username"),
        "mailed",
        "the letter's recipient, now live"
    );
}

/// Password reset: the request answers 204 whatever it names, the drained
/// queue leaves the token's link in the mailbox, the page opens on it, the
/// completion form changes the password, revokes the account's credentials
/// and reopens the login — with the new password only.
async fn emailed_reset_token_sets_a_new_password(pool: &PgPool) {
    let world = World::start(pool.clone(), mail_config()).await;
    let forgotten = world.register("forgetful").await;

    // Step 1: the request cannot tell a live account from a dead one.
    let address = address_of("forgetful");
    let asked = common::TestRequest::post("/auth/password")
        .form(&[("email", address.as_str())])
        .send(&world.app)
        .await;
    common::assert_answer(&asked, StatusCode::NO_CONTENT, "the reset request");
    let quiet = common::TestRequest::post("/auth/password")
        .form(&[("email", "nobody@example.org")])
        .send(&world.app)
        .await;
    common::assert_answer(&quiet, StatusCode::NO_CONTENT, "the request for a miss");

    // Step 2: the only letter for this account is the token's.
    let ran = world.drain().await;
    assert!(ran >= 1, "the reset letter was queued and ran, got {ran}");
    let addressed = letters_to(&world.mailbox(), &address);
    assert_eq!(addressed.len(), 1, "one letter, to the account that asked");
    let path = letter_path(&addressed, "password_reset");

    // Step 3: the page the link names renders the form.
    let page = common::api_get(&world.app, None, &path).await;
    common::assert_answer(&page, StatusCode::OK, "the reset page");
    assert!(
        page.text().contains("<form"),
        "the live token renders its form: {}",
        page.text()
    );

    // Step 4: the completion form sets the new password.
    let token = path.rsplit('/').next().unwrap_or_default().to_owned();
    let done = common::TestRequest::post("/api/v1/pleroma/password_reset")
        .form(&[
            ("data[token]", token.as_str()),
            ("data[password]", NEW_PASSWORD),
            ("data[password_confirmation]", NEW_PASSWORD),
        ])
        .send(&world.app)
        .await;
    common::assert_answer(&done, StatusCode::OK, "the reset completion");
    assert!(
        done.text().contains("Password changed!"),
        "Pleroma's success page: {}",
        done.text()
    );

    // Step 5: every credential the old password opened is gone, the old
    // password opens nothing, and the new one logs in.
    let stale = common::api_get(&world.app, Some(&forgotten.token), VERIFY).await;
    assert_refused(
        &stale,
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "the session the reset revoked",
    );
    let old = common::login_password(&world.app, &world.client, "forgetful", PASSWORD).await;
    assert_refused(
        &old,
        StatusCode::BAD_REQUEST,
        INVALID_CREDENTIALS,
        "the password the reset replaced",
    );
    let fresh = world.login_with("forgetful", NEW_PASSWORD).await;
    let read = common::api_get(&world.app, Some(&fresh), VERIFY).await;
    common::assert_answer(&read, StatusCode::OK, "the login with the reset password");
    assert_eq!(
        string_field(&read.json_value(), "username"),
        "forgetful",
        "the same account, with a new password"
    );
}

/// The scenario config with a relay configured: the switch that makes
/// [`deliver_later`](wired_be::mailer::deliver_later) queue a letter
/// instead of dropping it, which the mail-driven steps then drain.
fn mail_config() -> Config {
    mode_config(Toggle::new(false), Toggle::new(false))
}

// ────────────────────  Scenario 6: admin user management  ────────────────────

/// Admin user management end to end: an admin created as the fixture and
/// signed in over HTTP creates an account through the admin API, tags it,
/// promotes it, rewrites its credentials, deactivates and re-activates it,
/// and finally deletes it — every effect read back through the
/// user-facing API the account itself uses.
#[tokio::test]
async fn an_admin_walks_a_user_from_create_to_delete() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _root = common::create_admin_with_password(&pool, "root", PASSWORD).await;
    let world = World::open(pool).await;
    let admin = world.login("root").await;

    // Step 1: the admin creates a user, which then signs in over HTTP, and
    // a tag the public profile carries.
    let (id, token) = created_then_tagged(&world, &admin).await;

    // Step 2: the role grant the account itself can read, and the
    // credentials rewrite the profile shows.
    promoted_and_rewritten(&world, &admin, &id, &token).await;

    // Step 3: the deactivation closes the account, the activation opens it.
    deactivated_then_activated(&world, &admin, &token).await;

    // Step 4: the delete, swept by the queue, takes the profile with it.
    let deleted = common::api_delete(
        &world.app,
        Some(&admin),
        ADMIN_USERS,
        &json!({"nicknames": ["madeup"]}),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the admin delete");
    assert_eq!(
        deleted.json_value(),
        json!(["madeup"]),
        "the started nickname"
    );
    let ran = world.drain().await;
    assert!(ran >= 1, "the delete sweep ran, got {ran}");
    let gone = common::api_get(&world.app, None, &format!("{ACCOUNTS}/{id}")).await;
    assert_refused(
        &gone,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the profile of a deleted account",
    );
    let refusal = common::login_password(&world.app, &world.client, "madeup", PASSWORD).await;
    assert_eq!(
        refusal.status,
        StatusCode::BAD_REQUEST,
        "and its password no longer resolves: {}",
        refusal.text()
    );
}

/// The admin creation, answered per item, and the sign-in it makes
/// possible: the new account's id and bearer token, beside the tag the
/// public profile then carries.
///
/// # Panics
///
/// If the create or the sign-in is refused, or an answer carries no id.
async fn created_then_tagged(world: &World, admin: &str) -> (String, String) {
    let created = common::api_post(
        &world.app,
        Some(admin),
        ADMIN_USERS,
        &json!({"users": [{
            "nickname": "madeup",
            "email": address_of("madeup"),
            "password": PASSWORD,
        }]}),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "the admin create");
    assert_eq!(
        created.json_value()[0]["code"],
        200,
        "the per-item success entry: {}",
        created.json_value()
    );
    let token = world.login("madeup").await;
    let signed_in = common::api_get(&world.app, Some(&token), VERIFY).await;
    common::assert_answer(
        &signed_in,
        StatusCode::OK,
        "the created account signs straight in",
    );
    assert_eq!(
        string_field(&signed_in.json_value(), "username"),
        "madeup",
        "as itself"
    );
    let id = common::entity_id(&signed_in);

    let tagged = common::api_put(
        &world.app,
        Some(admin),
        &format!("{ADMIN_USERS}/tag"),
        &json!({"nicknames": ["madeup"], "tags": ["Beta testers"]}),
    )
    .await;
    common::assert_answer(&tagged, StatusCode::NO_CONTENT, "the tag write");
    let shown = common::api_get(&world.app, None, &format!("{ACCOUNTS}/{id}")).await;
    common::assert_answer(&shown, StatusCode::OK, "the tagged profile read");
    assert_eq!(
        shown.json_value()["pleroma"]["tags"],
        json!(["beta testers"]),
        "the public profile carries the stored, lowercased tag"
    );
    (id, token)
}

/// The promotion an account reads on itself, and the credentials write the
/// public profile reads back: staff own both ends of the row.
///
/// # Panics
///
/// If either write or either read is refused.
async fn promoted_and_rewritten(world: &World, admin: &str, id: &str, token: &str) {
    let granted = common::api_post(
        &world.app,
        Some(admin),
        &format!("{ADMIN_USERS}/madeup/permission_group/admin"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&granted, StatusCode::OK, "the admin role grant");
    assert_eq!(granted.json_value(), json!({"is_admin": true}), "one key");
    let promoted = common::api_get(&world.app, Some(token), VERIFY).await;
    common::assert_answer(&promoted, StatusCode::OK, "the promoted account's read");
    assert_eq!(
        promoted.json_value()["pleroma"]["is_admin"],
        json!(true),
        "and the account's own read says so"
    );

    let rewritten = common::api_patch(
        &world.app,
        Some(admin),
        &format!("{ADMIN_USERS}/madeup/credentials"),
        &json!({"name": "Made Up", "bio": "created by staff"}),
    )
    .await;
    common::assert_answer(&rewritten, StatusCode::OK, "the credentials write");
    assert_eq!(
        rewritten.json_value(),
        json!({"status": "success"}),
        "Pleroma's success map"
    );
    let profile = common::api_get(&world.app, None, &format!("{ACCOUNTS}/{id}")).await;
    common::assert_answer(&profile, StatusCode::OK, "the rewritten profile read");
    assert_eq!(
        profile.json_value()["display_name"],
        "Made Up",
        "the display name the admin set"
    );
    assert_eq!(
        profile.json_value()["note"],
        "<p>created by staff</p>",
        "and the bio it wrote"
    );
}

/// The deactivation and the activation, each proved from the account's own
/// side: its bearer is refused with the gate's words, its login carries the
/// `account_is_disabled` identifier, and both come back with the flag.
async fn deactivated_then_activated(world: &World, admin: &str, token: &str) {
    let off = common::api_patch(
        &world.app,
        Some(admin),
        &format!("{ADMIN_USERS}/deactivate"),
        &json!({"nicknames": ["madeup"]}),
    )
    .await;
    common::assert_answer(&off, StatusCode::OK, "the deactivation");
    assert_eq!(
        off.json_value()["users"][0]["is_active"],
        json!(false),
        "answered as deactivated"
    );
    let denied = common::api_get(&world.app, Some(token), VERIFY).await;
    assert_refused(
        &denied,
        StatusCode::FORBIDDEN,
        DISABLED,
        "the bearer gate's refusal of a disabled account",
    );
    let refused = common::login_password(&world.app, &world.client, "madeup", PASSWORD).await;
    assert_refused(
        &refused,
        StatusCode::FORBIDDEN,
        DISABLED,
        "the login of a disabled account",
    );
    assert_eq!(
        refused.json_value()["identifier"],
        "account_is_disabled",
        "with the grant's own reason code"
    );

    let on = common::api_patch(
        &world.app,
        Some(admin),
        &format!("{ADMIN_USERS}/activate"),
        &json!({"nicknames": ["madeup"]}),
    )
    .await;
    common::assert_answer(&on, StatusCode::OK, "the activation");
    assert_eq!(
        on.json_value()["users"][0]["is_active"],
        json!(true),
        "answered as active again"
    );
    let read = common::api_get(&world.app, Some(token), VERIFY).await;
    common::assert_answer(&read, StatusCode::OK, "the released account reads itself");
}

// ───────────────────────  Scenario 7: the CSV imports  ───────────────────────

/// The import scenario end to end: three Mastodon-style CSV lists posted
/// to the import endpoints, the jobs they queue drained through
/// [`common::run_all_jobs`], and the relationships they promised read back
/// through `GET /api/v1/accounts/relationships` — including the `Show
/// boosts` column, which lands as the follow's own toggle.
#[tokio::test]
async fn csv_imports_land_as_relationships_when_the_queue_drains() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let alice = world.register("alice").await;
    let bob = world.register("bob").await;
    let carol = world.register("carol").await;
    let dave = world.register("dave").await;

    // Step 1: the follow list, posted as an inline `list` parameter: the
    // header line dropped, a leading `@` stripped, one row per account.
    let follows = import(
        &world,
        &alice,
        "/api/pleroma/follow_import",
        "Account address,Show boosts,Notify on new posts,Languages\n\
         @bob,,,\n\
         carol,False,,\n",
    )
    .await;
    common::assert_answer(&follows, StatusCode::OK, "the follow import");
    assert_eq!(
        follows.json_value(),
        "jobs started",
        "Pleroma's JSON-string answer"
    );

    // Step 2: the block and mute lists, and one drain for the whole queue.
    let blocks = import(
        &world,
        &alice,
        "/api/pleroma/blocks_import",
        "Account address\ndave\n",
    )
    .await;
    let mutes = import(
        &world,
        &alice,
        "/api/pleroma/mutes_import",
        "Account address\n@bob\n",
    )
    .await;
    common::assert_answer(&blocks, StatusCode::OK, "the block import");
    common::assert_answer(&mutes, StatusCode::OK, "the mute import");
    let ran = world.drain().await;
    assert!(ran >= 3, "one job per import ran, got {ran}");

    // Step 3: the graph the imports built, read back through the API.
    let followed = relationship(&world, &alice.token, &bob.id).await;
    assert_flags(
        &followed,
        &json!({"following": true, "showing_reblogs": true}),
        "the follow whose boosts column was left blank",
    );
    let hidden = relationship(&world, &alice.token, &carol.id).await;
    assert_flags(
        &hidden,
        &json!({"following": true, "showing_reblogs": false}),
        "the follow whose `Show boosts` said False",
    );
    let blocked = relationship(&world, &alice.token, &dave.id).await;
    assert_flags(&blocked, &json!({"blocking": true}), "the imported block");
    let quiet = relationship(&world, &alice.token, &bob.id).await;
    assert_flags(
        &quiet,
        &json!({"muting": true}),
        "the imported mute, beside the follow it left standing",
    );

    // Step 4: an empty list queues nothing and is still answered.
    let nothing = import(&world, &alice, "/api/pleroma/follow_import", "").await;
    common::assert_answer(&nothing, StatusCode::OK, "the empty import");
}

/// One CSV list posted to `path` as `account`, in the inline JSON spelling
/// of the `list` field.
async fn import(world: &World, account: &Account, path: &str, csv: &str) -> TestResponse {
    common::api_post(
        &world.app,
        Some(&account.token),
        path,
        &json!({"list": csv}),
    )
    .await
}

// ──────────────────────────  The loopback mock peer  ──────────────────────────

/// A mock `ActivityPub` peer on loopback serving one actor document: the
/// actor the domain-block step resolves and mirrors through the public API,
/// on a domain the scenario can then name.
struct MockPeer {
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
    async fn start() -> Self {
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
    fn actor_url(&self) -> String {
        format!("{}/users/zoe", self.base)
    }

    /// The `host:port` authority of [`Self::actor_url`]: the domain the
    /// mirror of this peer's actor is stored under, port included.
    fn authority(&self) -> String {
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
