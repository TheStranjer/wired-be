//! Writing a list's members: [`add_members`] records the accounts that joined
//! and [`remove_members`] the ones that left, both keyed on the membership
//! row's own `(list_id, user_id)` primary key — which is what makes adding
//! twice one membership and removing twice one removal.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

/// [`add_members`]: every requested member in one statement, each row taking
/// the next `position` from the sequence (so the array's order is the order the
/// members were added in). A pair that is already a member conflicts on the
/// primary key and is left exactly where it was — Pleroma's `Enum.uniq` on the
/// list's address array, expressed as the upsert this table's key allows, and
/// what makes adding twice succeed without writing a second row.
const ADD_MEMBERS_SQL: &str = "INSERT INTO list_members (list_id, user_id)
     SELECT $1::uuid, id FROM unnest($2::uuid[]) AS requested(id)
ON CONFLICT (list_id, user_id) DO NOTHING";

/// [`remove_members`]: the memberships of the given accounts in this one list.
/// An id that is not a member matches nothing and changes nothing, which is
/// Pleroma's `List.unfollow` being content with an address the array never
/// held.
const REMOVE_MEMBERS_SQL: &str = "DELETE FROM list_members
     WHERE list_id = $1 AND user_id = ANY($2)";

/// Adds `member_ids` to the list `list_id`, returning how many memberships
/// were new.
///
/// Every id takes the next [`list_members.position`](super::MemberEntry::position)
/// ordinal, in the order given, so the array's order is the order the accounts
/// joined the list in. An id that is already a member is kept as it was — its
/// position included, which is why re-adding a member is idempotent and does
/// not move them to the end of the list. An empty `member_ids` writes nothing.
///
/// The owner's right to the list, and Mastodon's rule that only followed
/// accounts may be added, are the API's question (see
/// [`crate::api::lists`]); this write trusts its caller and only needs the list
/// row to exist, which the `list_id` foreign key enforces.
///
/// # Errors
///
/// Any database failure (connection, encoding, a list that does not exist)
/// arrives as [`AppError::Database`](crate::error::AppError).
pub async fn add_members(pool: &PgPool, list_id: Uuid, member_ids: &[Uuid]) -> AppResult<u64> {
    let inserted = sqlx::query(ADD_MEMBERS_SQL)
        .bind(list_id)
        .bind(member_ids)
        .execute(pool)
        .await?;
    Ok(inserted.rows_affected())
}

/// Removes `member_ids` from the list `list_id`, returning how many
/// memberships went away.
///
/// An id that is not a member of this list — or that is not a member of any
/// list at all — matches no row and changes nothing (Pleroma's `List.unfollow`
/// on an address the list's array never held), so removing twice removes once
/// and the second call answers `0`. An empty `member_ids` matches nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn remove_members(pool: &PgPool, list_id: Uuid, member_ids: &[Uuid]) -> AppResult<u64> {
    let deleted = sqlx::query(REMOVE_MEMBERS_SQL)
        .bind(list_id)
        .bind(member_ids)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        error::AppError,
        models::list::{delete, lists_containing, member_ids, test_support::*},
    };

    #[tokio::test]
    async fn membership_is_idempotent_only_through_the_key() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = seed(&pool, &alice, "hers").await;
        add_member(&pool, &hers, &bob).await;

        let refused = sqlx::query("INSERT INTO list_members (list_id, user_id) VALUES ($1, $2)")
            .bind(hers.id)
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect_err("the primary key refuses a second row");
        assert!(
            matches!(refused, sqlx::Error::Database(_)),
            "and the refusal is the database's own: {refused}"
        );
        assert_eq!(
            lists_containing(&pool, alice.id, bob.id)
                .await
                .expect("the lookup reads")
                .len(),
            1,
            "and the lookup answers the list once"
        );
    }

    #[tokio::test]
    async fn deleting_a_list_takes_its_memberships() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let doomed = seed(&pool, &alice, "doomed").await;
        add_member(&pool, &doomed, &bob).await;
        assert_eq!(count_members(&pool).await, 1, "one membership");

        assert!(
            delete(&pool, alice.id, doomed.id)
                .await
                .expect("the list deletes"),
            "and it goes"
        );
        assert_eq!(
            count_members(&pool).await,
            0,
            "taking its memberships with it"
        );
    }

    #[tokio::test]
    async fn deleting_a_user_takes_their_lists_and_memberships() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = seed(&pool, &alice, "hers").await;
        add_member(&pool, &hers, &bob).await;

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .expect("the fixture user deletes");
        assert_eq!(deleted.rows_affected(), 1, "one account went");

        assert_eq!(count_lists(&pool).await, 0, "and her lists with it");
        assert_eq!(
            count_members(&pool).await,
            0,
            "and every membership in them"
        );
    }

    #[tokio::test]
    async fn deleting_a_member_takes_their_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = seed(&pool, &alice, "hers").await;
        add_member(&pool, &hers, &bob).await;

        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.id)
            .execute(&pool)
            .await
            .expect("the fixture member deletes");
        assert_eq!(deleted.rows_affected(), 1, "the account went");
        assert_eq!(
            count_members(&pool).await,
            0,
            "and its memberships everywhere"
        );
        assert_eq!(count_lists(&pool).await, 1, "while alice's list stands");
    }

    #[tokio::test]
    async fn add_members_records_the_order_it_was_given() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let hers = seed(&pool, &alice, "hers").await;

        let added = add_members(&pool, hers.id, &[carol.id, bob.id])
            .await
            .expect("both members add");
        assert_eq!(added, 2, "two memberships went in");
        assert_eq!(
            member_ids(&pool, hers.id).await.expect("members read"),
            vec![carol.id, bob.id],
            "and the list holds them in the order they were added, not the \
             order of their ids"
        );

        let positions = positions(&pool, hers.id).await;
        assert!(
            positions.first() < positions.get(1),
            "the earlier member has the smaller ordinal: {positions:?}"
        );
    }

    #[tokio::test]
    async fn adding_a_member_twice_writes_no_second_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let hers = seed(&pool, &alice, "hers").await;
        add_member(&pool, &hers, &bob).await;
        let before = positions(&pool, hers.id).await;

        let added = add_members(&pool, hers.id, &[bob.id, carol.id])
            .await
            .expect("adding a member twice is not a failure");
        assert_eq!(added, 1, "only the new account became a member");
        assert_eq!(
            member_ids(&pool, hers.id).await.expect("members read"),
            vec![bob.id, carol.id],
            "bob stayed, carol joined"
        );
        let after = positions(&pool, hers.id).await;
        assert_eq!(
            after.first(),
            before.first(),
            "and re-adding did not move bob to the end of the list"
        );
    }

    #[tokio::test]
    async fn adding_nobody_writes_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let hers = seed(&pool, &alice, "hers").await;

        assert_eq!(
            add_members(&pool, hers.id, &[])
                .await
                .expect("an empty add succeeds"),
            0,
            "no id, no membership"
        );
        assert!(
            member_ids(&pool, hers.id)
                .await
                .expect("members read")
                .is_empty(),
            "and the list is still empty"
        );
    }

    #[tokio::test]
    async fn adding_to_a_list_that_does_not_exist_fails() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bob = user(&pool, "bob").await;

        let err = add_members(&pool, Uuid::now_v7(), &[bob.id])
            .await
            .expect_err("no list row, no membership");
        assert!(
            matches!(err, AppError::Database(_)),
            "the list_id foreign key refuses it: {err}"
        );
        assert_eq!(count_members(&pool).await, 0, "and nothing was written");
    }

    #[tokio::test]
    async fn remove_members_takes_one_list_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let first = seed(&pool, &alice, "first").await;
        let second = seed(&pool, &alice, "second").await;
        add_member(&pool, &first, &bob).await;
        add_member(&pool, &second, &bob).await;

        let removed = remove_members(&pool, first.id, &[bob.id])
            .await
            .expect("the member goes");
        assert_eq!(removed, 1, "one membership ended");
        assert!(
            member_ids(&pool, first.id)
                .await
                .expect("members read")
                .is_empty(),
            "of the list it was named in"
        );
        assert_eq!(
            member_ids(&pool, second.id).await.expect("members read"),
            vec![bob.id],
            "while his other membership stands"
        );

        assert_eq!(
            remove_members(&pool, first.id, &[bob.id])
                .await
                .expect("removing a non-member succeeds"),
            0,
            "and removing him again removes nobody"
        );
    }

    #[tokio::test]
    async fn removing_nobody_takes_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let carol = user(&pool, "carol").await;
        let hers = seed(&pool, &alice, "hers").await;

        for ids in [Vec::new(), vec![carol.id]] {
            assert_eq!(
                remove_members(&pool, hers.id, &ids)
                    .await
                    .expect("an empty or unmatched remove succeeds"),
                0,
                "{ids:?} named no member"
            );
        }
    }

    /// The `list_members.position` ordinals of `list_id`'s members, oldest
    /// first — what the page keys on.
    async fn positions(pool: &PgPool, list_id: Uuid) -> Vec<i64> {
        sqlx::query_scalar::<_, i64>(
            "SELECT position FROM list_members WHERE list_id = $1 ORDER BY position",
        )
        .bind(list_id)
        .fetch_all(pool)
        .await
        .expect("the positions read")
    }
}
