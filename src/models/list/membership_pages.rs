//! Reading a list's members as a page: [`members_page`] walks the
//! membership's own `position` ordinals newest first, and [`member_ids`] reads
//! the whole membership in the order it was added.

use sqlx::{PgPool, QueryBuilder, postgres::Postgres};
use uuid::Uuid;

use crate::{error::AppResult, pagination::OrdinalPageParams};

use super::MemberEntry;

/// [`members_page`]: one page of a list's members as their `users` rows, each
/// entry led by the `position` of the membership row behind it — the ordinal
/// the page keys on. The text stops where its one placeholder belongs, because
/// that is where [`members_page`] pushes its bind and the builder is what
/// numbers it `$1`; the keyset it then appends runs on `lm.position`.
const MEMBERS_PAGE_SQL: &str = "SELECT lm.position, u.* FROM list_members lm
     JOIN users u ON u.id = lm.user_id
     WHERE lm.list_id = ";

/// [`member_ids`]: every member of one list, in the order they were added —
/// the whole membership, which is what a later federation ticket walks when it
/// delivers a status to a list.
const MEMBER_IDS_SQL: &str = "SELECT user_id FROM list_members
     WHERE list_id = $1 ORDER BY position";

/// The members of `list_id`, newest membership first and `page`'s `limit` at a
/// time, each entry carrying its [`MemberEntry::position`] next to the [`User`]
/// (see [`MemberEntry`]).
///
/// [`User`]: crate::models::user::User
///
/// [`OrdinalPageParams`]'s cursors are position ordinals, and the page is
/// ordered by them: the order the accounts were added in, newest first — the
/// insertion order the membership's own `position` sequence records, not the
/// members' `UUIDv7` ids (decision 4's rule applies to id-keyed listings).
/// `limit=0` answers every member.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn members_page(
    pool: &PgPool,
    list_id: Uuid,
    page: OrdinalPageParams,
) -> AppResult<Vec<MemberEntry>> {
    let mut builder = QueryBuilder::<Postgres>::new(MEMBERS_PAGE_SQL);
    // The builder's push methods hand back `&mut Self` for chaining; binding
    // it keeps the statement from having an unused result (decision 14).
    let _bound: &mut _ = builder.push_bind(list_id);
    page.apply_keyset(&mut builder, "lm.position");
    let rows = builder
        .build_query_as::<MemberEntry>()
        .fetch_all(pool)
        .await?;
    Ok(page.finish(rows))
}

/// The ids of `list_id`'s members, oldest membership first — the whole
/// membership, unordered by anything but the order it was added in.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn member_ids(pool: &PgPool, list_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(MEMBER_IDS_SQL)
        .bind(list_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::list::{List, test_support::*},
        pagination::MAX_LIMIT,
    };

    /// One page of `list`'s members with `query` applied, unwrapped with the
    /// caller's own reason `why`.
    async fn members(pool: &PgPool, list: &List, query: &str, why: &str) -> Vec<MemberEntry> {
        members_page(pool, list.id, ordinal_page(query))
            .await
            .unwrap_or_else(|err| panic!("{why}: {err}"))
    }

    /// The `position` of entry `index` of `entries` — the ordinal a cursor is
    /// keyed on — spelled as the page spells it.
    fn position_of(entries: &[MemberEntry], index: usize, why: &str) -> String {
        entries
            .get(index)
            .unwrap_or_else(|| panic!("{why}"))
            .position
            .to_string()
    }

    #[tokio::test]
    async fn members_page_is_newest_membership_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let dave = user(&pool, "dave").await;
        let hers = seed(&pool, &alice, "hers").await;
        add_member(&pool, &hers, &bob).await;
        add_member(&pool, &hers, &carol).await;
        add_member(&pool, &hers, &dave).await;
        let theirs = seed(&pool, &alice, "theirs").await;
        add_member(&pool, &theirs, &bob).await;

        let first = members(&pool, &hers, "limit=2", "the first page reads").await;
        assert_eq!(
            nicks(&first),
            ["dave", "carol"],
            "newest membership first, and only the named list's"
        );
        let oldest = position_of(&first, 1, "the page holds two entries");
        let newest = position_of(&first, 0, "the first page holds two entries");

        let second = members(
            &pool,
            &hers,
            &format!("limit=2&max_id={oldest}"),
            "the next page reads",
        )
        .await;
        assert_eq!(
            nicks(&second),
            ["bob"],
            "max_id walks to the older memberships"
        );
        let only = position_of(&second, 0, "the second page holds one entry");

        let back = members(
            &pool,
            &hers,
            &format!("limit=2&min_id={only}"),
            "and min_id walks back",
        )
        .await;
        assert_eq!(nicks(&back), ["dave", "carol"], "newer ones, newest first");

        let since_newest = members(
            &pool,
            &hers,
            &format!("limit=2&since_id={newest}"),
            "since_id reads unseen rows",
        )
        .await;
        assert!(
            since_newest.is_empty(),
            "nothing is newer than the newest membership the client has seen"
        );

        let since_oldest = members(
            &pool,
            &hers,
            &format!("limit=2&since_id={only}"),
            "and a cursor further back reads forward",
        )
        .await;
        assert_eq!(
            nicks(&since_oldest),
            ["dave", "carol"],
            "since_id answers everything newer than the cursor, newest first"
        );
    }

    #[tokio::test]
    async fn a_zero_limit_answers_every_member() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let hers = seed(&pool, &alice, "hers").await;
        let mut added = Vec::new();
        for index in 0..25 {
            let member = user(&pool, &format!("member{index}")).await;
            add_member(&pool, &hers, &member).await;
            added.push(member);
        }
        assert_eq!(added.len(), 25, "twenty-five members, over a page");

        let capped = members_page(&pool, hers.id, ordinal_page(""))
            .await
            .expect("the default page reads");
        assert_eq!(capped.len(), 20, "and the default page is 20 of them");

        let every = members_page(&pool, hers.id, ordinal_page("limit=0"))
            .await
            .expect("limit=0 reads");
        assert_eq!(every.len(), 25, "and every member is on it");
        assert_eq!(
            every.first().expect("the page is not empty").user.id,
            added.last().expect("twenty-five members were added").id,
            "newest membership first"
        );
    }

    #[tokio::test]
    async fn members_of_an_empty_list_are_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let hers = seed(&pool, &alice, "hers").await;

        assert!(
            members_page(&pool, hers.id, ordinal_page(""))
                .await
                .expect("reading an empty list succeeds")
                .is_empty(),
            "no memberships, no entries"
        );
        assert!(
            member_ids(&pool, hers.id)
                .await
                .expect("reading an empty list succeeds")
                .is_empty(),
            "and no ids"
        );
    }

    #[tokio::test]
    async fn member_ids_of_an_unknown_list_are_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        assert!(
            member_ids(&pool, Uuid::now_v7())
                .await
                .expect("reading nothing is not a failure")
                .is_empty(),
            "a list that never existed has no members"
        );
    }

    /// Ordinal page params built from a query string, the way the endpoint's
    /// parse makes them.
    fn ordinal_page(query: &str) -> OrdinalPageParams {
        OrdinalPageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture query parses")
    }

    /// The nicknames of `entries`' members, in the order they came back.
    fn nicks(entries: &[MemberEntry]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| entry.user.nickname.as_str())
            .collect()
    }
}
