//! The timeline query builder: the one statement behind every status
//! timeline (`docs/ARCHITECTURE.md` decision 3: a cross-table read is a
//! service, not an endpoint).
//!
//! A [`TimelineQuery`] composes four things into ONE parametrized
//! [`sqlx::QueryBuilder`] statement — user input only ever rides in as bind
//! arguments, never as text spliced into the SQL:
//!
//! - where the rows come from: [`TimelineSource`] (who authored them, or
//!   which tags they carry);
//! - what the request narrows them by: [`TimelineFilters`] (the Pleroma
//!   timeline parameters: `local_only`, `remote_only`, `only_media`,
//!   `with_muted`, `visibility` exclusions, `reply_visibility`,
//!   `exclude_replies`, `exclude_reblogs`, `instance`, `pinned_only`,
//!   `tagged`);
//! - what the viewer asked to never see again: the viewer's blocks and
//!   domain blocks (always applied) and unexpired mutes and thread mutes
//!   (applied unless `with_muted` is set);
//! - what the viewer may see at all: [`push_visibility_clause`], always
//!   applied, on top of the unconditional `deleted_at IS NULL` and the
//!   author-`is_active` check of `SELECT_SQL` (a scheduled
//!   `expires_at` is not a filter: an expired row stays readable until the
//!   sweep takes it, as everywhere else in this schema).
//!
//! Finally [`PageParams::apply_keyset`] runs on `statuses.id` (decision 4:
//! `UUIDv7` text sorts by creation, so the cursors are plain id
//! comparisons); endpoints extract the page with the default
//! [`MAX_LIMIT`](crate::pagination::MAX_LIMIT) cap of 40.
//!
//! Reference: Pleroma assembles the same layers in
//! `Pleroma.Web.ActivityPub.ActivityPub.fetch_activities/3` and
//! `fetch_public_activities/2`
//! (`lib/pleroma/web/activity_pub/activity_pub.ex`) behind the actions of
//! `lib/pleroma/web/mastodon_api/controllers/timeline_controller.ex`; the
//! filter names and defaults are Pleroma's, the ticket body is
//! authoritative.
//!
//! One deliberate absence:
//!
//! - This module does not render. [`fetch`](TimelineQuery::fetch) answers
//!   [`Status`] model rows;
//!   the timelines' endpoint helper turns them into entities with
//!   [`render_statuses`] and builds the `Link` header with
//!   [`crate::pagination::link_header`].
//!
//! [`Status`]: crate::models::status::Status
//! [`render_statuses`]: crate::services::render_status::render_statuses

use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::{
    error::AppResult,
    models::{
        status::Status,
        status_visibility::{ACTIVE_AUTHOR, push_visibility_clause},
        user::User,
    },
    pagination::PageParams,
};

mod arms;
mod filters;
mod list_arms;
mod source;
mod visibility_arms;

#[cfg(test)]
mod home_tests;
#[cfg(test)]
mod test_support;

pub use filters::TimelineFilters;
pub use source::{ReplyVisibility, TimelineSource};

use arms::push_source;
use visibility_arms::{push_block_exclusions, push_mute_exclusions};

/// The head of every [`TimelineQuery::fetch`] statement: the live rows
/// (`deleted_at IS NULL`, always applied) of `statuses`, aliased `s` for the
/// fragments [`push_visibility_clause`] and the keyset paging take.
/// [`fetch`](TimelineQuery::fetch) pushes [`ACTIVE_AUTHOR`] right after it,
/// and every clause below appends its own ` AND `.
///
/// The author check is Pleroma's `Activity.restrict_deactivated_users`
/// (`lib/pleroma/activity.ex`), which `fetch_activities` folds into every
/// timeline read (`lib/pleroma/web/activity_pub/activity_pub.ex`): a
/// deactivated account's posts leave every timeline while the flag stands,
/// and come back with it. The admin surfaces read through
/// [`crate::models::status::admin`] instead and keep seeing everything.
const SELECT_SQL: &str = "SELECT s.* FROM statuses s WHERE s.deleted_at IS NULL";

/// The author of the original a reblog row boosts, as a scalar subquery over
/// `s`: `NULL` when `s` is not a reblog, which no `= ` comparison ever
/// matches.
const REBLOG_AUTHOR_SQL: &str = "(SELECT su.user_id FROM statuses su WHERE su.id = s.reblog_of_id)";

/// The existence check for one tag on `s`: the `status_hashtags` link joined
/// to the tag's row, ready for an `h.name` comparison.
const TAG_JOIN_SQL: &str = "SELECT 1 FROM status_hashtags sh \
     JOIN hashtags h ON h.id = sh.hashtag_id WHERE sh.status_id = s.id";

/// The accepted-follow arms shared by the [`TimelineSource::Home`] predicate
/// and the [`ReplyVisibility::Following`] filter.
const ACCEPTED_FOLLOW_STATE: &str = " AND f.state = 'accepted'";

/// The single query builder behind every status timeline, composed into
/// one statement and run by [`fetch`](Self::fetch).
///
/// It carries a source of rows, the request's filters, the viewer's blocks
/// and mutes, the visibility clause and a keyset page.
#[derive(Debug)]
pub struct TimelineQuery {
    /// The account asking, or `None` for an anonymous request: it decides
    /// whose follows fill [`TimelineSource::Home`], whose replies survive
    /// [`TimelineSource::List`]'s `replies_policy`, whose visibility
    /// [`push_visibility_clause`] answers for, whose exclusions apply, and
    /// which `local` rows appear on [`TimelineSource::Public`].
    pub viewer: Option<User>,
    /// Which set of statuses the timeline answers from.
    pub source: TimelineSource,
    /// The optional narrowings of the request.
    pub filters: TimelineFilters,
    /// The keyset page, run on `statuses.id` by
    /// [`PageParams::apply_keyset`] and flipped back to newest-first by
    /// [`PageParams::finish`]; the endpoints extract it with the default
    /// 40-row cap ([`MAX_LIMIT`](crate::pagination::MAX_LIMIT)).
    pub page: PageParams,
}

impl TimelineQuery {
    /// Builds the one statement the timeline is and runs it: `SELECT s.*
    /// FROM statuses s WHERE s.deleted_at IS NULL`, then the source
    /// predicate, the filters, the block exclusions, the mute exclusions
    /// (unless `with_muted`), [`push_visibility_clause`], and the keyset page.
    /// Answers the rows newest-first (a `min_id` page read ascending comes
    /// back flipped).
    ///
    /// The only page that never reaches the database is the impossible one:
    /// [`TimelineSource::Home`], [`TimelineSource::Direct`] or
    /// [`TimelineSource::List`] for an anonymous request — there is nobody
    /// to follow, no conversation to be a party of, and no viewer whose
    /// list could answer.
    ///
    /// # Errors
    ///
    /// Any database failure (connection, encoding) arrives as
    /// [`AppError::Database`](crate::error::AppError).
    pub async fn fetch(&self, pool: &PgPool) -> AppResult<Vec<Status>> {
        if self.anonymous_impossible_page() {
            return Ok(Vec::new());
        }
        let viewer = self.viewer.as_ref();
        let mut builder = QueryBuilder::<Postgres>::new(SELECT_SQL);
        let _active: &mut _ = builder.push(ACTIVE_AUTHOR);
        push_source(&mut builder, &self.source, viewer);
        self.filters.push_clauses(&mut builder, viewer);
        push_block_exclusions(&mut builder, viewer);
        if !self.filters.with_muted.get() {
            push_mute_exclusions(&mut builder, viewer);
        }
        push_visibility_clause(&mut builder, viewer, "s");
        self.page.apply_keyset(&mut builder, "s.id");
        let rows = builder.build_query_as::<Status>().fetch_all(pool).await?;
        Ok(self.page.finish(rows))
    }

    /// Whether this is the impossible page [`fetch`](Self::fetch) answers
    /// without asking the database: a viewer-relative source behind an
    /// anonymous request.
    const fn anonymous_impossible_page(&self) -> bool {
        self.viewer.is_none()
            && matches!(
                self.source,
                TimelineSource::Home | TimelineSource::Direct | TimelineSource::List { .. }
            )
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::{
        models::status::{self, NewStatus, Visibility, soft_delete},
        services::timeline::test_support::{
            BASE, full_page, local, newest_first, page_ids, post, timeline,
        },
    };

    /// A timeline `source` as `viewer`, unfiltered, on a custom `page`.
    fn paged(viewer: Option<User>, source: TimelineSource, page: PageParams) -> TimelineQuery {
        TimelineQuery {
            viewer,
            source,
            filters: TimelineFilters::default(),
            page,
        }
    }

    /// The anonymous `Account` page of `user_id`, on `page` — the read the
    /// keyset test walks.
    async fn all_pages(pool: &PgPool, user_id: Uuid, page: PageParams) -> Vec<Uuid> {
        page_ids(
            pool,
            &paged(None, TimelineSource::Account { user_id }, page),
        )
        .await
    }

    /// A two-row page, the shape every cursor case of
    /// [`keyset_pages_walk_ids_and_min_id_flips`] asks for.
    const fn two_rows(cursor: PageCursor) -> PageParams {
        let (max_id, min_id, since_id) = match cursor {
            PageCursor::None => (None, None, None),
            PageCursor::Max(id) => (Some(id), None, None),
            PageCursor::Min(id) => (None, Some(id), None),
            PageCursor::Since(id) => (None, None, Some(id)),
        };
        PageParams {
            max_id,
            min_id,
            since_id,
            limit: 2,
            offset: None,
        }
    }

    /// Which cursor a [`two_rows`] page carries, beside no cursor at all.
    #[derive(Clone, Copy, Debug)]
    enum PageCursor {
        /// No cursor: the first page.
        None,
        /// `max_id`, reading strictly older rows.
        Max(Uuid),
        /// `min_id`, reading strictly newer rows, flipped.
        Min(Uuid),
        /// `since_id`, reading strictly newer rows, newest-first.
        Since(Uuid),
    }

    /// A tombstone is off every timeline even for its author, while a status
    /// whose `expires_at` has passed stays readable until the sweep takes it.
    #[tokio::test]
    async fn tombstones_are_out_and_expired_statuses_stay() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let live = post(&pool, &alice, Visibility::Public, "live post").await;
        let expiring = status::insert(
            &pool,
            BASE,
            &NewStatus {
                expires_at: Some(Utc::now() - Duration::hours(1)),
                ..NewStatus::local(&alice, "due for deletion, not yet swept")
            },
        )
        .await
        .expect("the expiring post inserts");
        let doomed = post(&pool, &alice, Visibility::Public, "already deleted").await;
        let gone = soft_delete(&pool, doomed.id)
            .await
            .expect("the delete runs")
            .expect("the post was live");
        assert!(gone.deleted_at.is_some(), "the row is a tombstone now");

        let own_page = page_ids(
            &pool,
            &timeline(
                Some(alice),
                TimelineSource::Account {
                    user_id: doomed.user_id,
                },
            ),
        )
        .await;
        assert_eq!(
            own_page,
            newest_first(&[&live, &expiring]),
            "the tombstone is gone, the un-swept expired row is not"
        );
    }

    /// Keyset pagination walks `statuses.id` (decision 4): a plain page is
    /// newest-first, `max_id` continues strictly older, `since_id` reads the
    /// newest rows after the cursor still newest-first, and a `min_id` page
    /// is read ascending and answered flipped.
    #[tokio::test]
    async fn keyset_pages_walk_ids_and_min_id_flips() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        for n in 0..5 {
            let _status = post(
                &pool,
                &alice,
                Visibility::Public,
                &format!("paged post {n}"),
            )
            .await;
        }
        let alice_id = alice.id;
        let all = all_pages(&pool, alice_id, full_page()).await;
        assert_eq!(all.len(), 5, "the whole page is the fixture");
        let on = |n: usize| all[n];

        let first = all_pages(&pool, alice_id, two_rows(PageCursor::None)).await;
        assert_eq!(
            first,
            vec![on(0), on(1)],
            "a plain page takes the two newest, newest first"
        );
        let second = all_pages(&pool, alice_id, two_rows(PageCursor::Max(on(1)))).await;
        assert_eq!(
            second,
            vec![on(2), on(3)],
            "max_id continues strictly older"
        );
        let tail = all_pages(&pool, alice_id, two_rows(PageCursor::Max(on(3)))).await;
        assert_eq!(tail, vec![on(4)], "the walk ends on the oldest row");
        let polling = all_pages(&pool, alice_id, two_rows(PageCursor::Since(on(3)))).await;
        assert_eq!(
            polling,
            vec![on(0), on(1)],
            "since_id reads the newer rows, still answered newest-first"
        );
        let flipped = all_pages(&pool, alice_id, two_rows(PageCursor::Min(on(3)))).await;
        assert_eq!(
            flipped,
            vec![on(1), on(2)],
            "min_id takes the page just newer than the cursor — read ascending, answered flipped"
        );
        let nothing_newer = all_pages(&pool, alice_id, two_rows(PageCursor::Min(on(0)))).await;
        assert!(
            nothing_newer.is_empty(),
            "nothing is newer than the newest row"
        );
    }
}
