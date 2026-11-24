//! Scenario 5: the registration modes, one instance per mode over the same
//! database — approval required until staff approve through the admin API,
//! an invite token opening a closed instance, the confirmation letter the
//! drained mailer leaves in the memory mailbox, and a password reset
//! through its emailed token. The mode legs and their shared registration
//! helpers live here; the configs they start on are in
//! [`support`](super::support).

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::config::Toggle;

use super::support::{
    ACCOUNTS, ADMIN_USERS, BASE, INVALID_CREDENTIALS, INVALID_TOKEN, NEW_PASSWORD, PASSWORD,
    VERIFY, World, address_of, assert_refused, letter_path, letters_to, mail_config, mode_config,
    string_field,
};
use crate::common::{self, TestResponse};

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

    // Step 3: the link names the frontend's reset screen, which checks the
    // token with the JSON endpoint.
    assert!(
        path.starts_with("/password_reset/"),
        "the link is the frontend's reset screen: {path}"
    );
    let token = path.rsplit('/').next().unwrap_or_default().to_owned();
    let page = common::api_get(
        &world.app,
        None,
        &format!("/api/v1/pleroma/password_reset/{token}"),
    )
    .await;
    common::assert_answer(&page, StatusCode::OK, "the token check");
    assert_eq!(
        page.json_value(),
        json!({"nickname": "forgetful"}),
        "the live token names its account: {}",
        page.text()
    );

    // Step 4: the completion call sets the new password.
    let done = common::TestRequest::post("/api/v1/pleroma/password_reset")
        .form(&[
            ("data[token]", token.as_str()),
            ("data[password]", NEW_PASSWORD),
            ("data[password_confirmation]", NEW_PASSWORD),
        ])
        .send(&world.app)
        .await;
    common::assert_answer(&done, StatusCode::NO_CONTENT, "the reset completion");

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
