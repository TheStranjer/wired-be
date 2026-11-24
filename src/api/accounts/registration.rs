//! The shapes of `POST /api/v1/accounts`: the request parameters, the
//! token JSON of a ready registration and the `message`/`identifier`
//! pair of a pending one. [`register`] composes them into the answer.
//!
//! [`register`]: super::register

use serde::{Deserialize, Serialize};

use crate::{
    api::oauth::issue::TokenResponse,
    auth::{AWAITING_APPROVAL_ID, EMAIL_UNCONFIRMED_ID},
    extract::flex,
    models::user::{User, register::RegisterParams},
};

use super::{REGISTERED_AWAITING_APPROVAL, REGISTERED_AWAITING_CONFIRMATION};

/// The request parameters of `POST /api/v1/accounts`, Pleroma's
/// `AccountCreateRequest` (`lib/pleroma/web/api_spec/operations/
/// account_operation.ex`) minus the captcha fields: `username`, `email`,
/// `password` and `agreement` are read, the first three required and
/// `agreement` required too (an absent one is the extractor's 422).
/// `agreement` deserializes through [`flex::bool`] so the form spelling
/// `true` arrives. The invite `token` is read as well — required and
/// checked while registrations are closed and invites are enabled,
/// optional and ignored otherwise (see [`load_invite`]). Every other key
/// the body carries — `locale`, `confirm`, the captcha pair, Pleroma's
/// own `birthday` — is accepted and ignored, as
/// [`Registration.register_user`] reads none of them on this path;
/// `reason`, `fullname` and `bio` are the optional registration facts
/// that do reach the row.
///
/// [`Registration.register_user`]: https://hexdocs.pm/pleroma/Pleroma.Web.Registration.html
#[derive(Debug, Deserialize)]
pub(super) struct CreateAccountParams {
    /// The desired handle, validated by [`create_local_user`] as
    /// `RegisterParams::nickname`.
    username: String,
    /// Contact email; required unless
    /// [`accounts_email_optional`](crate::config::Config::accounts_email_optional).
    #[serde(default)]
    email: Option<String>,
    /// Plaintext password, stored only as a hash.
    password: String,
    /// Whether the applicant accepts the instance's terms; must be true.
    #[serde(deserialize_with = "flex::bool")]
    pub(super) agreement: bool,
    /// The registration invite Pleroma's `token` parameter carries: read
    /// only through [`load_invite`]'s gate, and consumed by
    /// [`create_local_user`] in the user's own insert transaction.
    #[serde(default)]
    pub(super) token: Option<String>,
    /// Free-text reason for the later approval flow, stored in
    /// `users.registration_reason`.
    #[serde(default)]
    reason: Option<String>,
    /// Display name (Pleroma's `fullname`); the nickname stands in when
    /// absent or blank.
    #[serde(default)]
    fullname: Option<String>,
    /// Profile note as source text, stored as `raw_bio` and rendered for
    /// `bio`.
    #[serde(default)]
    bio: Option<String>,
}

impl CreateAccountParams {
    /// The registration [`RegisterParams`] these body parameters claim:
    /// `fullname` is Pleroma's display `name` fallback to the username,
    /// and `reason` rides along for storage.
    pub(super) fn into_register_params(self) -> RegisterParams {
        RegisterParams {
            nickname: self.username,
            email: self.email,
            password: self.password,
            name: self.fullname,
            bio: self.bio,
            reason: self.reason,
            // This endpoint carries no provider: the email is only ever
            // confirmed by the link the confirmation letter sends.
            provider_email_verified: false,
        }
    }
}

/// The 200 body of a registration whose account may not log in yet: Pleroma's
/// `create` answer for `{:account_status, :approval_pending}` and
/// `{:account_status, :confirmation_pending}` — a human-facing `message`
/// beside the machine-readable `identifier`, and no token at all.
#[derive(Debug, Serialize)]
pub(super) struct RegistrationMessage {
    /// What the applicant is told: see
    /// [`REGISTERED_AWAITING_APPROVAL`] and
    /// [`REGISTERED_AWAITING_CONFIRMATION`].
    message: &'static str,
    /// Why the registration has no token: [`AWAITING_APPROVAL_ID`] or
    /// [`EMAIL_UNCONFIRMED_ID`].
    identifier: &'static str,
}

/// What a finished registration answers: the bearer credentials of an
/// account that may log in now, or the message of one that may not.
///
/// `untagged` is the whole point: the two shapes are the two bodies, with
/// no wrapper key around either, exactly as Pleroma renders them.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(super) enum RegistrationAnswer {
    /// The token JSON of a ready account: the `/oauth/token` answer
    /// without its `me` key ([`TokenResponse::without_me`]).
    Ready(TokenResponse),
    /// The `message`/`identifier` pair of an account that first has to be
    /// approved, confirmed, or both.
    Pending(RegistrationMessage),
}

impl RegistrationMessage {
    /// The message an unfinished registration owes `user`, `None` when the
    /// account may log in now and its token is what answers. Approval is
    /// reported before confirmation — an account that waits on staff is
    /// told that first, whatever its mailbox still owes (Pleroma's `create`
    /// reads the two statuses off `User.account_status`, which likewise
    /// weighs approval before confirmation).
    pub(super) const fn for_user(user: &User) -> Option<Self> {
        if user.approval_pending.get() {
            return Some(Self {
                message: REGISTERED_AWAITING_APPROVAL,
                identifier: AWAITING_APPROVAL_ID,
            });
        }
        if !user.email_confirmed.get() {
            return Some(Self {
                message: REGISTERED_AWAITING_CONFIRMATION,
                identifier: EMAIL_UNCONFIRMED_ID,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::{
        api::accounts::test_support::stored_user,
        error::{AppError, AppResult},
        models::{oauth_token::OauthToken, user::Flag},
    };

    use super::*;

    /// Parses `value` as the registration endpoint's parameters, as
    /// `ApiBody` would.
    fn registration_params(value: Value) -> AppResult<CreateAccountParams> {
        crate::extract::into_params(value)
    }

    /// A stored-looking token row minted right now, for the
    /// registration answer's render.
    fn minted_token() -> OauthToken {
        let now = Utc::now();
        OauthToken {
            id: Uuid::now_v7(),
            token: "access-secret".to_owned(),
            refresh_token: "refresh-secret".to_owned(),
            user_id: Some(Uuid::now_v7()),
            app_id: Uuid::now_v7(),
            scopes: "read write follow push".to_owned(),
            valid_until: now + chrono::Duration::days(30),
            inserted_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn registration_params_take_their_required_fields_and_default_the_rest() {
        let params = registration_params(json!({
            "username": "lain",
            "email": "lain@example.org",
            "password": "PlzDontHackLain",
            "agreement": true,
        }))
        .expect("the required four are there");
        assert_eq!(params.username, "lain", "username");
        assert!(params.agreement, "a real json boolean");
        assert!(
            params.reason.is_none() && params.fullname.is_none() && params.bio.is_none(),
            "every optional field defaults away when absent"
        );
        assert!(params.token.is_none(), "an absent invite token is None");
    }

    #[test]
    fn registration_agreement_accepts_the_form_spellings() {
        // The extractor turns `agreement=true` into this shape
        // (`src/extract.rs` covers the parsing itself).
        let params = registration_params(json!({
            "username": "lain",
            "password": "PlzDontHackLain",
            "agreement": "true",
            "reason": "I'm a cool dude, bro",
            "fullname": "Lain Iwakura",
            "bio": "☕️",
        }))
        .expect("a form-style body");
        assert!(params.agreement, "`\"true\"` is the form spelling");
        assert_eq!(params.reason.as_deref(), Some("I'm a cool dude, bro"));
    }

    #[test]
    fn ignored_fields_are_accepted_and_untyped() {
        // `locale` and `confirm` are on the wire but never read: the
        // endpoint takes them as sent, in any shape, and ignores them.
        let params = registration_params(json!({
            "username": "lain",
            "password": "PlzDontHackLain",
            "agreement": "on",
            "locale": "en",
            "confirm": "PlzDontHackLain",
            "captcha_response": 9001,
        }))
        .expect("unknown keys do not bind and do not reject");
        assert!(params.agreement, "`\"on\"` is the checkbox spelling");
    }

    #[test]
    fn the_invite_token_is_a_read_optional() {
        // Unlike the ignored extras, Pleroma's `token` binds: the gate in
        // `load_invite` weighs it, and a body without it still parses.
        let with = registration_params(json!({
            "username": "lain",
            "password": "PlzDontHackLain",
            "agreement": true,
            "token": "VjtxW_-a-some-random-text",
        }))
        .expect("a body carrying an invite");
        assert_eq!(
            with.token.as_deref(),
            Some("VjtxW_-a-some-random-text"),
            "the token text arrives verbatim"
        );
    }

    #[test]
    fn a_missing_or_unboolean_agreement_is_unprocessable() {
        for value in [
            json!({"username": "lain", "password": "x"}),
            json!({"username": "lain", "password": "x", "agreement": "maybe"}),
        ] {
            let err = registration_params(value).expect_err("`agreement` is required");
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "a bad agreement is a 422, got {err}"
            );
        }
    }

    #[test]
    fn registration_params_map_onto_the_register_model() {
        let params = registration_params(json!({
            "username": "lain",
            "email": "lain@example.org",
            "password": "PlzDontHackLain",
            "agreement": true,
            "fullname": "Lain Iwakura",
            "bio": "wired",
            "reason": "let me in",
            "locale": "ja",
            "confirm": "PlzDontHackLain",
        }))
        .expect("a full body");
        let register = params.into_register_params();
        assert_eq!(register.nickname, "lain", "username is the nickname");
        assert_eq!(register.email.as_deref(), Some("lain@example.org"), "email");
        assert_eq!(register.password, "PlzDontHackLain", "password");
        assert_eq!(register.name.as_deref(), Some("Lain Iwakura"), "fullname");
        assert_eq!(register.bio.as_deref(), Some("wired"), "bio");
        assert_eq!(register.reason.as_deref(), Some("let me in"), "reason");
    }

    #[test]
    fn the_registration_answer_is_the_token_json() {
        let stored = minted_token();
        let expected_id = stored.id;
        let expected_created = stored.inserted_at.timestamp();
        let body =
            serde_json::to_value(TokenResponse::new(stored, Some(&stored_user())).without_me())
                .unwrap();
        assert_eq!(body["token_type"], "Bearer", "bearer credentials");
        assert_eq!(body["access_token"], "access-secret", "the access token");
        assert_eq!(body["refresh_token"], "refresh-secret", "the refresh token");
        assert_eq!(
            body["scope"], "read write follow push",
            "the presenting token's scopes"
        );
        assert_eq!(body["created_at"], expected_created, "unix seconds");
        assert_eq!(
            body["id"],
            expected_id.to_string().as_str(),
            "hyphenated `UUIDv7`"
        );
        let ttl = i64::try_from(crate::config::DEFAULT_OAUTH_TOKEN_TTL_SECS).unwrap();
        assert!(
            (ttl - 60..=ttl + 1).contains(&body["expires_in"].as_i64().unwrap()),
            "expires_in counts down the month-long lifetime"
        );
        assert_eq!(
            body.as_object().expect("an object").len(),
            7,
            "exactly the ticket's seven keys, no `me`"
        );
    }

    /// `stored_user` with the two registration-mode flags set as named: the
    /// row [`RegistrationMessage::for_user`] reads.
    fn registered(approval_pending: Flag, email_confirmed: Flag) -> User {
        let mut user = stored_user();
        user.approval_pending = approval_pending;
        user.email_confirmed = email_confirmed;
        user
    }

    #[test]
    fn a_ready_registration_owes_no_message() {
        assert!(
            RegistrationMessage::for_user(&registered(Flag::new(false), Flag::new(true))).is_none(),
            "approved and confirmed: the token answers"
        );
    }

    /// The two pending answers, in Pleroma's exact wording and key order,
    /// and the order the two modes are reported in.
    #[test]
    fn a_pending_registration_answers_the_message_and_identifier() {
        let approval = serde_json::to_value(RegistrationMessage::for_user(&registered(
            Flag::new(true),
            Flag::new(true),
        )))
        .expect("a message serializes");
        assert_eq!(
            approval,
            json!({
                "message": REGISTERED_AWAITING_APPROVAL,
                "identifier": "awaiting_approval",
            }),
            "the approval answer, with no token keys beside it"
        );
        assert_eq!(
            approval.as_object().expect("an object").len(),
            2,
            "message and identifier only"
        );
        let confirmation = serde_json::to_value(RegistrationMessage::for_user(&registered(
            Flag::new(false),
            Flag::new(false),
        )))
        .expect("a message serializes");
        assert_eq!(
            confirmation,
            json!({
                "message": REGISTERED_AWAITING_CONFIRMATION,
                "identifier": "missing_confirmed_email",
            }),
            "the confirmation answer"
        );
    }

    #[test]
    fn approval_is_reported_before_confirmation() {
        let both = RegistrationMessage::for_user(&registered(Flag::new(true), Flag::new(false)))
            .expect("both wait");
        assert_eq!(
            both.identifier, AWAITING_APPROVAL_ID,
            "an account waiting on staff and on its mailbox is told about staff"
        );
        assert_eq!(
            both.message, REGISTERED_AWAITING_APPROVAL,
            "with the approval letter, which is the wait that ends last"
        );
    }

    #[test]
    fn the_registration_answer_serializes_either_arm_without_a_wrapper() {
        let ready = serde_json::to_value(RegistrationAnswer::Ready(
            TokenResponse::new(minted_token(), Some(&stored_user())).without_me(),
        ))
        .expect("the token arm");
        assert!(
            ready["access_token"].is_string() && ready["message"].is_null(),
            "the token JSON is written bare: {ready}"
        );
        let pending = serde_json::to_value(RegistrationAnswer::Pending(RegistrationMessage {
            message: REGISTERED_AWAITING_CONFIRMATION,
            identifier: EMAIL_UNCONFIRMED_ID,
        }))
        .expect("the message arm");
        assert!(
            pending.get("access_token").is_none() && pending["identifier"] == EMAIL_UNCONFIRMED_ID,
            "the message is written bare, with no credential in it: {pending}"
        );
    }
}
