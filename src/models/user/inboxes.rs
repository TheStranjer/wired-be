//! The inbox census behind an account's `Delete` fan-out:
//! [`all_remote_inboxes`] answers every inbox this instance knows a remote
//! account at.

use sqlx::PgPool;

use crate::error::AppResult;

/// The `SELECT` behind [`all_remote_inboxes`]: one row per distinct inbox
/// a live remote mirror publishes — its shared (instance-wide) inbox when
/// it has one, its personal inbox otherwise — the same preference the
/// publisher's `best_inbox` applies. A mirror with neither, and a mirror
/// the instance has deleted, contribute nothing.
///
/// The inboxes are the ones on the account's `activitypub_actors` row; an
/// account without a row has no `ActivityPub` identity and adds nothing.
const ALL_REMOTE_INBOXES_SQL: &str = "SELECT DISTINCT COALESCE(a.shared_inbox, a.inbox) \
       FROM users u JOIN activitypub_actors a ON a.user_id = u.id \
     WHERE u.local = false AND u.deleted_at IS NULL \
       AND COALESCE(a.shared_inbox, a.inbox) IS NOT NULL \
     ORDER BY 1";

/// Every distinct inbox of a live remote account this instance has ever
/// stored, the shared inbox preferred over the personal one.
///
/// Entries that are blank or not an `http(s)` URL are left out, exactly as
/// the follower and followee snapshot of the `Delete` leaves them out.
/// Ordered by URL, so the answer is stable.
///
/// # Errors
///
/// Any database failure (connection, decoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn all_remote_inboxes(pool: &PgPool) -> AppResult<Vec<String>> {
    let inboxes = sqlx::query_scalar::<_, String>(ALL_REMOTE_INBOXES_SQL)
        .fetch_all(pool)
        .await?;
    Ok(inboxes
        .into_iter()
        .filter(|inbox| !inbox.trim().is_empty() && inbox.starts_with("http"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::user::{Flag, NewUser, insert};

    /// A remote mirror on `host` with the given inbox pair.
    async fn mirror(pool: &PgPool, name: &str, inbox: Option<&str>, shared: Option<&str>) {
        let _row = insert(
            pool,
            &NewUser {
                nickname: format!("{name}@host.example"),
                ap_id: format!("https://host.example/users/{name}"),
                local: Flag::new(false),
                domain: Some("host.example".to_owned()),
                inbox: inbox.map(str::to_owned),
                shared_inbox: shared.map(str::to_owned),
                ..NewUser::default()
            },
        )
        .await
        .expect("the mirror fixture inserts");
    }

    /// Shared inboxes win over personal ones, duplicates collapse, and
    /// mirrors without any inbox, local accounts and deleted mirrors add
    /// nothing.
    #[tokio::test]
    async fn lists_each_live_remote_inbox_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        mirror(
            &pool,
            "a",
            Some("https://a.example/users/a/inbox"),
            Some("https://a.example/inbox"),
        )
        .await;
        mirror(
            &pool,
            "b",
            Some("https://a.example/users/b/inbox"),
            Some("https://a.example/inbox"),
        )
        .await;
        mirror(&pool, "c", Some("https://c.example/users/c/inbox"), None).await;
        mirror(&pool, "d", None, None).await;
        mirror(&pool, "e", Some("not-a-url"), None).await;
        mirror(&pool, "gone", Some("https://gone.example/inbox"), None).await;
        let _marked = sqlx::query("UPDATE users SET deleted_at = now() WHERE nickname = $1")
            .bind("gone@host.example")
            .execute(&pool)
            .await
            .expect("the deletion mark writes");
        let local = NewUser::local("alice", "https://wired.test/users/alice");
        let _alice = insert(
            &pool,
            &NewUser {
                inbox: Some("https://wired.test/users/alice/inbox".to_owned()),
                ..local
            },
        )
        .await
        .expect("the local fixture inserts");

        let inboxes = all_remote_inboxes(&pool).await.expect("the census reads");

        assert_eq!(
            inboxes,
            vec![
                "https://a.example/inbox".to_owned(),
                "https://c.example/users/c/inbox".to_owned(),
            ]
        );
    }

    /// The census reads `activitypub_actors`: a change made to that table
    /// alone moves the inbox, and an account without a row adds nothing.
    #[tokio::test]
    async fn follows_the_actor_rows_not_the_users_columns() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        mirror(
            &pool,
            "moved",
            Some("https://old.example/users/moved/inbox"),
            Some("https://old.example/inbox"),
        )
        .await;
        mirror(
            &pool,
            "rowless",
            Some("https://rowless.example/inbox"),
            None,
        )
        .await;
        let moved = sqlx::query(
            "UPDATE activitypub_actors SET shared_inbox = $1 \
             WHERE user_id = (SELECT id FROM users WHERE nickname = $2)",
        )
        .bind("https://new.example/inbox")
        .bind("moved@host.example")
        .execute(&pool)
        .await
        .expect("the actor row updates")
        .rows_affected();
        assert_eq!(moved, 1, "the shared inbox moved on the actor row only");
        let dropped = sqlx::query(
            "DELETE FROM activitypub_actors \
             WHERE user_id = (SELECT id FROM users WHERE nickname = $1)",
        )
        .bind("rowless@host.example")
        .execute(&pool)
        .await
        .expect("the actor row deletes")
        .rows_affected();
        assert_eq!(dropped, 1, "the account's actor row is gone");

        let inboxes = all_remote_inboxes(&pool).await.expect("the census reads");

        assert_eq!(
            inboxes,
            vec!["https://new.example/inbox".to_owned()],
            "the actor row's shared inbox, and nothing for the account without a row"
        );
    }
}
