//! Status search: the query behind the `statuses` arm of
//! `GET /api/v1/search` and `GET /api/v2/search`
//! (`docs/ARCHITECTURE.md` decision 3).
//!
//! Reference: Pleroma's `Activity.Search.search`
//! (`lib/pleroma/activity/search.ex`), whose full-text arm this mirrors on
//! our relational statuses (decision 6): the ticket's migration stores a
//! generated `search_vector tsvector` over `raw_text` and `spoiler_text`
//! built with the `simple` configuration — no stemming, no stop words, so
//! the search finds the words the author wrote in any language — and a GIN
//! index answers the match. A client's query goes through
//! `plainto_tsquery('simple', q)`, which treats it as plain words
//! `AND`ed together rather than `tsquery` syntax the client never meant to
//! write.
//!
//! The rows a search may answer are exactly the rows the viewer may see:
//! [`push_visibility_clause`] narrows the statement by the same rules every
//! timeline applies, and reblog rows are out on top of that — a reblog
//! carries no text of its own (decision 6), so what a boost "contains" is
//! the original, which the original's own row answers with. Tombstones are
//! out as well: [`soft_delete`](crate::models::status::soft_delete) clears
//! their text, but the clause and the explicit `deleted_at IS NULL` do not
//! rely on that.
//!
//! On top of the clause's own rules, a viewer never searches statuses by
//! accounts they have blocked or whose unexpired mutes they hold — the
//! search hides what the viewer asked to stop seeing, the way Pleroma's
//! `maybe_restrict_blocked` drops blocked authors from every search page.
//! (The clause already drops the other direction: an author who blocked
//! the viewer is out of it.)
//!
//! Paging is keyset on the id ([`PageParams::apply_keyset`]) with the
//! `offset` the endpoint also accepts, ordered `id DESC`; the caller owns
//! the default and cap of the page (20 and 40 for the search's statuses).

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{status::Status, status_visibility::push_visibility_clause, user::User},
    pagination::PageParams,
};

/// The shared head of every [`search_statuses`] statement: live, non-reblog
/// rows whose generated text vector matches the query. The bind for the
/// query text is pushed right after this, and [`push_visibility_clause`],
/// the viewer exclusions, the author filter and the page follow.
const BASE_SQL: &str = "SELECT s.* FROM statuses s \
     WHERE s.reblog_of_id IS NULL AND s.deleted_at IS NULL \
     AND s.search_vector @@ plainto_tsquery('simple', ";

/// The tail of the mute exclusion: the edge hides the author only while it
/// has no expiry, or an expiry that has not lapsed — the same test the
/// account search and every other mute read of this repo applies.
const MUTE_LIVE_ARMS: &str = " AND (m.expires_at IS NULL OR m.expires_at > now()))";

/// The [`crate::models::status::Status`] rows matching `query` for
/// `viewer`, as the search's newest-first page.
///
/// `viewer` is `None` for an anonymous request, and the visibility clause
/// then leaves only `public` and `unlisted` rows searchable. `account_id`,
/// when given, narrows the search to that one author's posts. A query that
/// trims to nothing answers the empty `Vec` without touching the database,
/// and so would any query of pure punctuation —
/// `plainto_tsquery('simple', '...')` is an empty `tsquery`, which matches
/// no row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn search_statuses(
    pool: &PgPool,
    viewer: Option<&User>,
    query: &str,
    account_id: Option<Uuid>,
    page: &PageParams,
) -> AppResult<Vec<Status>> {
    let needle = query.trim();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Postgres>::new(BASE_SQL);
    let _match: &mut _ = builder.push_bind(needle.to_owned()).push(")");
    push_visibility_clause(&mut builder, viewer, "s");
    push_viewer_exclusions(&mut builder, viewer);
    if let Some(account_id) = account_id {
        let _author: &mut _ = builder.push(" AND s.user_id = ").push_bind(account_id);
    }
    page.apply_keyset(&mut builder, "s.id");
    let _offset: &mut _ = builder
        .push(" OFFSET ")
        .push_bind(page.offset.unwrap_or(0).max(0));
    let rows = builder.build_query_as::<Status>().fetch_all(pool).await?;
    Ok(page.finish(rows))
}

/// Appends the viewer-side exclusions: authors the viewer blocked, and
/// authors under one of the viewer's unexpired mutes. An anonymous viewer
/// has neither, so an anonymous request gains no condition here.
fn push_viewer_exclusions(builder: &mut QueryBuilder<Postgres>, viewer: Option<&User>) {
    let Some(viewer) = viewer else {
        return;
    };
    let _blocked: &mut _ = builder
        .push(" AND NOT EXISTS (SELECT 1 FROM blocks b WHERE b.blocker_id = ")
        .push_bind(viewer.id)
        .push(" AND b.blocked_id = s.user_id)");
    let _muted: &mut _ = builder
        .push(" AND NOT EXISTS (SELECT 1 FROM mutes m WHERE m.muter_id = ")
        .push_bind(viewer.id)
        .push(" AND m.muted_id = s.user_id")
        .push(MUTE_LIVE_ARMS);
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            block, follow,
            mute::{self, MuteOptions},
            status::{self, NewStatus, Visibility},
            user::{NewUser, insert as insert_user, local_ap_id},
        },
    };

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = TEST_BASE_URL;
    /// A limit above every fixture page this module asks for, so a "full"
    /// page really is the whole answer.
    const EVERY: i64 = 80;

    /// A local user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert_user(
            pool,
            &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts `text` as `author`'s status of `visibility`.
    async fn post(pool: &PgPool, author: &User, visibility: Visibility, text: &str) -> Status {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                visibility,
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("a plain local post inserts")
    }

    /// The page parameters of a query string, parsed the way the endpoint
    /// would.
    fn page(query: &str) -> PageParams {
        PageParams::from_query(Some(query), 40).expect("the fixed test page parses")
    }

    /// The search as the endpoint runs it, with `extra` pagination keys.
    async fn find(
        pool: &PgPool,
        viewer: Option<&User>,
        query: &str,
        account_id: Option<Uuid>,
        extra: &str,
    ) -> AppResult<Vec<Status>> {
        search_statuses(
            pool,
            viewer,
            query,
            account_id,
            &page(&format!("limit={EVERY}&{extra}")),
        )
        .await
    }

    /// The full page of a query for `viewer`, expecting success.
    async fn search(pool: &PgPool, viewer: Option<&User>, query: &str) -> Vec<Status> {
        find(pool, viewer, query, None, "")
            .await
            .expect("the search runs")
    }

    /// The ids of `statuses`, in the order they came back.
    fn ids(statuses: &[Status]) -> Vec<Uuid> {
        statuses.iter().map(|found| found.id).collect()
    }

    /// A set of ids, so assertions name membership whatever order (and
    /// whatever same-millisecond `UUIDv7` race) the rows came back in.
    fn id_set(id_list: &[Uuid]) -> HashSet<Uuid> {
        id_list.iter().copied().collect()
    }

    /// Whether `id_list` strictly descends, as a newest-first page must.
    fn is_descending(id_list: &[Uuid]) -> bool {
        id_list.windows(2).all(|pair| match pair {
            [newer, older] => newer > older,
            _ => false,
        })
    }

    #[tokio::test]
    async fn matched_words_find_their_posts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let both = post(&pool, &alice, Visibility::Public, "the quick brown fox").await;
        let warned = post(&pool, &alice, Visibility::Public, "unrelated body").await;
        let warning =
            sqlx::query("UPDATE statuses SET spoiler_text = 'fox sighting' WHERE id = $1")
                .bind(warned.id)
                .execute(&pool)
                .await
                .expect("the fixture warning lands");
        assert_eq!(warning.rows_affected(), 1, "and it hit the fixture row");
        let _slow = post(&pool, &alice, Visibility::Public, "a slow brown cat").await;

        assert_eq!(
            id_set(&ids(&search(&pool, None, "fox").await)),
            id_set(&[both.id, warned.id]),
            "the word matches raw text and `spoiler_text` alike"
        );
        assert_eq!(
            ids(&search(&pool, None, "quick fox").await),
            vec![both.id],
            "`plainto` ANDs the words: only the post carrying both matches"
        );
        assert!(
            search(&pool, None, "quickly fox").await.is_empty(),
            "`simple` means no stemming: `quickly` is not the word `quick`"
        );
        assert!(
            search(&pool, None, "  ").await.is_empty(),
            "a blank query searches nothing"
        );
        assert!(
            search(&pool, None, "...").await.is_empty(),
            "a query with no words is an empty tsquery, which matches nothing"
        );
    }

    #[tokio::test]
    async fn a_reblog_row_and_a_tombstone_never_match() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let original = post(&pool, &alice, Visibility::Public, "searchable original").await;
        let reblog = status::insert(
            &pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(original.id),
                ..NewStatus::local(&bob, "")
            },
        )
        .await
        .expect("a reblog inserts");
        let doomed = post(&pool, &alice, Visibility::Public, "searchable doomed").await;
        assert!(
            status::soft_delete(&pool, doomed.id)
                .await
                .expect("the fixture delete runs")
                .is_some(),
            "one matching post is now a tombstone"
        );

        let found = ids(&search(&pool, None, "searchable").await);
        assert_eq!(found, vec![original.id], "the original answers for itself");
        assert!(
            !found.contains(&reblog.id),
            "a reblog row carries no text and is excluded outright"
        );
    }

    #[tokio::test]
    async fn visibility_decides_who_finds_a_private_post() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let carol = user(&pool, "carol").await;
        let edge = post(&pool, &alice, Visibility::Private, "keyword followers").await;
        let open = post(&pool, &alice, Visibility::Public, "keyword everyone").await;

        for (who, viewer) in [("carol", Some(&carol)), ("anonymous", None)] {
            assert_eq!(
                id_set(&ids(&search(&pool, viewer, "keyword").await)),
                id_set(&[open.id]),
                "a stranger ({who}) searches only what is open to them"
            );
        }
        assert_eq!(
            id_set(&ids(&search(&pool, Some(&alice), "keyword").await)),
            id_set(&[edge.id, open.id]),
            "the author finds their own private post"
        );
        let followed = follow::follow(&pool, BASE, &bob, &alice)
            .await
            .expect("bob follows alice");
        assert_eq!(
            followed.state,
            follow::FollowState::Accepted,
            "alice is unlocked, so the follow is granted"
        );
        assert_eq!(
            id_set(&ids(&search(&pool, Some(&bob), "keyword").await)),
            id_set(&[edge.id, open.id]),
            "the private status is found only by the follower (and the author)"
        );
    }

    #[tokio::test]
    async fn blocked_and_muted_authors_are_excluded() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let viewer = user(&pool, "viewer").await;
        let stranger = user(&pool, "stranger").await;
        let bane = user(&pool, "bane").await;
        let muted = user(&pool, "mutedpal").await;
        let blocked = post(&pool, &bane, Visibility::Public, "keyword from bane").await;
        let hidden = post(&pool, &muted, Visibility::Public, "keyword from the muted").await;
        let kept = post(&pool, &stranger, Visibility::Public, "keyword kept").await;
        let edge = block::block(&pool, BASE, &viewer, &bane)
            .await
            .expect("the viewer blocks bane");
        assert_eq!(edge.blocked_id, bane.id, "the fixture block landed");
        let mute = mute::mute(&pool, &viewer, &muted, MuteOptions::default())
            .await
            .expect("the viewer mutes the pal");
        assert_eq!(mute.muted_id, muted.id, "the fixture mute landed");

        assert_eq!(
            id_set(&ids(&search(&pool, Some(&viewer), "keyword").await)),
            id_set(&[kept.id]),
            "the viewer's blocked and muted authors drop out of their search"
        );
        assert_eq!(
            id_set(&ids(&search(&pool, Some(&stranger), "keyword").await)),
            id_set(&[blocked.id, hidden.id, kept.id]),
            "someone else's block and mute hide nothing from a third account"
        );

        let flipped = block::block(&pool, BASE, &bane, &viewer)
            .await
            .expect("bane blocks the viewer back");
        assert_eq!(flipped.blocker_id, bane.id, "the second block lands");
        assert_eq!(
            id_set(&ids(&search(&pool, Some(&viewer), "keyword").await)),
            id_set(&[kept.id]),
            "and the author's own block of the viewer keeps them out too, \
             as the visibility clause's rule 2 says"
        );
    }

    #[tokio::test]
    async fn account_id_restricts_the_search_to_one_author() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let bob = user(&pool, "bob").await;
        let hers = post(&pool, &alice, Visibility::Public, "keyword hers").await;
        let his = post(&pool, &bob, Visibility::Public, "keyword his").await;

        assert_eq!(
            ids(&find(&pool, Some(&bob), "keyword", Some(alice.id), "")
                .await
                .expect("the author-filtered search runs")),
            vec![hers.id],
            "only that author's matching posts answer"
        );
        assert_eq!(
            ids(&find(&pool, Some(&bob), "keyword", Some(bob.id), "")
                .await
                .expect("the self-filtered search runs")),
            vec![his.id],
            "and the filter follows the asked-for author, not the viewer"
        );
        assert!(
            find(&pool, Some(&bob), "keyword", Some(Uuid::now_v7()), "")
                .await
                .expect("an unknown author is no error")
                .is_empty(),
            "an id no status carries answers the empty page"
        );
    }

    #[tokio::test]
    async fn cursors_limit_and_offset_walk_the_newest_first_page() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        for n in 0..5 {
            let _posted = post(
                &pool,
                &alice,
                Visibility::Public,
                &format!("paging keyword {n}"),
            )
            .await;
        }
        let all = ids(&search(&pool, None, "keyword").await);
        assert_eq!(all.len(), 5, "five matches on the full page");
        assert!(is_descending(&all), "the page is ordered `id DESC`");

        let middle = all[2];
        let older = find(&pool, None, "keyword", None, &format!("max_id={middle}"))
            .await
            .expect("the max_id page runs");
        assert_eq!(
            ids(&older),
            all[3..],
            "max_id answers the strictly older rows, still newest-first"
        );
        let newer = find(&pool, None, "keyword", None, &format!("min_id={middle}"))
            .await
            .expect("the min_id page runs");
        assert_eq!(
            ids(&newer),
            all[..2],
            "min_id answers the newer rows and is still handed back newest-first"
        );
        let small = find(&pool, None, "keyword", None, "limit=2")
            .await
            .expect("the limited page runs");
        assert_eq!(ids(&small), all[..2], "limit cuts the page short");
        let second = find(&pool, None, "keyword", None, "limit=2&offset=2")
            .await
            .expect("the offset page runs");
        assert_eq!(ids(&second), all[2..4], "and offset walks it");
        assert!(
            find(&pool, None, "keyword", None, "offset=99")
                .await
                .expect("past the end is no error")
                .is_empty(),
            "an offset beyond the matches answers nothing"
        );
    }
}
