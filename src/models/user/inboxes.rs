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
const ALL_REMOTE_INBOXES_SQL: &str = "SELECT DISTINCT COALESCE(shared_inbox, inbox) FROM users \
     WHERE local = false AND deleted_at IS NULL \
       AND COALESCE(shared_inbox, inbox) IS NOT NULL \
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
}
