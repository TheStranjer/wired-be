//! The single-account reads: by primary key, [`nickname`](get_by_nickname),
//! actor id, email, `acct` handle and id batch, plus the password-reset
//! candidate search and [`active_local_ids`]. Every read answers `None`
//! (or skips the id) for a row that is not there.

use sqlx::PgPool;
use uuid::Uuid;

use super::User;
use crate::{
    config::Config,
    error::{AppError, AppResult},
};

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

/// Fetches the local, active user named `nickname` for a route that serves
/// one account's public face.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for an unknown nickname or a remote mirror's
/// (neither is told apart from a missing route), [`AppError::Gone`] for a
/// deactivated local account, [`AppError::Database`] on a lookup failure.
pub async fn require_local_active(pool: &PgPool, nickname: &str) -> AppResult<User> {
    let Some(user) = get_by_nickname(pool, nickname).await? else {
        return Err(AppError::RouteNotFound);
    };
    if !user.local.get() {
        return Err(AppError::RouteNotFound);
    }
    if !user.is_active.get() {
        return Err(AppError::Gone);
    }
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

/// Fetches the user whose document named `key_id` as its `publicKey.id`.
///
/// `None` when no mirror stored it. The comparison is exact and served by
/// the unique partial index on the column; it is how an inbox signature's
/// `keyId` finds a path-style key (`GoToSocial`'s `.../users/alice/main-key`)
/// that is not the actor URL plus a fragment.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_public_key_id(pool: &PgPool, key_id: &str) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE public_key_id = $1")
        .bind(key_id)
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

/// The `password_reset` lookup: the local, active account whose nickname or
/// email is exactly `identifier` (citext folds case in both columns).
///
/// A remote mirror, a deactivated account and an account with no email to
/// write to are never a candidate — there is no reset letter to send them
/// (Pleroma's `Registration.password_reset` matches
/// `%User{local: true, is_active: true, email: email} when is_binary(email)`).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn find_password_reset_target(
    pool: &PgPool,
    identifier: &str,
) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>(
        "SELECT * FROM users
          WHERE local AND is_active AND email IS NOT NULL
            AND (nickname = $1::citext OR email = $1::citext)
          LIMIT 1",
    )
    .bind(identifier)
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

/// Fetches the user an `acct` handle names, `None` when absent.
///
/// The resolution mirrors `GET /api/v1/accounts/lookup` (Pleroma's
/// `AccountController.lookup`): the handle is first tried verbatim as a
/// stored nickname — case-insensitively, because the column is citext, so
/// `ALICE` and the remote handle `BOB@Remote.Example` find their rows just
/// like their stored spellings. When nothing matches and the handle
/// carries this instance's own domain (`alice@test.example`, compared
/// case-insensitively), the domain is stripped and the bare nickname is
/// tried again: local rows are stored without their domain (decision 7).
/// Resolving a handle this instance has never seen (via `WebFinger`)
/// belongs to the federation ticket.
///
/// This is a pure resolver: deactivated rows are found like any other;
/// callers that must hide them filter on [`is_active`](User::is_active)
/// themselves.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_acct(pool: &PgPool, config: &Config, acct: &str) -> AppResult<Option<User>> {
    if let Some(found) = get_by_nickname(pool, acct).await? {
        return Ok(Some(found));
    }
    let Some((nickname, domain)) = acct.split_once('@') else {
        return Ok(None);
    };
    if domain.eq_ignore_ascii_case(config.domain()) {
        return get_by_nickname(pool, nickname).await;
    }
    Ok(None)
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

/// The `SELECT` behind [`active_local_ids`]: this instance's own active
/// accounts by id, in `UUIDv7` text order (decision 4's sort, the settled
/// order [`mark_deleted_by_domain`](super::mark_deleted_by_domain) also answers in).
const ACTIVE_LOCAL_IDS_SQL: &str = "SELECT id FROM users WHERE local AND is_active ORDER BY id";

/// The ids of every active LOCAL user, oldest account first.
///
/// The roster for an event that goes to every account this instance owns at
/// once and has no shared hub name to ride: a client's `user` subscription
/// only matches `user:<reader uuid>` at the socket, so the publisher needs
/// the ids themselves to name one stream per reader and to gate the
/// audience — which is [`crate::services::streaming::announcement_published`]'s
/// shape. Remote mirrors and deactivated accounts are out of the read by
/// construction.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn active_local_ids(pool: &PgPool) -> AppResult<Vec<Uuid>> {
    let ids = sqlx::query_scalar::<_, Uuid>(ACTIVE_LOCAL_IDS_SQL)
        .fetch_all(pool)
        .await?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use chrono::{SubsecRound as _, Utc};
    use serde_json::json;

    use super::*;
    use crate::{
        models::user::{
            DEFAULT_ACTOR_TYPE, NewUser, inbox, insert, local_ap_id, set_activation,
            test_support::*,
        },
        util::id::new_id,
    };

    /// Every column of the fixture `NewUser` that came back on the inserted
    /// row, plus the defaults the model and the database fill in.
    fn assert_stored_columns(user: &User) {
        assert!(user.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(user.nickname, "alice", "the nickname round trips");
        assert_eq!(user.ap_id, format!("{BASE}/users/alice"), "ap_id");
        assert!(user.local.get(), "the local flag round trips");
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
            json!([{"name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>"}]),
            "fields round trip through jsonb"
        );
        assert_eq!(
            user.raw_fields,
            json!([{"name": "site", "value": "https://alice.example"}]),
            "raw fields round trip through jsonb"
        );
        assert_eq!(
            user.inbox.as_deref(),
            Some("https://test.example/users/alice/inbox"),
            "inbox column"
        );
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
        assert!(
            user.inserted_at <= user.updated_at,
            "the database stamps both timestamps"
        );
    }

    /// The four reads of [`crate::models::user`] all find the inserted row,
    /// and two of them fold case the way `citext` does.
    async fn assert_every_lookup_finds(pool: &PgPool, user: &User) {
        let by_id = get_by_id(pool, user.id).await.unwrap().expect("by id");
        let by_nickname = get_by_nickname(pool, "ALiCE")
            .await
            .unwrap()
            .expect("citext nickname lookup folds case");
        let by_ap_id = get_by_ap_id(pool, &user.ap_id)
            .await
            .unwrap()
            .expect("by ap_id");
        let by_email = get_by_email(pool, "ALICE@example.org")
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
        let many = get_many(pool, &[user.id, new_id()]).await.unwrap();
        assert_eq!(many.len(), 1, "unknown ids are simply absent");
        assert_eq!(
            many.first().map(|found| found.id),
            Some(user.id),
            "the known id comes back"
        );
    }

    /// The reads that find nothing: an empty id list, and an unknown actor id,
    /// email or nickname.
    async fn assert_unknown_lookups_answer_none(pool: &PgPool) {
        assert!(
            get_many(pool, &[]).await.unwrap().is_empty(),
            "no ids, no rows"
        );
        assert!(
            get_by_ap_id(pool, "https://nowhere.example/users/x")
                .await
                .unwrap()
                .is_none(),
            "an unknown ap_id is None"
        );
        assert!(
            get_by_email(pool, "nobody@example.org")
                .await
                .unwrap()
                .is_none(),
            "an unknown email is None"
        );
        assert!(
            get_by_nickname(pool, "nobody").await.unwrap().is_none(),
            "an unknown nickname is None"
        );
    }

    /// The local-user fixture of these tests: every column the model can
    /// write, set to something distinct.
    async fn inserted_alice(pool: &PgPool) -> User {
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
            fields: json!([{"name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>"}]),
            raw_fields: json!([{"name": "site", "value": "https://alice.example"}]),
            inbox: Some(inbox(&local_ap_id(BASE, "alice"))),
            public_key: Some("PUBLIC-PEM".to_owned()),
            private_key: Some(SECRET_KEY.to_owned()),
            last_fetched_at: Some(fetched_at),
            ..local_new("alice")
        };
        let user = insert(pool, &new).await.unwrap();
        assert_eq!(
            user.last_fetched_at,
            Some(fetched_at),
            "timestamptz round trips"
        );
        assert!(user.domain.is_none(), "local users carry no domain");
        assert!(user.shared_inbox.is_none(), "unset endpoints stay NULL");
        assert!(user.last_status_at.is_none(), "no statuses yet");
        user
    }

    #[tokio::test]
    async fn insert_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = inserted_alice(&pool).await;

        assert_stored_columns(&user);
        assert_every_lookup_finds(&pool, &user).await;
        assert_unknown_lookups_answer_none(&pool).await;
    }

    #[tokio::test]
    async fn get_by_acct_folds_case_and_strips_the_local_domain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        let bob = insert(
            &pool,
            &NewUser {
                nickname: "bob@remote.example".to_owned(),
                ap_id: "https://remote.example/users/bob".to_owned(),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .unwrap();

        for (acct, expected) in [
            ("alice", Some(alice.id)),
            ("ALICE", Some(alice.id)),
            ("alice@test.example", Some(alice.id)),
            ("AlIce@TEST.example", Some(alice.id)),
            ("bob@remote.example", Some(bob.id)),
            ("BOB@Remote.Example", Some(bob.id)),
            ("nobody", None),
            ("nobody@test.example", None),
            ("alice@other.example", None),
            ("carol@remote.example", None),
            ("@test.example", None),
        ] {
            let found = get_by_acct(&pool, &config, acct).await.unwrap();
            let got = found.map(|user| user.id);
            assert_eq!(
                got, expected,
                "`{acct}` must resolve to {expected:?}, got {got:?}"
            );
        }
    }

    #[tokio::test]
    async fn get_by_public_key_id_finds_the_stored_id_and_nothing_else() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mirror = insert(
            &pool,
            &NewUser {
                nickname: "mirror@remote.example".to_owned(),
                ap_id: "https://remote.example/users/mirror".to_owned(),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts");
        let key_id = "https://remote.example/users/mirror/main-key";

        assert!(
            get_by_public_key_id(&pool, key_id).await.unwrap().is_none(),
            "an unknown key id finds nobody"
        );

        let _stored = sqlx::query("UPDATE users SET public_key_id = $1 WHERE id = $2")
            .bind(key_id)
            .bind(mirror.id)
            .execute(&pool)
            .await
            .unwrap();
        let found = get_by_public_key_id(&pool, key_id)
            .await
            .unwrap()
            .expect("the stored key id finds its row");
        assert_eq!(found.id, mirror.id, "the row that stores it");
        assert!(
            get_by_public_key_id(&pool, "https://remote.example/users/mirror")
                .await
                .unwrap()
                .is_none(),
            "the actor id is not a key id"
        );
    }

    #[tokio::test]
    async fn active_local_ids_are_the_local_active_accounts_in_id_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        let bob = insert(&pool, &local_new("bob")).await.unwrap();
        let gone = insert(&pool, &local_new("gone")).await.unwrap();
        set_activation(&pool, gone.id, false).await.unwrap();
        let mirror = insert(
            &pool,
            &NewUser {
                nickname: "mirror@remote.example".to_owned(),
                ap_id: "https://remote.example/users/mirror".to_owned(),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts");

        let ids = active_local_ids(&pool).await.unwrap();
        assert_eq!(
            ids,
            vec![alice.id, bob.id],
            "the two active locals, in `UUIDv7` text order"
        );
        assert!(!ids.contains(&gone.id), "a deactivated account is out");
        assert!(!ids.contains(&mirror.id), "and so is a remote mirror");
    }

    #[tokio::test]
    async fn require_local_active_answers_by_account_state() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        let gone = insert(&pool, &local_new("gone")).await.unwrap();
        set_activation(&pool, gone.id, false).await.unwrap();
        let mirror = insert(
            &pool,
            &NewUser {
                nickname: "mirror@remote.example".to_owned(),
                ap_id: "https://remote.example/users/mirror".to_owned(),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts");

        let found = require_local_active(&pool, "ALICE")
            .await
            .expect("a local active account is served, whatever the case");
        assert_eq!(found.id, alice.id, "the row behind the nickname");

        let unknown = require_local_active(&pool, "nobody").await;
        assert!(
            matches!(unknown, Err(AppError::RouteNotFound)),
            "an unknown nickname is a missing route, got {unknown:?}"
        );
        let remote = require_local_active(&pool, &mirror.nickname).await;
        assert!(
            matches!(remote, Err(AppError::RouteNotFound)),
            "a remote mirror is a missing route, got {remote:?}"
        );
        let deactivated = require_local_active(&pool, "gone").await;
        assert!(
            matches!(deactivated, Err(AppError::Gone)),
            "a deactivated local account is gone, got {deactivated:?}"
        );
    }
}
