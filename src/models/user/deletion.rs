//! The deletion writes: [`mark_deleted`] stamps one account,
//! [`mark_deleted_by_domain`] sweeps every mirror of a domain in one pass,
//! and [`purge_personal_data`] leaves the purged state its deletion job's
//! end step writes.

use sqlx::{Executor, PgPool, Postgres};
use uuid::Uuid;

use crate::error::AppResult;

/// The `UPDATE` behind [`mark_deleted`]: deactivates the account and
/// stamps `deleted_at` in one statement, refreshing `updated_at`.
const MARK_DELETED_SQL: &str = "UPDATE users
       SET is_active = false,
           deleted_at = now(),
           updated_at = now()
     WHERE id = $1";

/// Deactivates user `id` and stamps [`deleted_at`](super::User::deleted_at): the
/// mark `POST /api/pleroma/delete_account` puts on the account.
///
/// The row itself stays — see [`purge_personal_data`] for the state its
/// [`users.delete`](crate::jobs::deletion::KIND_DELETE) job leaves behind.
///
/// Runs on the caller's executor (or any transaction), so the endpoint can
/// land the mark together with the token revocation.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_deleted<'e, E>(executor: E, id: Uuid) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(MARK_DELETED_SQL)
        .bind(id)
        .execute(executor)
        .await
        .map(drop)?;
    Ok(())
}

/// The statement behind [`mark_deleted_by_domain`]: [`MARK_DELETED_SQL`]'s
/// mark applied to every remote mirror of one domain at once, its
/// data-modifying CTE handing the stamped ids back in `UUIDv7` text order
/// — decision 4's sort, so approximately the order the accounts were
/// mirrored and, more usefully, a settled order the caller's queueing
/// and the tests' assertions can both rely on.
const MARK_DELETED_BY_DOMAIN_SQL: &str = "WITH marked AS ( \
    UPDATE users SET is_active = false, deleted_at = now(), updated_at = now() \
    WHERE domain = $1 AND local = false AND deleted_at IS NULL \
    RETURNING id \
) SELECT id FROM marked ORDER BY id";

/// Deactivates every remote mirror of the domain `domain` and stamps
/// [`deleted_at`](super::User::deleted_at) on it, answering with the ids it
/// stamped.
///
/// This is the synchronous first act of
/// `DELETE /api/v1/pleroma/admin/instances/{instance}`
/// ([`crate::api::admin::federation`]), where each returned id then rides
/// its own [`users.delete_remote`](crate::jobs::deletion::KIND_DELETE_REMOTE)
/// sweep — the same mark-then-queue pair
/// [`crate::federation::handlers::actor`] applies to a single actor's
/// self-deletion.
///
/// Rows already stamped are not returned and not re-marked: their accounts
/// are deleted, and the sweep a deletion implies was queued when the mark
/// went on. Local rows carry no [`domain`](super::User::domain) and are out of the
/// statement's reach by construction.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_deleted_by_domain(pool: &PgPool, domain: &str) -> AppResult<Vec<Uuid>> {
    let ids = sqlx::query_scalar::<_, Uuid>(MARK_DELETED_BY_DOMAIN_SQL)
        .bind(domain)
        .fetch_all(pool)
        .await?;
    Ok(ids)
}

/// The `UPDATE` behind [`purge_personal_data`]: nulls the email and
/// password hash, blanks the display name and every profile column (bio,
/// fields, images, birthday, tags, aliases, settings), drops the roles,
/// discoverability and lock, zeroes the counters, and refreshes
/// `updated_at`. `id`, `ap_id`, `nickname`, the keys and `deleted_at` stay.
const PURGE_PERSONAL_DATA_SQL: &str = "UPDATE users
       SET email = NULL,
           password_hash = NULL,
           name = '',
           bio = '',
           raw_bio = '',
           fields = '[]',
           raw_fields = '[]',
           avatar_url = NULL,
           banner_url = NULL,
           background_image_url = NULL,
           birthday = NULL,
           show_birthday = false,
           tags = '{}',
           also_known_as = '{}',
           settings_store = '{}',
           is_admin = false,
           is_moderator = false,
           discoverable = false,
           locked = false,
           followers_count = 0,
           following_count = 0,
           statuses_count = 0,
           updated_at = now()
     WHERE id = $1";

/// Replaces user `id`'s personal data with empty values: the final step of
/// the [`users.delete`](crate::jobs::deletion::KIND_DELETE) job.
///
/// Email, password hash, display name and the whole public profile (bio,
/// fields, avatar, banner, background, birthday, tags, aliases, settings
/// store) go, as do the roles, discoverability, lock and counters; the row
/// stays (decision 7: other tables and mirrored actors reference it) with
/// its id, `ap_id`, nickname, keys and `deleted_at`, so nothing that was
/// said about the account can be attributed to a person any more and a
/// lookup renders no profile data beyond the handle.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn purge_personal_data<'e, E>(executor: E, id: Uuid) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(PURGE_PERSONAL_DATA_SQL)
        .bind(id)
        .execute(executor)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use serde_json::json;

    use super::*;
    use crate::models::user::{Flag, NewUser, User, get_by_id, insert};

    /// A remote mirror of `domain` named `nickname`, inserted.
    async fn mirror_of(pool: &PgPool, nickname: &str, domain: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@{domain}"),
                ap_id: format!("https://{domain}/users/{nickname}"),
                domain: Some(domain.to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the mirror fixture inserts")
    }

    /// `id`'s row, insisted upon.
    async fn must_get(pool: &PgPool, id: Uuid) -> User {
        get_by_id(pool, id)
            .await
            .expect("the read runs")
            .expect("the row stands")
    }

    /// [`mark_deleted_by_domain`] stamps every remote mirror of one
    /// domain in one pass and answers the stamped ids: an already-stamped
    /// row is left out, a local row and another domain's mirror stand
    /// untouched, and a repeat pass marks nothing again.
    #[tokio::test]
    async fn mark_deleted_by_domain_sweeps_one_domains_mirrors() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = mirror_of(&pool, "bob", "sweep.example").await;
        let carol = mirror_of(&pool, "carol", "sweep.example").await;
        let already = mirror_of(&pool, "dave", "sweep.example").await;
        mark_deleted(&pool, already.id)
            .await
            .expect("the pre-stamp lands");
        let alice = insert(
            &pool,
            &NewUser::local("alice", "https://test.example/users/alice"),
        )
        .await
        .expect("the local fixture inserts");
        let erin = mirror_of(&pool, "erin", "keep.example").await;

        let doomed = mark_deleted_by_domain(&pool, "sweep.example")
            .await
            .unwrap();
        let mut expected = vec![bob.id, carol.id];
        expected.sort_unstable();
        assert_eq!(doomed, expected, "the two fresh mirrors, id-ordered");

        for id in [bob.id, carol.id] {
            let gone = must_get(&pool, id).await;
            assert!(!gone.is_active.get(), "{id} is deactivated");
            assert!(gone.deleted_at.is_some(), "{id} carries the stamp");
        }
        let kept = must_get(&pool, erin.id).await;
        assert!(
            kept.is_active.get() && kept.deleted_at.is_none(),
            "another domain stands untouched"
        );
        let local = must_get(&pool, alice.id).await;
        assert!(
            local.is_active.get() && local.deleted_at.is_none(),
            "the local row is out of the statement's reach"
        );

        let second_pass = mark_deleted_by_domain(&pool, "sweep.example")
            .await
            .unwrap();
        assert!(
            second_pass.is_empty(),
            "a repeat pass restamps nobody, the pre-stamped row included"
        );
    }

    /// [`purge_personal_data`] blanks every profile column of a fully
    /// populated local account and zeroes its counters, while the handle,
    /// the actor id and the deletion stamp stand.
    #[tokio::test]
    async fn purge_personal_data_blanks_the_whole_profile() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let full = insert(
            &pool,
            &NewUser {
                email: Some("full@example.org".to_owned()),
                password_hash: Some("hash-of-a-secret".to_owned()),
                name: "Full Profile".to_owned(),
                bio: "<p>about me</p>".to_owned(),
                raw_bio: "about me".to_owned(),
                fields: json!([{"name": "site", "value": "<a>x</a>"}]),
                raw_fields: json!([{"name": "site", "value": "https://x.example"}]),
                avatar_url: Some("https://cdn.example/avatar.png".to_owned()),
                banner_url: Some("https://cdn.example/banner.png".to_owned()),
                background_image_url: Some("https://cdn.example/bg.png".to_owned()),
                birthday: NaiveDate::from_ymd_opt(1990, 5, 17),
                show_birthday: Flag::new(true),
                tags: vec!["force_unlisted".to_owned()],
                also_known_as: vec!["https://other.example/users/full".to_owned()],
                settings_store: json!({"theme": "dark"}),
                is_admin: Flag::new(true),
                is_moderator: Flag::new(true),
                discoverable: Flag::new(true),
                locked: Flag::new(true),
                followers_count: 12,
                following_count: 3,
                statuses_count: 48,
                ..NewUser::local("full", "https://test.example/users/full")
            },
        )
        .await
        .expect("the populated fixture inserts");
        mark_deleted(&pool, full.id).await.expect("the stamp lands");
        let stamped = must_get(&pool, full.id).await;
        assert!(stamped.deleted_at.is_some(), "the fixture is stamped");

        purge_personal_data(&pool, full.id)
            .await
            .expect("the purge runs");

        let purged = must_get(&pool, full.id).await;
        assert!(purged.email.is_none(), "email is gone");
        assert!(purged.password_hash.is_none(), "password hash is gone");
        assert_eq!(purged.name, "", "display name is blank");
        assert_eq!(purged.bio, "", "bio is blank");
        assert_eq!(purged.raw_bio, "", "raw bio is blank");
        assert_eq!(purged.fields, json!([]), "fields are empty");
        assert_eq!(purged.raw_fields, json!([]), "raw fields are empty");
        assert!(purged.avatar_url.is_none(), "avatar is gone");
        assert!(purged.banner_url.is_none(), "banner is gone");
        assert!(purged.background_image_url.is_none(), "background is gone");
        assert!(purged.birthday.is_none(), "birthday is gone");
        assert!(!purged.show_birthday.get(), "birthday is not shown");
        assert!(purged.tags.is_empty(), "tags are empty");
        assert!(purged.also_known_as.is_empty(), "aliases are empty");
        assert_eq!(purged.settings_store, json!({}), "settings store is empty");
        assert!(!purged.is_admin.get(), "admin role is dropped");
        assert!(!purged.is_moderator.get(), "moderator role is dropped");
        assert!(!purged.discoverable.get(), "discoverability is off");
        assert!(!purged.locked.get(), "the lock is off");
        assert_eq!(
            (
                purged.followers_count,
                purged.following_count,
                purged.statuses_count
            ),
            (0, 0, 0),
            "counters are zeroed"
        );

        assert_eq!(purged.nickname, stamped.nickname, "the handle stays");
        assert_eq!(purged.ap_id, stamped.ap_id, "the actor id stays");
        assert_eq!(purged.deleted_at, stamped.deleted_at, "the stamp stays");
    }
}
