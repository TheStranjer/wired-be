//! Creating local accounts and checking their passwords.
//!
//! [`create_local_user`] is the registration path: it validates the
//! submitted parameters the way Pleroma's `User.register_changeset`
//! (`lib/pleroma/user.ex`) does — collecting every failure into one
//! field-keyed error map — then hashes the password, generates the actor's
//! RSA key pair, derives the `ActivityPub` endpoint URLs and inserts the
//! row. A lost race against a concurrent registration on one of `users`'
//! unique constraints maps to the same field error as the up-front
//! availability check. When the request came in on a registration invite,
//! the caller hands the validated [`InviteToken`] in and its consumption
//! runs on the same transaction as the insert, so a failed registration
//! spends nothing.
//!
//! The row's starting state follows the two registration-mode settings
//! (see [`crate::config`]): `REGISTRATIONS_APPROVAL_REQUIRED` leaves a new
//! account awaiting staff approval (an invite skips that wait), and
//! `ACCOUNT_ACTIVATION_REQUIRED` leaves it unconfirmed with a
//! [`confirmation`] token and a queued letter. Neither mode changes what
//! the insert stores; both are read back off the row by the endpoints that
//! act on them.
//!
//! [`authenticate`] is the login path: it verifies the password against
//! any supported hash for the local, active user found by nickname or
//! email, transparently upgrading legacy (bcrypt / PBKDF2) hashes to
//! Argon2id; [`authenticate_any_status`] skips the active-user gate for
//! flows that check account readiness themselves.
//! Unknown identifiers are answered with a verification against a built-in
//! dummy hash so a wrong password and a missing account cost roughly the
//! same work.
//!
//! [`RegisterError`] converts into [`AppError`] exactly like Pleroma's
//! Mastodon-API account creation does: validation failures become a 422
//! whose `error` body value is the JSON *string* of the field map, e.g.
//! `{"error": "{\"nickname\":[\"has already been taken\"]}"}` (the shape
//! its `account_controller_test.exs` asserts).
//!
//! The work is split by job behind this file: `validate` holds the field
//! rules and their messages, `error` the failure type and its conversions,
//! `login` the password checks, `insert` the availability check and the
//! write, and `modes` what the two registration-mode settings decide. Those
//! children are this module's internals; what the rest of the crate uses is
//! here — [`create_local_user`], [`RegisterParams`], [`FieldErrors`],
//! [`RegisterError`], [`authenticate`] and [`authenticate_any_status`],
//! beside the crate-internal `is_plausible_email` — so outside paths stay
//! `crate::models::user::register::…`.
//!
//! [`AppError`]: crate::error::AppError

mod error;
mod insert;
mod login;
mod modes;
mod validate;

#[cfg(test)]
mod test_support;

pub use error::RegisterError;
pub use login::{authenticate, authenticate_any_status};

use std::{collections::BTreeMap, fmt};

use super::confirmation;
use crate::{
    models::{invite_token::InviteToken, user::User},
    services::webhooks,
    state::AppState,
};
use insert::{build_and_insert, check_taken};
use validate::{
    is_valid_email_domain, is_valid_email_local, normalize_email, validate_email,
    validate_nickname, validate_password,
};

/// Validation failures keyed by the field they belong to, mirroring the
/// map Ecto's `traverse_errors` produces in Pleroma, e.g.
/// `{"nickname": ["has already been taken"]}`.
pub type FieldErrors = BTreeMap<&'static str, Vec<String>>;

/// What a registration request claims about the account it wants.
#[derive(Clone)]
pub struct RegisterParams {
    /// Desired handle. Required, 1-30 characters matching
    /// `^[a-zA-Z0-9_-]+$`, unique case-insensitively, and not one of the
    /// reserved names.
    pub nickname: String,
    /// Contact email. Required unless
    /// [`accounts_email_optional`](crate::config::Config::accounts_email_optional);
    /// must look like `x@y.z` and be unique case-insensitively.
    pub email: Option<String>,
    /// Plaintext password. Required and at least
    /// [`min_password_length`](crate::config::Config::min_password_length)
    /// characters; stored only as a hash. Renders as `<redacted>` in
    /// `Debug` output.
    pub password: String,
    /// Display name; defaults to the nickname when absent or blank.
    pub name: Option<String>,
    /// Profile note as source text; stored as `raw_bio` verbatim and
    /// rendered to sanitized HTML for `bio` with [`text_to_html`].
    ///
    /// [`text_to_html`]: crate::util::format::text_to_html
    pub bio: Option<String>,
    /// Free-text registration reason for the later approval flow, stored
    /// verbatim in `users.registration_reason` (Pleroma's
    /// `reason` parameter); `None` stores `NULL`.
    pub reason: Option<String>,
    /// Whether an external provider has already proven the address: the
    /// external OAuth registration sets this when the form submits the
    /// very email its provider reported. `ACCOUNT_ACTIVATION_REQUIRED`
    /// then registers the account confirmed and queues no letter (the
    /// address needed no link to prove it); `false` — the only value
    /// `POST /api/v1/accounts` ever gives — leaves the confirmation mode
    /// in full force.
    pub provider_email_verified: bool,
}

/// The password is secret material and renders as `<redacted>`; everything
/// else shows.
impl fmt::Debug for RegisterParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisterParams")
            .field("nickname", &self.nickname)
            .field("email", &self.email)
            .field("password", &"<redacted>")
            .field("name", &self.name)
            .field("bio", &self.bio)
            .field("reason", &self.reason)
            .field("provider_email_verified", &self.provider_email_verified)
            .finish()
    }
}

/// Creates a new local account from `params` and returns the stored row.
///
/// The password is hashed ([`hash_password`]), a fresh RSA-2048 key pair is
/// generated ([`generate_keypair_async`]), and the actor URL comes from
/// [`local_ap_id`] with its `ActivityPub` endpoints beside it; the shared
/// inbox is the instance-wide `{BASE_URL}/inbox`. Nickname and email
/// availability are checked up front and the unique constraints are
/// re-checked through the insert, so a concurrent registration of the same
/// account loses with the same field error the check would have produced.
///
/// `invite` is the already-validated [`InviteToken`] the registration came
/// in with, or `None` for an open-registrations request: an invited
/// account's [`consume`] runs on the same transaction as the user insert,
/// so a registration that fails validation spends no invite and the
/// invite is never left spent without its account.
///
/// The two registration modes decide what the finished row says about
/// itself. With `REGISTRATIONS_APPROVAL_REQUIRED` the account starts with
/// `approval_pending` set — except on an invite, which already is a staff
/// decision, exactly as Pleroma's `create_user_with_invite` passes
/// `approved: true`. With `ACCOUNT_ACTIVATION_REQUIRED` an account that
/// gave an address starts unconfirmed, and its confirmation letter — the
/// token and the link it carries included — goes out through
/// [`confirmation::send`] once the row exists. An account with no address
/// to confirm (an `ACCOUNTS_EMAIL_OPTIONAL` instance) is registered
/// confirmed: there is nothing a link could prove. So is one whose
/// address an external provider already vouched for
/// ([`RegisterParams::provider_email_verified`], the external-OAuth
/// registration's rule): the proof exists, the letter would ask for it
/// again.
///
/// A row that exists is announced: [`webhooks::account_created`] queues the
/// signed `account.created` event for every subscribed webhook, the way
/// Pleroma's `User.handle_creation` triggers it for any account the instance
/// creates — which is why the event hangs off this one path instead of off a
/// single endpoint.
///
/// # Errors
///
/// [`RegisterError::Invalid`] with every failed rule keyed by field when
/// validation or an availability check does not pass;
/// [`RegisterError::Failed`] for hashing, key generation, the confirmation
/// queue or database failures beyond those.
///
/// [`consume`]: crate::models::invite_token::consume
/// [`generate_keypair_async`]: crate::util::keys::generate_keypair_async
/// [`hash_password`]: crate::util::password::hash_password
/// [`local_ap_id`]: crate::models::user::local_ap_id
pub async fn create_local_user(
    state: &AppState,
    params: RegisterParams,
    invite: Option<&InviteToken>,
) -> Result<User, RegisterError> {
    let nickname = params.nickname.trim().to_owned();
    let email = normalize_email(params.email.as_deref());
    let mut errors = FieldErrors::new();
    validate_nickname(&nickname, &mut errors);
    validate_email(email.as_deref(), &state.config, &mut errors);
    validate_password(&params.password, &state.config, &mut errors);
    check_taken(state, &nickname, email.as_deref(), &mut errors).await?;
    if !errors.is_empty() {
        return Err(RegisterError::Invalid(errors));
    }
    let user = build_and_insert(state, &params, &nickname, email, invite).await?;
    if !user.email_confirmed.get() {
        confirmation::send(state, &user).await?;
    }
    webhooks::account_created(state, &user).await;
    Ok(user)
}

/// The nickname a request for `requested` would register, or why it cannot.
///
/// It applies the rules [`create_local_user`] applies to a nickname (trim,
/// then [`validate_nickname`]) and nothing else: availability is a separate
/// question, answered by [`nickname_taken`](super::nickname_taken). This is
/// `ActivityPub`'s identifier validator.
///
/// # Errors
///
/// Every message the nickname earned, in the order the rules ran.
pub(crate) fn validated_nickname(requested: &str) -> Result<String, Vec<String>> {
    let nickname = requested.trim();
    let mut errors = FieldErrors::new();
    validate_nickname(nickname, &mut errors);
    errors
        .remove(NICKNAME_FIELD)
        .map_or_else(|| Ok(nickname.to_owned()), Err)
}

/// The [`FieldErrors`] key of the nickname, which `ActivityPub` also uses for its identifier.
pub(crate) const NICKNAME_FIELD: &str = "nickname";

/// Whether `email` looks like `x@y.z`: a non-empty local part, exactly one
/// `@`, and a domain of at least two dot-separated hostname labels
/// (Pleroma's local-part class, and a dotted domain as Mastodon's email
/// validation requires). Also the shape rule of
/// `POST /api/pleroma/change_email` (see
/// [`crate::api::pleroma_account_settings`]).
pub(crate) fn is_plausible_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    is_valid_email_local(local) && is_valid_email_domain(domain)
}

#[cfg(test)]
mod tests {
    use rsa::traits::PublicKeyParts as _;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{
            featured, followers, following, get_by_nickname, inbox, outbox,
            register::test_support::*,
        },
        test_support::app_state,
        util::{
            format::text_to_html,
            keys::{parse_private_key, parse_public_key, sign_sha256, verify_sha256},
            password::verify_password,
        },
    };

    /// PEM label opening a SPKI public key.
    const PUBLIC_PEM_LABEL: &str = "-----BEGIN PUBLIC KEY-----";
    /// PEM label opening a PKCS#8 private key.
    const PRIVATE_PEM_LABEL: &str = "-----BEGIN PRIVATE KEY-----";
    /// Text signed and verified through a registered user's keys.
    const SIGNED_DATA: &[u8] = b"GET /users/alice HTTP/1.1\nhost: test.example\n";

    /// The profile `create_local_user` stored for the registration: the
    /// trimmed nickname and email, the actor id on this instance, the
    /// registration reason kept verbatim, and the bio sanitized but never
    /// re-written.
    fn assert_registered_profile(user: &User, raw_bio: &str) {
        assert_eq!(user.nickname, "Alice", "trimmed, case preserved");
        assert!(user.local.get(), "a registration is a local account");
        assert!(user.is_active.get(), "and an active one");
        assert_eq!(
            user.ap_id,
            format!("{TEST_BASE_URL}/users/Alice"),
            "ap_id is local_ap_id on the instance"
        );
        assert_eq!(
            user.email.as_deref(),
            Some("Alice@Example.ORG"),
            "the trimmed email is stored as given"
        );
        assert_eq!(user.name, "Alice", "the name defaults to the nickname");
        assert_eq!(user.raw_bio, raw_bio, "raw_bio keeps the source text");
        assert_eq!(
            user.registration_reason.as_deref(),
            Some("I'm a cool dude, bro"),
            "the registration reason is stored verbatim"
        );
        assert_eq!(user.bio, text_to_html(raw_bio), "the formatter output");
        assert!(
            !user.bio.contains("<script") && user.bio.contains("https://example.org"),
            "bio is sanitized, linkified HTML, got {:?}",
            user.bio
        );
    }

    /// The five activity endpoints and the shared inbox registration derives
    /// from the new account's actor id.
    fn assert_actor_endpoints(user: &User) {
        let ap_id = user.ap_id.as_str();
        assert_eq!(user.inbox.as_deref(), Some(inbox(ap_id).as_str()), "inbox");
        assert_eq!(
            user.outbox.as_deref(),
            Some(outbox(ap_id).as_str()),
            "outbox"
        );
        assert_eq!(
            user.followers_url.as_deref(),
            Some(followers(ap_id).as_str()),
            "followers"
        );
        assert_eq!(
            user.following_url.as_deref(),
            Some(following(ap_id).as_str()),
            "following"
        );
        assert_eq!(
            user.featured_url.as_deref(),
            Some(featured(ap_id).as_str()),
            "featured"
        );
        assert_eq!(
            user.shared_inbox.as_deref(),
            Some(format!("{TEST_BASE_URL}/inbox").as_str()),
            "the shared inbox is the instance-wide one"
        );
    }

    /// The key pair registration generated with the account: two labeled PEMs
    /// that are one RSA pair, proven by signing and verifying with them.
    ///
    /// # Panics
    ///
    /// If the row carries no keys, which registration always stores.
    fn assert_generated_key_pair(user: &User) {
        let public_pem = user.public_key.clone().expect("a public key");
        let private_pem = user.private_key.clone().expect("a private key");
        assert!(
            public_pem.starts_with(PUBLIC_PEM_LABEL),
            "SPKI PEM public key"
        );
        assert!(
            private_pem.starts_with(PRIVATE_PEM_LABEL),
            "PKCS#8 PEM private key"
        );
        let public = parse_public_key(&public_pem).expect("the public key parses");
        let private = parse_private_key(&private_pem).expect("the private key parses");
        assert_eq!(
            public.n(),
            private.n(),
            "the parsed halves belong to one key pair"
        );
        let signature = sign_sha256(&private_pem, SIGNED_DATA).expect("the key signs");
        assert!(
            verify_sha256(&public_pem, SIGNED_DATA, &signature),
            "the pair is a real key pair"
        );
    }

    #[tokio::test]
    async fn register_stores_a_complete_local_account() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let raw_bio = "Hello <script>evil()</script> visit https://example.org";
        let params = RegisterParams {
            nickname: "  Alice  ".to_owned(),
            email: Some(" Alice@Example.ORG ".to_owned()),
            password: PASSWORD.to_owned(),
            name: None,
            bio: Some(raw_bio.to_owned()),
            reason: Some("I'm a cool dude, bro".to_owned()),
            provider_email_verified: false,
        };
        let user = create_local_user(&state, params, None).await.unwrap();

        assert_registered_profile(&user, raw_bio);

        let hash = user.password_hash.clone().expect("a password was set");
        assert!(hash.starts_with("$argon2id$"), "the hash is Argon2id");
        assert!(
            verify_password(PASSWORD, &hash),
            "and it verifies the submitted password"
        );

        assert_actor_endpoints(&user);
        assert_generated_key_pair(&user);
        let actor = crate::test_fixtures::assert_actor_in_step(&pool, user.id).await;
        assert_eq!(
            actor.private_key, user.private_key,
            "the actor row holds the generated private key"
        );
        assert!(
            actor.public_key.is_some() && actor.inbox.is_some(),
            "and the key pair's public half and the inbox"
        );

        let fetched = get_by_nickname(&state.db, "aLiCe")
            .await
            .unwrap()
            .expect("citext finds the row");
        assert_eq!(fetched.id, user.id, "one row, found in any case");
    }

    #[test]
    fn params_debug_redacts_the_password() {
        let params = params_for("alice");
        let debug = format!("{params:?}");
        assert!(
            !debug.contains(PASSWORD),
            "the password must not leak into Debug output"
        );
        assert!(
            debug.contains(r#"password: "<redacted>""#),
            "the redaction marker names the field"
        );
        assert!(debug.contains("alice"), "the rest is visible");
    }
}
