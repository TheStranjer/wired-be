//! The `users` table: one table for local accounts and remote actors.
//!
//! Per `docs/ARCHITECTURE.md` decision 7 a local user (`alice`) and a
//! remote mirror (`alice@remote.example`) share the table, told apart by
//! the `local` flag. [`NewUser::default`] mirrors the column defaults, and
//! [`NewUser::local`] is the usual starting point for a local account. The
//! reference shape is Pleroma's `User` schema (`lib/pleroma/user.ex`); the
//! ticket columns are the authoritative subset.
//!
//! Denormalised counters move only through [`update_counts`], which is
//! atomic in SQL and clamps every count at zero. The URL of a local actor
//! and its endpoints come from [`local_ap_id`] and the helpers beside it,
//! so federation and the API build them the same way.
//!
//! `password_hash` and `private_key` are secrets: the manual `Debug` impls
//! in this module redact them, so they can never reach a log line.
//!
//! Registration and password login live in [`register`].

use std::fmt;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppResult, util::id::new_id};

pub mod register;

/// `actor_type` a [`NewUser`] starts with: the `ActivityPub` default actor
/// kind, matching the column default.
pub const DEFAULT_ACTOR_TYPE: &str = "Person";

/// A Postgres `boolean` column as a named flag.
///
/// `User` and `NewUser` carry seven booleans each; Clippy's
/// `struct_excessive_bools` (denied, threshold 2) wants them modelled, not
/// piled up, so every boolean column is a `Flag`. The `sqlx` transparent
/// derive makes it encode, decode and compare exactly like the inner
/// `bool`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct Flag(bool);

impl Flag {
    /// Wraps a raw boolean as a flag.
    #[must_use]
    pub const fn new(value: bool) -> Self {
        Self(value)
    }

    /// The raw boolean behind the flag.
    #[must_use]
    pub const fn get(self) -> bool {
        self.0
    }
}

/// A `bool` is already a valid flag value; `true.into()` sets one.
impl From<bool> for Flag {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

/// One row of `users`: a local account or a mirror of a remote actor
/// (decision 7).
#[derive(sqlx::FromRow)]
pub struct User {
    /// `UUIDv7` primary key, minted by [`insert`] (decision 4).
    pub id: Uuid,
    /// Handle: `alice` locally, `alice@remote.example` for remote users.
    /// Case-insensitive (citext), unique.
    pub nickname: String,
    /// `ActivityPub` actor id, unique.
    pub ap_id: String,
    /// Whether this instance owns the account.
    pub local: Flag,
    /// Host of a remote actor; `None` for local users.
    pub domain: Option<String>,
    /// `ActivityPub` actor type (`Person`, `Service`, ...).
    pub actor_type: String,
    /// Account email, local users only; case-insensitive, unique.
    pub email: Option<String>,
    /// PHC password hash, local users only. Never in `Debug` output.
    pub password_hash: Option<String>,
    /// Display name; empty means "same as the nickname".
    pub name: String,
    /// Profile note as sanitized HTML.
    pub bio: String,
    /// Profile note as its source text.
    pub raw_bio: String,
    /// Avatar URL, if set.
    pub avatar_url: Option<String>,
    /// Banner URL, if set.
    pub banner_url: Option<String>,
    /// Human-facing profile URL (the `ActivityPub` `url`), if set.
    pub profile_url: Option<String>,
    /// Whether follow requests need manual approval.
    pub locked: Flag,
    /// Whether the account is flagged as automated.
    pub bot: Flag,
    /// Whether the account opted into discovery.
    pub discoverable: Flag,
    /// Profile metadata: JSON array of `{name, value}` objects.
    pub fields: Value,
    /// `ActivityPub` inbox endpoint, if known.
    pub inbox: Option<String>,
    /// Shared inbox endpoint, if known.
    pub shared_inbox: Option<String>,
    /// `ActivityPub` outbox collection id, if known.
    pub outbox: Option<String>,
    /// Followers collection id, if known.
    pub followers_url: Option<String>,
    /// Following collection id, if known.
    pub following_url: Option<String>,
    /// Featured (pinned) collection id, if known.
    pub featured_url: Option<String>,
    /// Actor public signing key (PEM), if known.
    pub public_key: Option<String>,
    /// Actor private signing key; local users only. Never in `Debug`
    /// output.
    pub private_key: Option<String>,
    /// Whether the account has the admin role.
    pub is_admin: Flag,
    /// Whether the account has the moderator role.
    pub is_moderator: Flag,
    /// Whether the account is active (not deactivated or suspended).
    pub is_active: Flag,
    /// Number of followers; moved by [`update_counts`].
    pub followers_count: i32,
    /// Number of follows; moved by [`update_counts`].
    pub following_count: i32,
    /// Number of posted statuses; moved by [`update_counts`].
    pub statuses_count: i32,
    /// Insert time of the newest status this user posted.
    pub last_status_at: Option<DateTime<Utc>>,
    /// When a remote actor was last fetched.
    pub last_fetched_at: Option<DateTime<Utc>>,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every update here.
    pub updated_at: DateTime<Utc>,
}

/// [`password_hash`](Self::password_hash) and [`private_key`](Self::private_key)
/// are secrets and render as `<redacted>`; everything else shows.
impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("User")
            .field("id", &self.id)
            .field("nickname", &self.nickname)
            .field("ap_id", &self.ap_id)
            .field("local", &self.local)
            .field("domain", &self.domain)
            .field("actor_type", &self.actor_type)
            .field("email", &self.email)
            .field("password_hash", &"<redacted>")
            .field("name", &self.name)
            .field("bio", &self.bio)
            .field("raw_bio", &self.raw_bio)
            .field("avatar_url", &self.avatar_url)
            .field("banner_url", &self.banner_url)
            .field("profile_url", &self.profile_url)
            .field("locked", &self.locked)
            .field("bot", &self.bot)
            .field("discoverable", &self.discoverable)
            .field("fields", &self.fields)
            .field("inbox", &self.inbox)
            .field("shared_inbox", &self.shared_inbox)
            .field("outbox", &self.outbox)
            .field("followers_url", &self.followers_url)
            .field("following_url", &self.following_url)
            .field("featured_url", &self.featured_url)
            .field("public_key", &self.public_key)
            .field("private_key", &"<redacted>")
            .field("is_admin", &self.is_admin)
            .field("is_moderator", &self.is_moderator)
            .field("is_active", &self.is_active)
            .field("followers_count", &self.followers_count)
            .field("following_count", &self.following_count)
            .field("statuses_count", &self.statuses_count)
            .field("last_status_at", &self.last_status_at)
            .field("last_fetched_at", &self.last_fetched_at)
            .field("inserted_at", &self.inserted_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// What to insert: every `users` column except the ones [`insert`] mints
/// itself (`id`) or the database stamps (`inserted_at`, `updated_at`).
///
/// [`NewUser::default`] gives the column defaults;
/// [`NewUser::local`] is the usual constructor for a local account.
#[derive(Clone)]
pub struct NewUser {
    /// Handle to store (case folds happen in citext).
    pub nickname: String,
    /// `ActivityPub` actor id.
    pub ap_id: String,
    /// Whether the account is owned by this instance.
    pub local: Flag,
    /// Host of the remote actor; `None` for local users.
    pub domain: Option<String>,
    /// `ActivityPub` actor type; [`DEFAULT_ACTOR_TYPE`] by default.
    pub actor_type: String,
    /// Account email, for local users.
    pub email: Option<String>,
    /// PHC password hash. Never in `Debug` output.
    pub password_hash: Option<String>,
    /// Display name; empty by default.
    pub name: String,
    /// Profile note as sanitized HTML; empty by default.
    pub bio: String,
    /// Profile note as its source text; empty by default.
    pub raw_bio: String,
    /// Avatar URL.
    pub avatar_url: Option<String>,
    /// Banner URL.
    pub banner_url: Option<String>,
    /// Human-facing profile URL (the `ActivityPub` `url`).
    pub profile_url: Option<String>,
    /// Whether follows require approval; `false` by default.
    pub locked: Flag,
    /// Whether the account is automated; `false` by default.
    pub bot: Flag,
    /// Whether the account opted into discovery; `false` by default.
    pub discoverable: Flag,
    /// Profile metadata; the empty JSON array by default.
    pub fields: Value,
    /// `ActivityPub` inbox endpoint.
    pub inbox: Option<String>,
    /// Shared inbox endpoint.
    pub shared_inbox: Option<String>,
    /// `ActivityPub` outbox collection id.
    pub outbox: Option<String>,
    /// Followers collection id.
    pub followers_url: Option<String>,
    /// Following collection id.
    pub following_url: Option<String>,
    /// Featured (pinned) collection id.
    pub featured_url: Option<String>,
    /// Actor public signing key (PEM).
    pub public_key: Option<String>,
    /// Actor private signing key, local users only. Never in `Debug`
    /// output.
    pub private_key: Option<String>,
    /// Whether the account is an admin; `false` by default.
    pub is_admin: Flag,
    /// Whether the account is a moderator; `false` by default.
    pub is_moderator: Flag,
    /// Whether the account is active; `true` by default.
    pub is_active: Flag,
    /// Initial follower count; `0` by default.
    pub followers_count: i32,
    /// Initial following count; `0` by default.
    pub following_count: i32,
    /// Initial status count; `0` by default.
    pub statuses_count: i32,
    /// Initial newest-status time; `None` by default.
    pub last_status_at: Option<DateTime<Utc>>,
    /// Initial last-fetch time; `None` by default.
    pub last_fetched_at: Option<DateTime<Utc>>,
}

/// [`password_hash`](Self::password_hash) and [`private_key`](Self::private_key)
/// are secrets and render as `<redacted>`; everything else shows.
impl fmt::Debug for NewUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewUser")
            .field("nickname", &self.nickname)
            .field("ap_id", &self.ap_id)
            .field("local", &self.local)
            .field("domain", &self.domain)
            .field("actor_type", &self.actor_type)
            .field("email", &self.email)
            .field("password_hash", &"<redacted>")
            .field("name", &self.name)
            .field("bio", &self.bio)
            .field("raw_bio", &self.raw_bio)
            .field("avatar_url", &self.avatar_url)
            .field("banner_url", &self.banner_url)
            .field("profile_url", &self.profile_url)
            .field("locked", &self.locked)
            .field("bot", &self.bot)
            .field("discoverable", &self.discoverable)
            .field("fields", &self.fields)
            .field("inbox", &self.inbox)
            .field("shared_inbox", &self.shared_inbox)
            .field("outbox", &self.outbox)
            .field("followers_url", &self.followers_url)
            .field("following_url", &self.following_url)
            .field("featured_url", &self.featured_url)
            .field("public_key", &self.public_key)
            .field("private_key", &"<redacted>")
            .field("is_admin", &self.is_admin)
            .field("is_moderator", &self.is_moderator)
            .field("is_active", &self.is_active)
            .field("followers_count", &self.followers_count)
            .field("following_count", &self.following_count)
            .field("statuses_count", &self.statuses_count)
            .field("last_status_at", &self.last_status_at)
            .field("last_fetched_at", &self.last_fetched_at)
            .finish()
    }
}

/// The column defaults of `users`: a remote `Person` with no contact
/// details, zero counts and nothing disabled.
impl Default for NewUser {
    fn default() -> Self {
        Self {
            nickname: String::new(),
            ap_id: String::new(),
            local: Flag::new(false),
            domain: None,
            actor_type: DEFAULT_ACTOR_TYPE.to_owned(),
            email: None,
            password_hash: None,
            name: String::new(),
            bio: String::new(),
            raw_bio: String::new(),
            avatar_url: None,
            banner_url: None,
            profile_url: None,
            locked: Flag::new(false),
            bot: Flag::new(false),
            discoverable: Flag::new(false),
            fields: json!([]),
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            is_admin: Flag::new(false),
            is_moderator: Flag::new(false),
            is_active: Flag::new(true),
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
        }
    }
}

impl NewUser {
    /// A new local account named `nickname` with actor id `ap_id`: the
    /// column defaults of [`NewUser::default`] plus `local = true`. Pair
    /// it with [`local_ap_id`] to build `ap_id`.
    #[must_use]
    pub fn local(nickname: &str, ap_id: &str) -> Self {
        Self {
            nickname: nickname.to_owned(),
            ap_id: ap_id.to_owned(),
            local: Flag::new(true),
            ..Self::default()
        }
    }
}

/// One step to apply to the denormalised counters of a user atomically.
///
/// Each field is added to its column by [`update_counts`]; negatives
/// shrink the count, and the result is clamped at zero.
#[derive(Debug, Default, Clone, Copy)]
pub struct CountDelta {
    /// Step for `followers_count`.
    pub followers: i32,
    /// Step for `following_count`.
    pub following: i32,
    /// Step for `statuses_count`.
    pub statuses: i32,
}

/// The `INSERT` behind [`insert`]: every column is written from a
/// [`NewUser`] (the id is the extra bind), with `RETURNING *` so the row
/// comes back with its database-side timestamps.
const INSERT_SQL: &str = "INSERT INTO users (
     id, nickname, ap_id, local, domain, actor_type, email, password_hash,
     name, bio, raw_bio, avatar_url, banner_url, profile_url, locked, bot,
     discoverable, fields, inbox, shared_inbox, outbox, followers_url,
     following_url, featured_url, public_key, private_key, is_admin,
     is_moderator, is_active, followers_count, following_count,
     statuses_count, last_status_at, last_fetched_at
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
     $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28,
     $29, $30, $31, $32, $33, $34
) RETURNING *";

/// Inserts `new_user` and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4); every other column comes from
/// `new_user`, whose [`Default`](NewUser::default) mirrors the column
/// defaults. `nickname`, `ap_id` and `email` are unique, and citext folds
/// case in the nickname and email comparisons, so a duplicate in any case
/// is rejected.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); a unique violation
/// (SQLSTATE `23505`) is reported through it unchanged.
pub async fn insert(pool: &PgPool, new_user: &NewUser) -> AppResult<User> {
    let user = sqlx::query_as::<_, User>(INSERT_SQL)
        .bind(new_id())
        .bind(new_user.nickname.as_str())
        .bind(new_user.ap_id.as_str())
        .bind(new_user.local)
        .bind(new_user.domain.as_deref())
        .bind(new_user.actor_type.as_str())
        .bind(new_user.email.as_deref())
        .bind(new_user.password_hash.as_deref())
        .bind(new_user.name.as_str())
        .bind(new_user.bio.as_str())
        .bind(new_user.raw_bio.as_str())
        .bind(new_user.avatar_url.as_deref())
        .bind(new_user.banner_url.as_deref())
        .bind(new_user.profile_url.as_deref())
        .bind(new_user.locked)
        .bind(new_user.bot)
        .bind(new_user.discoverable)
        .bind(new_user.fields.clone())
        .bind(new_user.inbox.as_deref())
        .bind(new_user.shared_inbox.as_deref())
        .bind(new_user.outbox.as_deref())
        .bind(new_user.followers_url.as_deref())
        .bind(new_user.following_url.as_deref())
        .bind(new_user.featured_url.as_deref())
        .bind(new_user.public_key.as_deref())
        .bind(new_user.private_key.as_deref())
        .bind(new_user.is_admin)
        .bind(new_user.is_moderator)
        .bind(new_user.is_active)
        .bind(new_user.followers_count)
        .bind(new_user.following_count)
        .bind(new_user.statuses_count)
        .bind(new_user.last_status_at)
        .bind(new_user.last_fetched_at)
        .fetch_one(pool)
        .await?;
    Ok(user)
}

/// Fetches the user whose primary key is `id`, `None` when no row matches.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

/// Fetches the user with nickname `nickname`, `None` when absent.
///
/// The comparison is case-insensitive because the column is citext, so
/// `ALICE` finds `alice`; the bind is cast to citext because a bare text
/// parameter would resolve to Postgres' case-sensitive `text = text`
/// operator.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_nickname(pool: &PgPool, nickname: &str) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE nickname = $1::citext")
        .bind(nickname)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

/// Fetches the user with `ActivityPub` actor id `ap_id`, `None` when absent.
/// The comparison is exact (plain `text`).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE ap_id = $1")
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

/// Fetches the local user whose email is `email`, `None` when absent.
///
/// The comparison is case-insensitive because the column is citext (the
/// bind is cast, as in [`get_by_nickname`]); remote users, who have no
/// email, are never found.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_email(pool: &PgPool, email: &str) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = $1::citext")
        .bind(email)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

/// Fetches the users whose ids are in `ids`, in `id` order (`UUIDv7` text
/// sorts by creation, decision 4). Ids with no row are skipped; an empty
/// `ids` yields an empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_many(pool: &PgPool, ids: &[Uuid]) -> AppResult<Vec<User>> {
    let ids = ids.to_vec();
    let users = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ANY($1) ORDER BY id")
        .bind(&ids)
        .fetch_all(pool)
        .await?;
    Ok(users)
}

/// Adds `delta` to the denormalised counters of user `id` atomically,
/// clamping every count at zero, and refreshes `updated_at`.
///
/// The arithmetic happens in the `UPDATE` statement itself, so two
/// concurrent updates to the same row both take effect and the stored
/// counts are never negative. An unknown `id` matches no row and changes
/// nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update_counts(pool: &PgPool, id: Uuid, delta: CountDelta) -> AppResult<()> {
    sqlx::query(
        "UPDATE users
            SET followers_count = GREATEST(0, followers_count + $2),
                following_count = GREATEST(0, following_count + $3),
                statuses_count  = GREATEST(0, statuses_count + $4),
                updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(delta.followers)
    .bind(delta.following)
    .bind(delta.statuses)
    .execute(pool)
    .await
    .map(drop)?;
    Ok(())
}

/// The `ActivityPub` actor id of the local account `nickname` on the
/// instance served at `base_url`: `{base_url}/users/{nickname}`.
#[must_use]
pub fn local_ap_id(base_url: &str, nickname: &str) -> String {
    format!("{base_url}/users/{nickname}")
}

/// The inbox endpoint of actor `ap_id`: `{ap_id}/inbox`.
#[must_use]
pub fn inbox(ap_id: &str) -> String {
    format!("{ap_id}/inbox")
}

/// The outbox collection of actor `ap_id`: `{ap_id}/outbox`.
#[must_use]
pub fn outbox(ap_id: &str) -> String {
    format!("{ap_id}/outbox")
}

/// The followers collection of actor `ap_id`: `{ap_id}/followers`.
#[must_use]
pub fn followers(ap_id: &str) -> String {
    format!("{ap_id}/followers")
}

/// The following collection of actor `ap_id`: `{ap_id}/following`.
#[must_use]
pub fn following(ap_id: &str) -> String {
    format!("{ap_id}/following")
}

/// The featured (pinned) collection of actor `ap_id`: `{ap_id}/featured`.
#[must_use]
pub fn featured(ap_id: &str) -> String {
    format!("{ap_id}/featured")
}

#[cfg(test)]
mod tests {
    use chrono::SubsecRound as _;
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::error::AppError;

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = "https://test.example";
    /// A password hash that must never surface in `Debug` output.
    const SECRET_HASH: &str = "$argon2id$v=19$secret-digest";
    /// Private key material that must never surface in `Debug` output.
    const SECRET_KEY: &str = "BEGIN RSA PRIVATE KEY secret END";

    /// A local [`NewUser`] named `nickname` at this module's [`BASE`] url.
    fn local_new(nickname: &str) -> NewUser {
        NewUser::local(nickname, &local_ap_id(BASE, nickname))
    }

    /// Reads the row behind `id` back out of the table, panicking if absent.
    async fn fetch(pool: &PgPool, id: Uuid) -> User {
        get_by_id(pool, id).await.unwrap().expect("row exists")
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn insert_round_trips_every_column(pool: PgPool) {
        let fetched_at = Utc::now().trunc_subsecs(6);
        let new = NewUser {
            email: Some("alice@example.org".to_owned()),
            password_hash: Some(SECRET_HASH.to_owned()),
            name: "Alice".to_owned(),
            bio: "<p>hi</p>".to_owned(),
            raw_bio: "hi".to_owned(),
            avatar_url: Some(format!("{BASE}/avatar.png")),
            banner_url: Some(format!("{BASE}/banner.png")),
            profile_url: Some(local_ap_id(BASE, "alice")),
            fields: json!([{"name": "site", "value": "https://alice.example"}]),
            inbox: Some(inbox(&local_ap_id(BASE, "alice"))),
            public_key: Some("PUBLIC-PEM".to_owned()),
            private_key: Some(SECRET_KEY.to_owned()),
            last_fetched_at: Some(fetched_at),
            ..local_new("alice")
        };
        let user = insert(&pool, &new).await.unwrap();
        assert!(user.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(user.nickname, "alice", "the nickname round trips");
        assert_eq!(user.ap_id, format!("{BASE}/users/alice"), "ap_id");
        assert!(user.local.get(), "the local flag round trips");
        assert!(user.domain.is_none(), "local users carry no domain");
        assert_eq!(user.actor_type, DEFAULT_ACTOR_TYPE, "constructor default");
        assert_eq!(
            user.email.as_deref(),
            Some("alice@example.org"),
            "email round trips"
        );
        assert_eq!(
            user.password_hash.as_deref(),
            Some(SECRET_HASH),
            "hash is stored even though Debug redacts it"
        );
        assert_eq!(user.name, "Alice", "display name round trips");
        assert_eq!(user.bio, "<p>hi</p>", "sanitized note round trips");
        assert_eq!(user.raw_bio, "hi", "source note round trips");
        assert_eq!(
            user.avatar_url.as_deref(),
            Some("https://test.example/avatar.png"),
            "avatar url"
        );
        assert_eq!(
            user.banner_url.as_deref(),
            Some("https://test.example/banner.png"),
            "banner url"
        );
        assert_eq!(
            user.profile_url.as_deref(),
            Some("https://test.example/users/alice"),
            "profile url"
        );
        assert!(
            !user.locked.get() && !user.bot.get() && !user.discoverable.get(),
            "the three option flags default to false"
        );
        assert_eq!(
            user.fields,
            json!([{"name": "site", "value": "https://alice.example"}]),
            "fields round trip through jsonb"
        );
        assert_eq!(
            user.inbox.as_deref(),
            Some("https://test.example/users/alice/inbox"),
            "inbox column"
        );
        assert!(user.shared_inbox.is_none(), "unset endpoints stay NULL");
        assert_eq!(user.public_key.as_deref(), Some("PUBLIC-PEM"), "public key");
        assert_eq!(
            user.private_key.as_deref(),
            Some(SECRET_KEY),
            "private key is stored even though Debug redacts it"
        );
        assert!(
            !user.is_admin.get() && !user.is_moderator.get(),
            "roles default to false"
        );
        assert!(user.is_active.get(), "a fresh account is active");
        assert_eq!(
            (
                user.followers_count,
                user.following_count,
                user.statuses_count
            ),
            (0, 0, 0),
            "counts default to zero"
        );
        assert!(user.last_status_at.is_none(), "no statuses yet");
        assert_eq!(
            user.last_fetched_at,
            Some(fetched_at),
            "timestamptz round trips"
        );
        assert!(
            user.inserted_at <= user.updated_at,
            "the database stamps both timestamps"
        );

        let by_id = get_by_id(&pool, user.id).await.unwrap().expect("by id");
        let by_nickname = get_by_nickname(&pool, "ALiCE")
            .await
            .unwrap()
            .expect("citext nickname lookup folds case");
        let by_ap_id = get_by_ap_id(&pool, &user.ap_id)
            .await
            .unwrap()
            .expect("by ap_id");
        let by_email = get_by_email(&pool, "ALICE@example.org")
            .await
            .unwrap()
            .expect("citext email lookup folds case");
        assert_eq!(by_nickname.id, user.id, "the nickname finds the row");
        assert_eq!(by_ap_id.id, user.id, "the actor id finds the row");
        assert_eq!(by_email.id, user.id, "the email finds the row");
        assert_eq!(
            by_id.nickname, by_nickname.nickname,
            "every path reads the same row"
        );

        let many = get_many(&pool, &[user.id, new_id()]).await.unwrap();
        assert_eq!(many.len(), 1, "unknown ids are simply absent");
        assert_eq!(
            many.first().map(|found| found.id),
            Some(user.id),
            "the known id comes back"
        );
        assert!(
            get_many(&pool, &[]).await.unwrap().is_empty(),
            "no ids, no rows"
        );
        assert!(
            get_by_ap_id(&pool, "https://nowhere.example/users/x")
                .await
                .unwrap()
                .is_none(),
            "an unknown ap_id is None"
        );
        assert!(
            get_by_email(&pool, "nobody@example.org")
                .await
                .unwrap()
                .is_none(),
            "an unknown email is None"
        );
        assert!(
            get_by_nickname(&pool, "nobody").await.unwrap().is_none(),
            "an unknown nickname is None"
        );
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn duplicate_nickname_in_other_case_is_rejected(pool: PgPool) {
        let alice = insert(&pool, &local_new("Alice")).await.unwrap();
        let err = insert(&pool, &local_new("aLiCe")).await.unwrap_err();
        let AppError::Database(SqlxError::Database(db_err)) = err else {
            panic!("a nickname collision must surface as a database error")
        };
        assert_eq!(
            db_err.code().as_deref(),
            Some("23505"),
            "the duplicate is the citext unique constraint, not some other failure"
        );

        let rows = get_many(&pool, &[alice.id]).await.unwrap();
        assert_eq!(rows.len(), 1, "the rejected insert stored nothing");
        let stored = get_by_nickname(&pool, "aLiCe")
            .await
            .unwrap()
            .expect("citext lookup folds case");
        assert_eq!(stored.id, alice.id, "the first spelling owns the row");
        assert_eq!(stored.nickname, "Alice", "the stored case survives citext");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn remote_user_nickname_carries_the_host(pool: PgPool) {
        let ap_id = "https://remote.example/users/bob";
        let new = NewUser {
            nickname: "bob@remote.example".to_owned(),
            ap_id: ap_id.to_owned(),
            domain: Some("remote.example".to_owned()),
            name: "Bob".to_owned(),
            inbox: Some(inbox(ap_id)),
            shared_inbox: Some("https://remote.example/inbox".to_owned()),
            outbox: Some(outbox(ap_id)),
            followers_url: Some(followers(ap_id)),
            following_url: Some(following(ap_id)),
            featured_url: Some(featured(ap_id)),
            public_key: Some("REMOTE-PEM".to_owned()),
            discoverable: Flag::new(true),
            followers_count: 12,
            following_count: 3,
            statuses_count: 48,
            last_fetched_at: Some(Utc::now()),
            ..NewUser::default()
        };
        let user = insert(&pool, &new).await.unwrap();
        assert!(!user.local.get(), "the default NewUser is a remote mirror");
        assert_eq!(
            user.nickname, "bob@remote.example",
            "the user@host handle round trips"
        );
        assert_eq!(user.domain.as_deref(), Some("remote.example"), "domain");
        assert_eq!(user.inbox.as_deref(), Some(inbox(ap_id).as_str()), "inbox");
        assert_eq!(
            user.shared_inbox.as_deref(),
            Some("https://remote.example/inbox"),
            "shared inbox"
        );
        assert_eq!(
            user.outbox.as_deref(),
            Some(outbox(ap_id).as_str()),
            "outbox"
        );
        assert_eq!(
            user.followers_url.as_deref(),
            Some(followers(ap_id).as_str()),
            "followers collection"
        );
        assert_eq!(
            user.following_url.as_deref(),
            Some(following(ap_id).as_str()),
            "following collection"
        );
        assert_eq!(
            user.featured_url.as_deref(),
            Some(featured(ap_id).as_str()),
            "featured collection"
        );
        assert!(
            user.private_key.is_none(),
            "remote users keep no private key"
        );
        assert!(user.email.is_none(), "remote users keep no email");
        assert!(user.discoverable.get(), "flags round trip");
        assert_eq!(
            (
                user.followers_count,
                user.following_count,
                user.statuses_count
            ),
            (12, 3, 48),
            "fetched counts are stored as given"
        );

        let fetched = get_by_nickname(&pool, "bob@remote.example")
            .await
            .unwrap()
            .expect("the remote handle finds the row");
        assert_eq!(fetched.ap_id, ap_id, "same row by nickname");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn update_counts_adds_atomically_and_clamps_at_zero(pool: PgPool) {
        let user = insert(&pool, &local_new("dave")).await.unwrap();
        update_counts(
            &pool,
            user.id,
            CountDelta {
                followers: 4,
                following: 2,
                statuses: 7,
            },
        )
        .await
        .unwrap();

        // Two concurrent +1s must both land: the arithmetic is in the
        // UPDATE itself, not a read-modify-write.
        let plus_one = CountDelta {
            followers: 1,
            following: 0,
            statuses: 0,
        };
        let (left, right) = tokio::join!(
            update_counts(&pool, user.id, plus_one),
            update_counts(&pool, user.id, plus_one)
        );
        left.unwrap();
        right.unwrap();

        let after = fetch(&pool, user.id).await;
        assert_eq!(after.followers_count, 6, "both concurrent steps landed");
        assert_eq!(after.following_count, 2, "the untouched step stays");
        assert_eq!(after.statuses_count, 7, "the untouched step stays");

        update_counts(
            &pool,
            user.id,
            CountDelta {
                followers: -100,
                following: -100,
                statuses: -100,
            },
        )
        .await
        .unwrap();
        let clamped = fetch(&pool, user.id).await;
        assert_eq!(
            (
                clamped.followers_count,
                clamped.following_count,
                clamped.statuses_count
            ),
            (0, 0, 0),
            "counts never go negative"
        );

        // An unknown id is a no-op, not an error.
        update_counts(&pool, new_id(), CountDelta::default())
            .await
            .unwrap();
        let still = fetch(&pool, user.id).await;
        assert_eq!(still.followers_count, 0, "a no-op update changed nothing");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn debug_redacts_secrets_in_user_and_new_user(pool: PgPool) {
        let user = insert(
            &pool,
            &NewUser {
                password_hash: Some(SECRET_HASH.to_owned()),
                private_key: Some(SECRET_KEY.to_owned()),
                ..local_new("erin")
            },
        )
        .await
        .unwrap();
        let debug = format!("{user:?}");
        assert!(
            !debug.contains(SECRET_HASH),
            "the password hash must not leak"
        );
        assert!(!debug.contains(SECRET_KEY), "the private key must not leak");
        assert!(
            debug.contains(r#"password_hash: "<redacted>""#),
            "the redaction marker names the field"
        );
        assert!(
            debug.contains(r#"private_key: "<redacted>""#),
            "the redaction marker names the field"
        );
        assert!(debug.contains("erin"), "the rest of the row is visible");

        let new = NewUser {
            password_hash: Some(SECRET_HASH.to_owned()),
            private_key: Some(SECRET_KEY.to_owned()),
            ..local_new("erin")
        };
        let new_debug = format!("{new:?}");
        assert!(
            !new_debug.contains(SECRET_HASH) && !new_debug.contains(SECRET_KEY),
            "NewUser redacts the same two fields"
        );
        assert!(
            new_debug.contains(r#"password_hash: "<redacted>""#),
            "and shows the markers"
        );
    }

    #[test]
    fn url_helpers_append_the_activitypub_paths() {
        let alice = local_ap_id(BASE, "alice");
        assert_eq!(
            alice, "https://test.example/users/alice",
            "local ap_id path"
        );
        assert_eq!(
            inbox(&alice),
            "https://test.example/users/alice/inbox",
            "inbox"
        );
        assert_eq!(
            outbox(&alice),
            "https://test.example/users/alice/outbox",
            "outbox"
        );
        assert_eq!(
            followers(&alice),
            "https://test.example/users/alice/followers",
            "followers"
        );
        assert_eq!(
            following(&alice),
            "https://test.example/users/alice/following",
            "following"
        );
        assert_eq!(
            featured(&alice),
            "https://test.example/users/alice/featured",
            "featured"
        );
        assert_eq!(
            inbox("https://remote.example/users/bob"),
            "https://remote.example/users/bob/inbox",
            "works on remote ids too"
        );
    }

    #[test]
    fn local_constructor_uses_the_column_defaults() {
        let new = local_new("alice");
        assert!(new.local.get(), "the constructor marks the account local");
        assert_eq!(new.nickname, "alice", "nickname");
        assert_eq!(new.ap_id, "https://test.example/users/alice", "ap_id");
        assert!(new.domain.is_none(), "no domain for a local account");
        assert_eq!(new.actor_type, DEFAULT_ACTOR_TYPE, "default actor type");
        assert!(
            new.email.is_none() && new.password_hash.is_none(),
            "no contact details by default"
        );
        assert_eq!(new.name, "", "empty display name");
        assert_eq!(new.bio, "", "empty note");
        assert_eq!(new.raw_bio, "", "empty source note");
        assert_eq!(new.fields, json!([]), "empty field array");
        assert!(
            !new.locked.get() && !new.bot.get() && !new.discoverable.get(),
            "profile flags default to false"
        );
        assert!(
            !new.is_admin.get() && !new.is_moderator.get(),
            "roles default to false"
        );
        assert!(new.is_active.get(), "a new account is active");
        assert!(new.private_key.is_none(), "keys are opt-in");
        assert_eq!(
            (new.followers_count, new.following_count, new.statuses_count),
            (0, 0, 0),
            "counts default to zero"
        );
    }
}
