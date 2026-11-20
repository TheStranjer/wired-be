//! The narrow reads the MRF preload needs from a user row without pulling
//! the whole account: the follower/status [`counts_by_ap_id`] and the
//! profile [`tags_by_ap_id`], with the local-only view
//! [`local_tags_by_ap_id`] beside it.

use sqlx::PgPool;

use crate::error::AppResult;

/// The denormalized follower and status counters of the user with actor id
/// `ap_id`, `None` when no row carries it — the two facts the
/// [`anti_link_spam`](crate::federation::mrf::anti_link_spam) policy weighs,
/// read without pulling the whole row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn counts_by_ap_id(pool: &PgPool, ap_id: &str) -> AppResult<Option<(i32, i32)>> {
    let counts = sqlx::query_as::<_, (i32, i32)>(
        "SELECT followers_count, statuses_count FROM users WHERE ap_id = $1",
    )
    .bind(ap_id)
    .fetch_optional(pool)
    .await?;
    Ok(counts)
}

/// The profile tags of the user with actor id `ap_id`, `None` when absent;
/// the facts the [`tag`](crate::federation::mrf::tag) policy acts on.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn tags_by_ap_id(pool: &PgPool, ap_id: &str) -> AppResult<Option<Vec<String>>> {
    let tags = sqlx::query_as::<_, (Vec<String>,)>("SELECT tags FROM users WHERE ap_id = $1")
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(tags.map(|(tags,)| tags))
}

/// The profile tags of the *local* user with actor id `ap_id`, `None` when
/// no local row carries it.
///
/// This is the [`tag`](crate::federation::mrf::tag) policy's view of a
/// `Follow`'s target: only a user this instance owns can be tagged for its
/// subscription rules.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn local_tags_by_ap_id(pool: &PgPool, ap_id: &str) -> AppResult<Option<Vec<String>>> {
    let tags =
        sqlx::query_as::<_, (Vec<String>,)>("SELECT tags FROM users WHERE ap_id = $1 AND local")
            .bind(ap_id)
            .fetch_optional(pool)
            .await?;
    Ok(tags.map(|(tags,)| tags))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::user::{Flag, NewUser, insert, test_support::*};

    /// [`counts_by_ap_id`], [`tags_by_ap_id`] and [`local_tags_by_ap_id`]
    /// read the MRF preload facts straight off the row: the counters as
    /// stored, the tags as stored, and the local-only view that a remote
    /// mirror can never answer.
    #[tokio::test]
    async fn mrf_preload_reads_answer_from_the_stored_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let remote = insert(
            &pool,
            &NewUser {
                nickname: "alice@peer.example".to_owned(),
                ap_id: "https://peer.example/users/alice".to_owned(),
                local: Flag::new(false),
                domain: Some("peer.example".to_owned()),
                followers_count: 4,
                statuses_count: 9,
                tags: vec!["mrf_tag:media-strip".to_owned()],
                ..NewUser::default()
            },
        )
        .await
        .unwrap();
        let local = insert(
            &pool,
            &NewUser {
                tags: vec!["mrf_tag:disable-any-subscription".to_owned()],
                ..local_new("carol")
            },
        )
        .await
        .unwrap();
        assert_eq!(remote.followers_count, 4, "the row carries the fixture");

        assert_eq!(
            counts_by_ap_id(&pool, &remote.ap_id).await.unwrap(),
            Some((4, 9)),
            "the counters read back as stored"
        );
        assert_eq!(
            tags_by_ap_id(&pool, &remote.ap_id).await.unwrap(),
            Some(vec!["mrf_tag:media-strip".to_owned()]),
            "and the tags of a remote mirror too"
        );
        assert_eq!(
            local_tags_by_ap_id(&pool, &local.ap_id).await.unwrap(),
            Some(vec!["mrf_tag:disable-any-subscription".to_owned()]),
            "the local view finds the local account"
        );
        assert_eq!(
            local_tags_by_ap_id(&pool, &remote.ap_id).await.unwrap(),
            None,
            "and refuses the remote mirror: only local users are Follow targets"
        );
        assert_eq!(
            local_tags_by_ap_id(&pool, "https://nowhere.example/users/x")
                .await
                .unwrap(),
            None,
            "an unknown actor id answers nothing"
        );
        assert_eq!(
            counts_by_ap_id(&pool, "https://nowhere.example/users/x")
                .await
                .unwrap(),
            None,
            "for the counters as well"
        );
        assert_eq!(
            tags_by_ap_id(&pool, "https://nowhere.example/users/x")
                .await
                .unwrap(),
            None,
            "and for the tags"
        );
    }
}
