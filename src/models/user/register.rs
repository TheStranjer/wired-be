//! Creating local accounts and checking their passwords.
//!
//! [`create_local_user`] is the registration path: it validates the
//! submitted parameters the way Pleroma's `User.register_changeset`
//! (`lib/pleroma/user.ex`) does — collecting every failure into one
//! field-keyed error map — then hashes the password, generates the actor's
//! RSA key pair, derives the `ActivityPub` endpoint URLs and inserts the
//! row. A lost race against a concurrent registration on one of `users`'
//! unique constraints maps to the same field error as the up-front
//! availability check.
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

use std::{collections::BTreeMap, error::Error, fmt};

use sqlx::{Error as SqlxError, PgPool};
use tracing::warn;
use uuid::Uuid;

use super::{
    NewUser, User, featured, followers, following, get_by_email, get_by_nickname, inbox, insert,
    local_ap_id, outbox,
};
use crate::{
    config::Config,
    error::{AppError, AppResult},
    state::AppState,
    util::{
        format::text_to_html,
        keys::generate_keypair_async,
        password::{hash_password, needs_rehash, verify_password},
        sanitize::sanitize_display_name,
    },
};

/// Ecto's `validate_required` message: a value the account cannot be
/// created without is missing or blank.
const CANNOT_BE_BLANK: &str = "can't be blank";
/// Ecto's `validate_format` message.
const HAS_INVALID_FORMAT: &str = "has invalid format";
/// Ecto's `unique_constraint` message, from the index behind it.
const HAS_ALREADY_BEEN_TAKEN: &str = "has already been taken";
/// Pleroma's `validate_not_restricted_nickname` message for a reserved
/// nickname (`lib/pleroma/user.ex`).
const INVALID_NICKNAME: &str = "Invalid nickname";
/// SQLSTATE of a Postgres unique violation.
const SQLSTATE_UNIQUE_VIOLATION: &str = "23505";
/// Longest allowed nickname; the shortest is 1, which the blank rule
/// already enforces (Pleroma-era length bound 1..30).
const NICKNAME_MAX_LEN: usize = 30;

/// Nicknames registration never grants: routes this instance owns plus the
/// spirit of Pleroma's `restricted_nicknames` (`lib/pleroma/user.ex`); the
/// comparison in [`is_reserved`] is case-insensitive.
const RESERVED_NICKNAMES: [&str; 24] = [
    "admin",
    "root",
    "api",
    "users",
    "inbox",
    "outbox",
    "relay",
    "internal",
    "media",
    "proxy",
    "oauth",
    "objects",
    "activities",
    "tags",
    "notice",
    "main",
    "static",
    "system",
    ".well-known",
    "nodeinfo",
    "registration",
    "auth",
    "feed",
    "embed",
];

/// A valid Argon2id PHC string of a password nobody will ever type, used
/// only by [`verify_dummy_password`] so a lookup that found no row still
/// pays the cost of one real verification.
const DUMMY_PASSWORD_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$/uRDL24D8QHL5NRLmAficA$Eoza8QsFsJDqfi2oMa05IbXRgPgZ7Kb4t6nxTSc4sIs";

/// The login lookup: local rows at any account status, matched by citext
/// nickname or email against the single bind `$1`. Whether a found account
/// is ready to receive a token is the caller's decision
/// ([`authenticate`] demands an active one; the OAuth password grant runs
/// its own, richer check); a remote mirror is never a candidate.
const LOGIN_LOOKUP_SQL: &str = "SELECT * FROM users
     WHERE local
       AND (nickname = $1::citext OR email = $1::citext)";

/// Characters the email local part may use: ASCII alphanumerics plus the
/// specials Pleroma's `@email_regex` allows (`lib/pleroma/user.ex`).
const EMAIL_LOCAL_SPECIALS: &str = ".!#$%&'*+/=?^_`{|}~-";

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
    /// [`accounts_email_optional`](Config::accounts_email_optional); must
    /// look like `x@y.z` and be unique case-insensitively.
    pub email: Option<String>,
    /// Plaintext password. Required and at least
    /// [`min_password_length`](Config::min_password_length) characters;
    /// stored only as a hash. Renders as `<redacted>` in `Debug` output.
    pub password: String,
    /// Display name; defaults to the nickname when absent or blank.
    pub name: Option<String>,
    /// Profile note as source text; stored as `raw_bio` verbatim and
    /// rendered to sanitized HTML for `bio` with [`text_to_html`].
    pub bio: Option<String>,
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
            .finish()
    }
}

/// Every way registration can fail.
#[derive(Debug)]
pub enum RegisterError {
    /// The submitted parameters did not pass validation; the map is keyed
    /// by field name, exactly like Pleroma's changeset errors.
    Invalid(FieldErrors),
    /// A server-side failure that is not the registrant's fault: password
    /// hashing, key generation or the database.
    Failed(AppError),
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(errors) => {
                let rendered: Vec<String> = errors
                    .iter()
                    .map(|(field, messages)| format!("{field}: {}", messages.join(", ")))
                    .collect();
                write!(
                    f,
                    "invalid registration parameters: {}",
                    rendered.join("; ")
                )
            }
            Self::Failed(err) => write!(f, "could not create the account: {err}"),
        }
    }
}

impl Error for RegisterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(_) => None,
            Self::Failed(err) => Some(err),
        }
    }
}

/// Any server-side failure is [`RegisterError::Failed`], so `?` works on
/// hashing, key generation and database calls inside the registration.
impl From<AppError> for RegisterError {
    fn from(err: AppError) -> Self {
        Self::Failed(err)
    }
}

/// Validation failures become the 422 Pleroma's Mastodon-API account
/// creation sends: `{"error": "<json-encoded errors map as string>"}`, the
/// `error` value being the JSON string of the field map (decision 8's body
/// shape carrying [`FieldErrors`] encoded by `serde_json`).
impl From<RegisterError> for AppError {
    fn from(err: RegisterError) -> Self {
        match err {
            RegisterError::Invalid(errors) => match serde_json::to_string(&errors) {
                Ok(encoded) => Self::Unprocessable(encoded),
                Err(encode_err) => Self::Internal(format!(
                    "could not encode the registration errors: {encode_err}"
                )),
            },
            RegisterError::Failed(err) => err,
        }
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
/// # Errors
///
/// [`RegisterError::Invalid`] with every failed rule keyed by field when
/// validation or an availability check does not pass;
/// [`RegisterError::Failed`] for hashing, key generation or database
/// failures beyond those.
pub async fn create_local_user(
    state: &AppState,
    params: RegisterParams,
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
    build_and_insert(state, &params, &nickname, email).await
}

/// Verifies `password` for the local, active user whose nickname or email
/// is `nickname_or_email`, returning the row on success.
///
/// A remote mirror, a deactivated account, an unknown identifier, an
/// account without a password hash or simply the wrong password all answer
/// `None`. This is the strict gate every ordinary login path wants; the
/// OAuth password grant instead authenticates with
/// [`authenticate_any_status`] and reports a deactivated account as a
/// distinct `account_is_disabled` error, so it does not route that case
/// through here.
pub async fn authenticate(pool: &PgPool, nickname_or_email: &str, password: &str) -> Option<User> {
    let user = authenticate_any_status(pool, nickname_or_email, password).await?;
    // The credential was correct; now enforce the ordinary active-user rule.
    user.is_active.get().then_some(user)
}

/// Verifies `password` for the local user whose nickname or email is
/// `nickname_or_email`, regardless of whether that account is active,
/// returning the row on success.
///
/// This is [`authenticate`] minus the active-user gate, for callers that
/// must distinguish a wrong password from a deactivated account (the OAuth
/// password grant answers the former with `Invalid credentials` and the
/// latter with `account_is_disabled`). A remote mirror, an unknown
/// identifier, an account without a password hash and the wrong password
/// all answer `None`; unknown identifiers and hashless accounts are checked
/// against a built-in dummy hash so they cost about the same as verifying a
/// wrong password, and a password that matches a legacy hash
/// ([`needs_rehash`]) is transparently re-stored as Argon2id before the row
/// is returned. A deactivated account whose password matches is returned
/// with its `is_active` flag `false`; the caller decides what that means.
pub async fn authenticate_any_status(
    pool: &PgPool,
    nickname_or_email: &str,
    password: &str,
) -> Option<User> {
    let Some(mut user) = find_login_candidate(pool, nickname_or_email).await else {
        verify_dummy_password(password);
        return None;
    };
    let Some(stored) = user.password_hash.as_deref() else {
        verify_dummy_password(password);
        return None;
    };
    if !verify_password(password, stored) {
        return None;
    }
    if needs_rehash(stored) {
        upgrade_password_hash(pool, &mut user, password).await;
    }
    Some(user)
}

/// The login lookup: the local row whose nickname or email matches
/// `identifier` (citext folds case in both), whatever the account's
/// status. A database failure is logged and answered `None`, like an
/// unknown account.
async fn find_login_candidate(pool: &PgPool, identifier: &str) -> Option<User> {
    let found = sqlx::query_as::<_, User>(LOGIN_LOOKUP_SQL)
        .bind(identifier)
        .fetch_optional(pool)
        .await;
    match found {
        Ok(user) => user,
        Err(err) => {
            warn!(%err, "login lookup failed");
            None
        }
    }
}

/// Runs one real Argon2id verification against [`DUMMY_PASSWORD_HASH`].
/// Its result is meaningless — the dummy hash verifies nothing a caller
/// submitted — only its work is wanted, so a login attempt against an
/// unknown account is not cheaply distinguishable from a wrong password.
fn verify_dummy_password(password: &str) {
    if verify_password(password, DUMMY_PASSWORD_HASH) {
        warn!("the dummy verification hash matched a submitted password");
    }
}

/// Upgrades `user`'s verified legacy hash to Argon2id in the row and in the
/// returned struct. A failure here never revokes a successful login; it is
/// only logged, and the user keeps their (working) legacy hash.
async fn upgrade_password_hash(pool: &PgPool, user: &mut User, password: &str) {
    match hash_password(password) {
        Ok(new_hash) => match set_password_hash(pool, user.id, &new_hash).await {
            Ok(()) => user.password_hash = Some(new_hash),
            Err(err) => warn!(%err, "could not store the upgraded password hash"),
        },
        Err(err) => warn!(%err, "could not rehash the verified password"),
    }
}

/// Writes a user's `password_hash` and refreshes `updated_at`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
async fn set_password_hash(pool: &PgPool, id: Uuid, hash: &str) -> AppResult<()> {
    sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(hash)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The registration's second half, reached only once every rule passed:
/// hash the password, generate the keys, assemble the [`NewUser`] and
/// insert it.
async fn build_and_insert(
    state: &AppState,
    params: &RegisterParams,
    nickname: &str,
    email: Option<String>,
) -> Result<User, RegisterError> {
    let password_hash = hash_password(&params.password)?;
    let keys = generate_keypair_async().await?;
    let ap_id = local_ap_id(&state.config.base_url, nickname);
    let shared_inbox = format!("{}/inbox", state.config.base_url);
    let raw_bio = params.bio.clone().unwrap_or_default();
    let bio = if raw_bio.is_empty() {
        String::new()
    } else {
        text_to_html(&raw_bio)
    };
    let new_user = NewUser {
        email,
        password_hash: Some(password_hash),
        name: display_name(params.name.as_deref(), nickname),
        bio,
        raw_bio,
        inbox: Some(inbox(&ap_id)),
        shared_inbox: Some(shared_inbox),
        outbox: Some(outbox(&ap_id)),
        followers_url: Some(followers(&ap_id)),
        following_url: Some(following(&ap_id)),
        featured_url: Some(featured(&ap_id)),
        public_key: Some(keys.public_pem),
        private_key: Some(keys.private_pem),
        ..NewUser::local(nickname, &ap_id)
    };
    match insert(&state.db, &new_user).await {
        Ok(user) => Ok(user),
        Err(err) => Err(map_insert_error(err)),
    }
}

/// Turns a database failure from the insert into a [`RegisterError`]: a
/// Postgres unique violation (SQLSTATE `23505`) on one of `users`' unique
/// constraints lost the race against a concurrent registration and
/// arrives as the same field error the availability check would have
/// produced; anything else stays a [`RegisterError::Failed`].
fn map_insert_error(err: AppError) -> RegisterError {
    unique_violation_field(&err).map_or_else(
        || RegisterError::Failed(err),
        |field| {
            let mut errors = FieldErrors::new();
            add_error(&mut errors, field, HAS_ALREADY_BEEN_TAKEN);
            RegisterError::Invalid(errors)
        },
    )
}

/// The `users` field whose unique constraint `err` violated, if `err` is a
/// Postgres `23505` on one of those constraints at all.
fn unique_violation_field(err: &AppError) -> Option<&'static str> {
    let AppError::Database(SqlxError::Database(db_err)) = err else {
        return None;
    };
    if db_err.code().as_deref() != Some(SQLSTATE_UNIQUE_VIOLATION) {
        return None;
    }
    db_err.constraint().and_then(constraint_field)
}

/// The field protected by the `users` unique constraint `constraint`
/// (Postgres' default names for the table's three column constraints).
fn constraint_field(constraint: &str) -> Option<&'static str> {
    match constraint {
        "users_nickname_key" => Some("nickname"),
        "users_email_key" => Some("email"),
        "users_ap_id_key" => Some("ap_id"),
        _ => None,
    }
}

/// Appends one validation `message` under `field`, creating its list.
fn add_error(errors: &mut FieldErrors, field: &'static str, message: &str) {
    errors.entry(field).or_default().push(message.to_owned());
}

/// Ecto's interpolated `too_short` message for a minimum of `count`
/// characters.
fn at_least(count: usize) -> String {
    format!("should be at least {count} character(s)")
}

/// Ecto's interpolated `too_long` message for a maximum of `count`
/// characters.
fn at_most(count: usize) -> String {
    format!("should be at most {count} character(s)")
}

/// Pleroma's shape rules on one (already trimmed) nickname: the blank rule
/// of `validate_required`, the 1..30 length bound, the
/// `~r/^[a-zA-Z0-9_-]+$/` format and `validate_not_restricted_nickname`.
fn validate_nickname(nickname: &str, errors: &mut FieldErrors) {
    if nickname.is_empty() {
        add_error(errors, "nickname", CANNOT_BE_BLANK);
        return;
    }
    if nickname.chars().count() > NICKNAME_MAX_LEN {
        add_error(errors, "nickname", &at_most(NICKNAME_MAX_LEN));
    }
    if !is_local_nickname(nickname) {
        add_error(errors, "nickname", HAS_INVALID_FORMAT);
    }
    if is_reserved(nickname) {
        add_error(errors, "nickname", INVALID_NICKNAME);
    }
}

/// Whether `nickname` is what `^[a-zA-Z0-9_-]+$` matches: one or more ASCII
/// letters, digits, `_` or `-`. The character class is checked by hand (as
/// in [`crate::util::format`]) — that language needs no regex engine and
/// cannot backtrack.
fn is_local_nickname(nickname: &str) -> bool {
    !nickname.is_empty()
        && nickname
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Whether `nickname` is one of [`RESERVED_NICKNAMES`], case-insensitively
/// (Pleroma compares lower-cased spellings).
fn is_reserved(nickname: &str) -> bool {
    RESERVED_NICKNAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(nickname))
}

/// The submitted email trimmed, with an all-blank submission normalized to
/// `None` so it reads as "not given" (Ecto's `trim: true` cast).
fn normalize_email(email: Option<&str>) -> Option<String> {
    email
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The email rules: required unless
/// [`accounts_email_optional`](Config::accounts_email_optional), and a
/// plausibly-formed `x@y.z` when given.
fn validate_email(email: Option<&str>, config: &Config, errors: &mut FieldErrors) {
    match email {
        None if !config.accounts_email_optional => add_error(errors, "email", CANNOT_BE_BLANK),
        Some(value) if !is_plausible_email(value) => add_error(errors, "email", HAS_INVALID_FORMAT),
        None | Some(_) => {}
    }
}

/// Whether `email` looks like `x@y.z`: a non-empty local part, exactly one
/// `@`, and a domain of at least two dot-separated hostname labels
/// (Pleroma's local-part class, and a dotted domain as Mastodon's email
/// validation requires).
fn is_plausible_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    is_valid_email_local(local) && is_valid_email_domain(domain)
}

/// The local part: at least one allowed character.
fn is_valid_email_local(local: &str) -> bool {
    !local.is_empty()
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || EMAIL_LOCAL_SPECIALS.contains(c))
}

/// The domain: at least two labels, each a valid hostname label.
fn is_valid_email_domain(domain: &str) -> bool {
    domain.split('.').count() >= 2 && domain.split('.').all(is_valid_email_label)
}

/// A hostname label: alphanumerics and inner hyphens, starting and ending
/// alphanumeric.
fn is_valid_email_label(label: &str) -> bool {
    !label.is_empty()
        && label
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && label
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The password rules: required, and at least
/// [`min_password_length`](Config::min_password_length) characters
/// (counted in characters, not bytes).
fn validate_password(password: &str, config: &Config, errors: &mut FieldErrors) {
    if password.is_empty() {
        add_error(errors, "password", CANNOT_BE_BLANK);
        return;
    }
    if password.chars().count() < config.min_password_length {
        add_error(errors, "password", &at_least(config.min_password_length));
    }
}

/// Whether the submitted nickname and email are free. The lookups are
/// citext comparisons, so "taken" means taken in any case.
async fn check_taken(
    state: &AppState,
    nickname: &str,
    email: Option<&str>,
    errors: &mut FieldErrors,
) -> Result<(), RegisterError> {
    if get_by_nickname(&state.db, nickname).await?.is_some() {
        add_error(errors, "nickname", HAS_ALREADY_BEEN_TAKEN);
    }
    if let Some(email) = email
        && get_by_email(&state.db, email).await?.is_some()
    {
        add_error(errors, "email", HAS_ALREADY_BEEN_TAKEN);
    }
    Ok(())
}

/// The display name to store: the requested name reduced to one line of
/// plain text, falling back to the nickname when nothing usable was given
/// (Pleroma renders `name` through the same display-name sanitizer).
fn display_name(requested: Option<&str>, nickname: &str) -> String {
    requested
        .map(sanitize_display_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| nickname.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rsa::traits::PublicKeyParts as _;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        http_client::HttpClient,
        models::user::{Flag, get_by_id},
        uploads::UploaderBackend,
        util::{
            id::new_id,
            keys::{parse_private_key, parse_public_key, sign_sha256, verify_sha256},
        },
    };

    /// The password every registration test submits (24 characters).
    const PASSWORD: &str = "correct horse battery staple";
    /// One character short of the default 8-character minimum.
    const SHORT_PASSWORD: &str = "short_7";
    /// A bcrypt `$2b$` hash of [`PASSWORD`] (the same vector
    /// `src/util/password.rs` verifies), for the rehash test.
    const BCRYPT_2B: &str = "$2b$12$28E9Mpz84hFJrMMxMmid6.2qqJZdjG.YOLhO5OjHkMJ2Lmm4AaGr2";
    /// PEM label opening a SPKI public key.
    const PUBLIC_PEM_LABEL: &str = "-----BEGIN PUBLIC KEY-----";
    /// PEM label opening a PKCS#8 private key.
    const PRIVATE_PEM_LABEL: &str = "-----BEGIN PRIVATE KEY-----";
    /// Text signed and verified through a registered user's keys.
    const SIGNED_DATA: &[u8] = b"GET /users/alice HTTP/1.1\nhost: test.example\n";

    /// An [`AppState`] over `pool` with the fixed test configuration.
    fn test_state(pool: &PgPool) -> AppState {
        test_state_with(pool, Config::for_tests())
    }

    /// An [`AppState`] over `pool` with `config`.
    fn test_state_with(pool: &PgPool, config: Config) -> AppState {
        let config = Arc::new(config);
        AppState {
            db: pool.clone(),
            hub: crate::streaming::hub::Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
        }
    }

    /// Valid registration params for `nickname` at `nickname@example.org`.
    fn params_for(nickname: &str) -> RegisterParams {
        RegisterParams {
            nickname: nickname.to_owned(),
            email: Some(format!("{nickname}@example.org")),
            password: PASSWORD.to_owned(),
            name: None,
            bio: None,
        }
    }

    /// A local [`NewUser`] named `nickname` on the test instance.
    fn local_new(nickname: &str) -> NewUser {
        NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
    }

    /// Inserts a local user with `email` and `password_hash` straight
    /// through [`insert`] — no keys, no validation, no password work.
    async fn insert_login(
        pool: &PgPool,
        nickname: &str,
        email: &str,
        password_hash: Option<&str>,
    ) -> User {
        insert(
            pool,
            &NewUser {
                email: Some(email.to_owned()),
                password_hash: password_hash.map(str::to_owned),
                ..local_new(nickname)
            },
        )
        .await
        .expect("the login fixture inserts")
    }

    /// Every message the pure validation of `nickname` produced.
    fn nickname_errors(nickname: &str) -> Vec<String> {
        let mut errors = FieldErrors::new();
        validate_nickname(nickname, &mut errors);
        errors.remove("nickname").unwrap_or_default()
    }

    /// Every message the pure validation of `email` produced.
    fn email_errors(email: Option<&str>, config: &Config) -> Vec<String> {
        let normalized = normalize_email(email);
        let mut errors = FieldErrors::new();
        validate_email(normalized.as_deref(), config, &mut errors);
        errors.remove("email").unwrap_or_default()
    }

    /// Every message the pure validation of `password` produced.
    fn password_errors(password: &str, config: &Config) -> Vec<String> {
        let mut errors = FieldErrors::new();
        validate_password(password, config, &mut errors);
        errors.remove("password").unwrap_or_default()
    }

    /// The number of rows in `users`.
    async fn user_count(pool: &PgPool) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT count(*) FROM users")
            .fetch_one(pool)
            .await
            .expect("users can be counted");
        row.0
    }

    /// Sets a user's `is_active` flag directly, for the login tests.
    async fn set_active(pool: &PgPool, id: Uuid, active: bool) {
        let result = sqlx::query("UPDATE users SET is_active = $2 WHERE id = $1")
            .bind(id)
            .bind(Flag::new(active))
            .execute(pool)
            .await
            .expect("the flag update runs");
        assert_eq!(result.rows_affected(), 1, "the row exists to change");
    }

    #[test]
    fn nickname_rules() {
        assert_eq!(
            nickname_errors(""),
            vec![CANNOT_BE_BLANK],
            "a blank nickname is required"
        );
        assert!(
            nickname_errors("a").is_empty(),
            "one character is the shortest valid nickname"
        );
        assert!(
            nickname_errors(&"a".repeat(NICKNAME_MAX_LEN)).is_empty(),
            "thirty characters are still allowed"
        );
        assert_eq!(
            nickname_errors(&"a".repeat(NICKNAME_MAX_LEN + 1)),
            vec![at_most(NICKNAME_MAX_LEN)],
            "thirty-one characters are not"
        );
        for bad in ["a b", "a.b", "aé", "über", "a+b", "a@b"] {
            assert_eq!(
                nickname_errors(bad),
                vec![HAS_INVALID_FORMAT],
                "{bad:?} does not match ^[a-zA-Z0-9_-]+$"
            );
        }
        assert!(
            nickname_errors("Z-9_a").is_empty(),
            "letters, digits, _ and - are exactly the language"
        );
    }

    #[test]
    fn every_reserved_nickname_is_rejected_case_insensitively() {
        for reserved in RESERVED_NICKNAMES {
            let errors = nickname_errors(reserved);
            assert!(
                errors.contains(&INVALID_NICKNAME.to_owned()),
                "{reserved:?} is reserved, got {errors:?}"
            );
            let upper = reserved.to_uppercase();
            if upper != *reserved {
                assert!(
                    nickname_errors(&upper).contains(&INVALID_NICKNAME.to_owned()),
                    "{upper:?} is reserved too"
                );
            }
        }
        assert_eq!(
            nickname_errors("Admin"),
            vec![INVALID_NICKNAME],
            "a purely alphabetic reserved name yields exactly its one message"
        );
    }

    #[test]
    fn email_rules() {
        let required = Config::for_tests();
        assert_eq!(
            email_errors(None, &required),
            vec![CANNOT_BE_BLANK],
            "no email when it is required"
        );
        assert_eq!(
            email_errors(Some("   "), &required),
            vec![CANNOT_BE_BLANK],
            "a blank email reads as missing"
        );
        let optional = Config {
            accounts_email_optional: true,
            ..Config::for_tests()
        };
        assert!(
            email_errors(None, &optional).is_empty(),
            "no email passes when ACCOUNTS_EMAIL_OPTIONAL is on"
        );
        assert!(
            email_errors(Some("  "), &optional).is_empty(),
            "a blank email is not-given, not invalid"
        );
        for bad in [
            "plainaddress",
            "a@b",
            "@x.y",
            "a@",
            "a@.y",
            "a@y.",
            "a@y..z",
            "a b@x.y",
            "a@x y.z",
            "a@@x.y",
            "a@-x.y",
            "a@x-.y",
        ] {
            assert_eq!(
                email_errors(Some(bad), &required),
                vec![HAS_INVALID_FORMAT],
                "{bad:?} does not look like x@y.z"
            );
        }
        for good in [
            "x@y.z",
            "a@b.c",
            "A@B.C",
            "alice+tag@sub.example.org",
            "o'brien!#%&/=?^_`{|}~-@a-b.example.co.uk",
        ] {
            assert!(
                email_errors(Some(good), &required).is_empty(),
                "{good:?} looks like x@y.z"
            );
        }
    }

    #[test]
    fn password_rules() {
        let config = Config::for_tests();
        assert_eq!(
            password_errors("", &config),
            vec![CANNOT_BE_BLANK],
            "a blank password is required"
        );
        assert_eq!(
            password_errors(SHORT_PASSWORD, &config),
            vec![at_least(8)],
            "seven characters miss the default minimum of 8"
        );
        assert!(
            password_errors("exactly8", &config).is_empty(),
            "exactly eight characters pass"
        );
        assert_eq!(
            password_errors("ééééééé", &config),
            vec![at_least(8)],
            "the minimum counts characters, not bytes"
        );
        assert!(
            password_errors("éééééééé", &config).is_empty(),
            "eight non-ASCII characters are eight characters"
        );
        let strict = Config {
            min_password_length: 20,
            ..Config::for_tests()
        };
        assert_eq!(
            password_errors("twelve chars", &strict),
            vec![at_least(20)],
            "MIN_PASSWORD_LENGTH raises the bar"
        );
        let lenient = Config {
            min_password_length: SHORT_PASSWORD.chars().count(),
            ..Config::for_tests()
        };
        assert!(
            password_errors(SHORT_PASSWORD, &lenient).is_empty(),
            "and lowers it"
        );
    }

    #[test]
    fn display_names_default_to_the_nickname() {
        assert_eq!(display_name(None, "alice"), "alice", "absent");
        assert_eq!(
            display_name(Some("   "), "alice"),
            "alice",
            "blank after sanitizing"
        );
        assert_eq!(
            display_name(
                Some("<script>evil()</script>Bob <b>the</b> Builder"),
                "alice"
            ),
            "Bob the Builder",
            "markup reduces to one line of text"
        );
        assert_eq!(
            display_name(Some("  Real Name  "), "alice"),
            "Real Name",
            "ends are trimmed"
        );
    }

    #[test]
    fn constraint_field_maps_the_three_unique_constraints() {
        assert_eq!(
            constraint_field("users_nickname_key"),
            Some("nickname"),
            "the citext nickname constraint"
        );
        assert_eq!(
            constraint_field("users_email_key"),
            Some("email"),
            "the citext email constraint"
        );
        assert_eq!(
            constraint_field("users_ap_id_key"),
            Some("ap_id"),
            "the actor id constraint"
        );
        assert_eq!(
            constraint_field("jobs_pkey"),
            None,
            "an unrelated constraint maps to no field"
        );
    }

    #[test]
    fn unique_violation_field_only_answers_database_errors() {
        assert_eq!(
            unique_violation_field(&AppError::NotFound),
            None,
            "a plain AppError carries no constraint"
        );
        assert_eq!(
            unique_violation_field(&AppError::Database(SqlxError::PoolClosed)),
            None,
            "a driver-less sqlx failure carries no constraint"
        );
    }

    #[test]
    fn register_error_display_and_source() {
        let mut errors = FieldErrors::new();
        add_error(&mut errors, "nickname", HAS_ALREADY_BEEN_TAKEN);
        add_error(&mut errors, "email", CANNOT_BE_BLANK);
        add_error(&mut errors, "email", HAS_INVALID_FORMAT);
        let invalid = RegisterError::Invalid(errors);
        assert_eq!(
            invalid.to_string(),
            "invalid registration parameters: email: can't be blank, has invalid format; nickname: has already been taken",
            "every field and message, ordered by field"
        );
        assert!(invalid.source().is_none(), "validation has no cause");

        let failed = RegisterError::Failed(AppError::Internal("os rng died".to_owned()));
        assert_eq!(
            failed.to_string(),
            "could not create the account: os rng died",
            "the inner message shows through Display"
        );
        assert_eq!(
            failed
                .source()
                .expect("the cause survives")
                .to_string()
                .as_str(),
            "os rng died",
            "Failed keeps its AppError as source"
        );
    }

    #[test]
    fn invalid_becomes_a_422_wrapping_the_json_field_map() {
        let mut errors = FieldErrors::new();
        add_error(&mut errors, "nickname", HAS_ALREADY_BEEN_TAKEN);
        let app_err = AppError::from(RegisterError::Invalid(errors));
        assert_eq!(
            app_err.to_string(),
            r#"{"nickname":["has already been taken"]}"#,
            "the message is the JSON string of the field map, exactly the \
             value Pleroma's account creation puts in `error`"
        );
        let not_found = AppError::from(RegisterError::Failed(AppError::NotFound));
        assert!(
            matches!(not_found, AppError::NotFound),
            "Failed forwards the wrapped AppError unchanged"
        );
    }

    #[test]
    fn an_app_error_lifts_into_a_failed_registration() {
        let lifted: RegisterError = AppError::RateLimited.into();
        assert!(
            matches!(lifted, RegisterError::Failed(AppError::RateLimited)),
            "From<AppError> wraps for the ? operator"
        );
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

    /// The profile `create_local_user` stored for the registration: the
    /// trimmed nickname and email, the actor id on this instance, and the bio
    /// sanitized but never re-written.
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
        let state = test_state(&pool);
        let raw_bio = "Hello <script>evil()</script> visit https://example.org";
        let params = RegisterParams {
            nickname: "  Alice  ".to_owned(),
            email: Some(" Alice@Example.ORG ".to_owned()),
            password: PASSWORD.to_owned(),
            name: None,
            bio: Some(raw_bio.to_owned()),
        };
        let user = create_local_user(&state, params).await.unwrap();

        assert_registered_profile(&user, raw_bio);

        let hash = user.password_hash.clone().expect("a password was set");
        assert!(hash.starts_with("$argon2id$"), "the hash is Argon2id");
        assert!(
            verify_password(PASSWORD, &hash),
            "and it verifies the submitted password"
        );

        assert_actor_endpoints(&user);
        assert_generated_key_pair(&user);

        let fetched = get_by_nickname(&state.db, "aLiCe")
            .await
            .unwrap()
            .expect("citext finds the row");
        assert_eq!(fetched.id, user.id, "one row, found in any case");
    }

    #[tokio::test]
    async fn register_honours_email_optional_and_requested_name() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config {
            accounts_email_optional: true,
            ..Config::for_tests()
        };
        let state = test_state_with(&pool, config);
        let params = RegisterParams {
            nickname: "noemail".to_owned(),
            email: None,
            password: PASSWORD.to_owned(),
            name: Some("<b>Mr Nobody</b>".to_owned()),
            bio: None,
        };
        let user = create_local_user(&state, params).await.unwrap();
        assert!(user.email.is_none(), "the account has no email");
        assert_eq!(user.name, "Mr Nobody", "the sanitized requested name");
        assert_eq!(user.bio, "", "absent bio is empty HTML");
        assert_eq!(user.raw_bio, "", "absent bio has no source text");
    }

    #[tokio::test]
    async fn register_collects_every_failure_and_inserts_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);

        let bad_shape = RegisterParams {
            nickname: "bad nickname!".to_owned(),
            email: Some("not-an-email".to_owned()),
            password: SHORT_PASSWORD.to_owned(),
            name: None,
            bio: None,
        };
        let shape_err = create_local_user(&state, bad_shape).await.unwrap_err();
        let RegisterError::Invalid(shape_errors) = shape_err else {
            panic!("a validation failure must be Invalid, got {shape_err:?}")
        };
        assert_eq!(
            shape_errors.get("nickname").expect("nickname errors"),
            &vec![HAS_INVALID_FORMAT.to_owned()],
            "the nickname's format error"
        );
        assert_eq!(
            shape_errors.get("email").expect("email errors"),
            &vec![HAS_INVALID_FORMAT.to_owned()],
            "the email's format error"
        );
        assert_eq!(
            shape_errors.get("password").expect("password errors"),
            &vec![at_least(8)],
            "the password's length error"
        );

        let all_blank = RegisterParams {
            nickname: " ".to_owned(),
            email: None,
            password: String::new(),
            name: None,
            bio: None,
        };
        let blank_err = create_local_user(&state, all_blank).await.unwrap_err();
        let RegisterError::Invalid(blank_errors) = blank_err else {
            panic!("blank parameters must be Invalid, got {blank_err:?}")
        };
        let mut expected = FieldErrors::new();
        add_error(&mut expected, "nickname", CANNOT_BE_BLANK);
        add_error(&mut expected, "email", CANNOT_BE_BLANK);
        add_error(&mut expected, "password", CANNOT_BE_BLANK);
        assert_eq!(blank_errors, expected, "all three blanks land in one map");

        assert_eq!(user_count(&state.db).await, 0, "nothing was inserted");
    }

    #[tokio::test]
    async fn duplicates_are_taken_in_any_case() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert_login(&pool, "Alice", "Alice@Example.ORG", None).await;
        let bob = insert_login(&pool, "bob", "bob@example.org", None).await;
        let state = test_state(&pool);
        let dup_params = RegisterParams {
            nickname: "aLiCe".to_owned(),
            email: Some("alice@example.org".to_owned()),
            ..params_for("aLiCe")
        };
        let dup_err = create_local_user(&state, dup_params).await.unwrap_err();
        let RegisterError::Invalid(dup_errors) = dup_err else {
            panic!("a duplicate must be Invalid, got {dup_err:?}")
        };
        assert_eq!(
            dup_errors.get("nickname").expect("nickname errors"),
            &vec![HAS_ALREADY_BEEN_TAKEN.to_owned()],
            "the citext nickname collision is found"
        );
        assert_eq!(
            dup_errors.get("email").expect("email errors"),
            &vec![HAS_ALREADY_BEEN_TAKEN.to_owned()],
            "the citext email collision is found"
        );

        let carol_params = RegisterParams {
            email: Some("BOB@example.org".to_owned()),
            ..params_for("carol")
        };
        let carol_err = create_local_user(&state, carol_params).await.unwrap_err();
        let RegisterError::Invalid(carol_errors) = carol_err else {
            panic!("a duplicate email must be Invalid, got {carol_err:?}")
        };
        assert_eq!(
            carol_errors.keys().copied().collect::<Vec<_>>(),
            vec!["email"],
            "only the email is taken for carol"
        );
        assert_eq!(
            carol_errors.get("email").expect("email errors"),
            &vec![HAS_ALREADY_BEEN_TAKEN.to_owned()],
            "bob's email blocks carol in any case"
        );

        assert_eq!(
            user_count(&pool).await,
            2,
            "only {alice:?} and {bob:?} are there"
        );
    }

    #[tokio::test]
    async fn a_lost_insert_race_maps_to_the_taken_field() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // Steal `alice`'s actor id from under the availability check: the
        // nickname looks free, so every check passes and only the INSERT
        // notices the `users_ap_id_key` conflict.
        let squatter = insert(
            &pool,
            &NewUser {
                ap_id: local_ap_id(TEST_BASE_URL, "alice"),
                ..local_new("mallory")
            },
        )
        .await
        .expect("the squatter inserts");
        assert_eq!(squatter.nickname, "mallory", "the squatter owns ap_id");
        let state = test_state(&pool);
        let race_err = create_local_user(&state, params_for("alice"))
            .await
            .unwrap_err();
        let RegisterError::Invalid(race_errors) = race_err else {
            panic!("the race must land as Invalid, got {race_err:?}")
        };
        assert_eq!(
            race_errors.get("ap_id").expect("ap_id errors"),
            &vec![HAS_ALREADY_BEEN_TAKEN.to_owned()],
            "the 23505 names its field just like the checks do"
        );
        assert_eq!(user_count(&pool).await, 1, "the race inserted nothing");
    }

    #[tokio::test]
    async fn a_other_database_error_stays_a_failure() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // A constraint failure that is *not* a unique violation must not be
        // dressed up as field errors: run a NOT NULL violation through the
        // mapper and expect the database failure to come out intact.
        let raw = sqlx::query("INSERT INTO users (id, ap_id, local) VALUES ($1, $2, true)")
            .bind(new_id())
            .bind("https://test.example/users/nonull")
            .execute(&pool)
            .await
            .expect_err("the nickname is NOT NULL");
        let mapped = map_insert_error(AppError::Database(raw));
        assert!(
            matches!(mapped, RegisterError::Failed(AppError::Database(_))),
            "a non-23505 violation stays a Failed database error, got {mapped:?}"
        );
    }

    #[tokio::test]
    async fn authenticate_accepts_anyone_it_should_and_no_one_else() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hash = hash_password(PASSWORD).expect("the fixture hash is made");
        let alice = insert_login(&pool, "alice", "alice@example.org", Some(&hash)).await;

        let by_nickname = authenticate(&pool, "alice", PASSWORD)
            .await
            .expect("the nickname and password log in");
        assert_eq!(by_nickname.id, alice.id, "the row itself");
        assert!(
            authenticate(&pool, "ALICE", PASSWORD).await.is_some(),
            "the nickname lookup is citext"
        );
        assert!(
            authenticate(&pool, "ALICE@EXAMPLE.ORG", PASSWORD)
                .await
                .is_some(),
            "the email lookup is citext too"
        );
        assert!(
            authenticate(&pool, "alice", "wrong password")
                .await
                .is_none(),
            "the wrong password never logs in"
        );
        assert!(
            authenticate(&pool, "nobody", PASSWORD).await.is_none(),
            "an unknown identifier never logs in (dummy-hash path)"
        );
        assert!(
            authenticate(&pool, "", PASSWORD).await.is_none(),
            "an empty identifier never logs in"
        );

        let carol = insert_login(&pool, "carol", "carol@example.org", None).await;
        assert!(
            carol.password_hash.is_none(),
            "the hashless fixture exists to be tested"
        );
        assert!(
            authenticate(&pool, "carol", PASSWORD).await.is_none(),
            "an account without a hash cannot log in by password"
        );

        set_active(&pool, alice.id, false).await;
        assert!(
            authenticate(&pool, "alice", PASSWORD).await.is_none(),
            "a deactivated account never logs in"
        );
        set_active(&pool, alice.id, true).await;
        assert!(
            authenticate(&pool, "alice", PASSWORD).await.is_some(),
            "reactivation lets it in again"
        );

        // A remote mirror with a hash is no candidate either: local-only.
        let bob = insert(
            &pool,
            &NewUser {
                nickname: "bob@remote.example".to_owned(),
                ap_id: "https://remote.example/users/bob".to_owned(),
                email: Some("bob@remote.example".to_owned()),
                password_hash: Some(hash.clone()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts");
        assert!(!bob.local.get(), "the fixture is a remote mirror");
        assert!(
            authenticate(&pool, "bob@remote.example", PASSWORD)
                .await
                .is_none(),
            "remote users never authenticate by password"
        );
    }

    #[tokio::test]
    async fn authenticate_any_status_returns_a_deactivated_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hash = hash_password(PASSWORD).expect("the fixture hash is made");
        let alice = insert_login(&pool, "henry", "henry@example.org", Some(&hash)).await;
        set_active(&pool, alice.id, false).await;
        let found = authenticate_any_status(&pool, "henry", PASSWORD)
            .await
            .expect("the password of a deactivated account still verifies");
        assert_eq!(found.id, alice.id, "the row itself comes back");
        assert!(
            !found.is_active.get(),
            "and it still reports the account as inactive"
        );
        assert!(
            authenticate_any_status(&pool, "henry", "wrong password")
                .await
                .is_none(),
            "but the wrong password never logs in either"
        );
        assert!(
            authenticate_any_status(&pool, "nobody-here", PASSWORD)
                .await
                .is_none(),
            "and an unknown identifier finds nothing (dummy-hash path)"
        );
    }

    #[tokio::test]
    async fn authenticate_upgrades_a_legacy_bcrypt_hash() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = insert_login(&pool, "dave", "dave@example.org", Some(BCRYPT_2B)).await;
        assert!(needs_rehash(BCRYPT_2B), "the fixture is a legacy hash");
        let logged_in = authenticate(&pool, "dave", PASSWORD)
            .await
            .expect("the legacy hash logs in");
        assert!(
            !needs_rehash(
                logged_in
                    .password_hash
                    .as_deref()
                    .expect("the returned user carries the upgrade")
            ),
            "the returned row reports the current hash"
        );

        let stored = get_by_id(&pool, user.id)
            .await
            .unwrap()
            .expect("the user exists");
        let new_hash = stored.password_hash.expect("the row was upgraded");
        assert_ne!(new_hash, BCRYPT_2B, "the legacy hash is gone");
        assert!(
            new_hash.starts_with("$argon2id$"),
            "the upgrade is Argon2id"
        );
        assert!(
            verify_password(PASSWORD, &new_hash),
            "the upgraded hash verifies the same password"
        );
        assert!(
            authenticate(&pool, "dave", PASSWORD).await.is_some(),
            "logging in still works afterwards"
        );
        assert!(
            authenticate(&pool, "dave", "wrong password")
                .await
                .is_none(),
            "and still rejects wrong passwords"
        );
    }

    #[tokio::test]
    async fn authenticate_leaves_a_current_hash_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hash = hash_password(PASSWORD).expect("the fixture hash is made");
        let user = insert_login(&pool, "erin", "erin@example.org", Some(&hash)).await;
        let logged_in = authenticate(&pool, "erin", PASSWORD)
            .await
            .expect("the current hash logs in");
        assert_eq!(
            logged_in.password_hash.as_deref(),
            Some(hash.as_str()),
            "an Argon2id hash is not rehashed"
        );
        let stored = get_by_id(&pool, user.id)
            .await
            .unwrap()
            .expect("the user exists");
        assert_eq!(
            stored.password_hash.as_deref(),
            Some(hash.as_str()),
            "and the row keeps it verbatim"
        );
    }

    #[tokio::test]
    async fn authenticate_answers_none_when_the_database_breaks() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hash = hash_password(PASSWORD).expect("the fixture hash is made");
        let frank = insert_login(&pool, "frank", "frank@example.org", Some(&hash)).await;
        assert_eq!(frank.nickname, "frank", "the fixture exists");
        // CASCADE because the OAuth token and authorization tables now
        // hold foreign keys to `users`; the point of the exercise is the
        // same — no readable `users` table for the lookup to hit.
        let dropped = sqlx::query("DROP TABLE users CASCADE")
            .execute(&pool)
            .await
            .expect("the table can be dropped in a throwaway database");
        assert_eq!(dropped.rows_affected(), 0, "DROP reports no rows");
        assert!(
            authenticate(&pool, "frank", PASSWORD).await.is_none(),
            "a broken lookup is not a login"
        );
    }
}
