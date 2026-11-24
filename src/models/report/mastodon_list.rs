//! The Mastodon admin listing's walk: [`mastodon_list`] over the
//! `resolved` state pair and the two account filters, keyed like [`list`] on
//! the report's own id.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    pagination::{PageParams, keyset_page},
};

use super::Report;

/// [`mastodon_list`]: the start of the Mastodon admin listing's page
/// statement. Its `WHERE true` is where [`push_mastodon_filters`] hangs the
/// state and account filters and [`PageParams::apply_keyset`] the cursors,
/// the newest-first order and the window — the same keyset walk over the
/// report's own id (decision 4) that [`list`](super::list) takes, under the
/// admin listing's filters.
const MASTODON_SELECT_SQL: &str = "SELECT * FROM reports WHERE true";

/// The filters of one [`mastodon_list`] read, as
/// `GET /api/v1/admin/reports` parses them.
///
/// These are Mastodon's admin-report parameters spelled the way the ticket
/// pins them: `resolved`, `account_id` and `target_account_id`.
#[derive(Debug)]
pub struct MastodonListParams {
    /// The `resolved` filter: `Some(true)` keeps the acted-on states
    /// (`resolved` *and* `closed` — the entity's `action_taken` is this same
    /// pair read from the state), and `None` or `Some(false)` keeps the open
    /// queue, so an unfiltered request lists what still needs action.
    pub resolved: Option<bool>,
    /// Only reports filed by this account (Mastodon's `account_id` names the
    /// reporter); `None` is every reporter.
    pub reporter_id: Option<Uuid>,
    /// Only reports about this account (Mastodon's `target_account_id`);
    /// `None` is every target.
    pub target_id: Option<Uuid>,
}

/// Every report `params`' filters select, newest filing first and `page`'s
/// `limit` at a time — the walk behind `GET /api/v1/admin/reports`.
///
/// The keyset is [`list`](super::list)'s, on the report row's own id (decision 4), which
/// is what the Mastodon admin listing's `Link` header keys its cursors on;
/// the filters are the ticket's reading of Mastodon's
/// `GET /api/v1/admin/reports` parameters:
///
/// - `resolved` is a state pair, not a state: `Some(true)` keeps every
///   acted-on report (`resolved` and `closed` — the entity's `action_taken`
///   is that same pair read off the state), and `Some(false)` and `None`
///   keep the open queue. An unfiltered request therefore lists what still
///   needs action, which is Pleroma's `restrict_state` fall-through too.
/// - `account_id` names the reporter, `target_account_id` the reported:
///   both are this schema's `UUIDv7` text, validated by the caller.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mastodon_list(
    pool: &PgPool,
    params: &MastodonListParams,
    page: PageParams,
) -> AppResult<Vec<Report>> {
    let mut builder = QueryBuilder::<Postgres>::new(MASTODON_SELECT_SQL);
    push_mastodon_filters(&mut builder, params);
    keyset_page(pool, builder, "id", &page).await
}

/// Appends [`mastodon_list`]'s `WHERE` clause to the always-true predicate
/// the statement leads with: the state filter as the pair or its complement
/// (never bare equality, since the acted-on set is two states), and the two
/// account filters on their columns.
fn push_mastodon_filters(builder: &mut QueryBuilder<Postgres>, params: &MastodonListParams) {
    if params.resolved == Some(true) {
        let _acted: &mut _ = builder.push(" AND state IN ('resolved', 'closed')");
    } else {
        let _open: &mut _ = builder.push(" AND state = 'open'");
    }
    if let Some(reporter_id) = params.reporter_id {
        let _reporter: &mut _ = builder.push(" AND reporter_id = ").push_bind(reporter_id);
    }
    if let Some(target_id) = params.target_id {
        let _target: &mut _ = builder.push(" AND target_id = ").push_bind(target_id);
    }
}

#[cfg(test)]
mod tests {
    use crate::models::report::{ReportState, set_state, test_support::*};

    use super::*;

    /// The [`mastodon_list`] params with nothing but the state filter set:
    /// the account filters are the tests' to add.
    fn mastodon_listing(resolved: Option<bool>) -> MastodonListParams {
        MastodonListParams {
            resolved,
            reporter_id: None,
            target_id: None,
        }
    }

    /// The report ids [`mastodon_list`] answers for `params` on `page`, in
    /// order.
    async fn mastodon_ids(
        pool: &PgPool,
        params: &MastodonListParams,
        page: PageParams,
    ) -> Vec<Uuid> {
        ids_of(
            &mastodon_list(pool, params, page)
                .await
                .expect("the listing runs"),
        )
    }

    /// The four filings the [`mastodon_list`] tests filter and page: every
    /// state and both accounts represented, newest filing first as the
    /// listing answers them — carol's closed one about alice, alice's
    /// resolved one about carol (acted on by root), bob's open one about
    /// carol, and the oldest filing, alice's open one about bob.
    async fn mastodon_seed(pool: &PgPool) -> (Report, Report, Report, Report) {
        let alice = local_user(pool, "alice").await;
        let bob = local_user(pool, "bob").await;
        let carol = local_user(pool, "carol").await;
        let root = local_user(pool, "root").await;
        let open_by_alice = seed(pool, &alice, &bob).await;
        let open_by_bob = seed(pool, &bob, &carol).await;
        let resolved = seed(pool, &alice, &carol).await;
        let _acted = set_state(pool, resolved.id, ReportState::Resolved, Some(root.id))
            .await
            .unwrap()
            .expect("the report exists");
        let closed = seed(pool, &carol, &alice).await;
        let _moved = set_state(pool, closed.id, ReportState::Closed, None)
            .await
            .unwrap()
            .expect("the report exists");
        (closed, resolved, open_by_bob, open_by_alice)
    }

    /// [`mastodon_list`]'s state filter is the ticket's pair, not a state:
    /// `resolved=true` keeps both acted-on states, and `resolved=false`,
    /// its absence and every junk value keep the open queue.
    #[tokio::test]
    async fn the_mastodon_admin_listing_selects_the_resolved_pair() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (closed, resolved, open_by_bob, open_by_alice) = mastodon_seed(&pool).await;

        let open = mastodon_ids(&pool, &mastodon_listing(None), page("")).await;
        assert_eq!(
            open,
            vec![open_by_bob.id, open_by_alice.id],
            "the unfiltered queue is the open pair, newest first"
        );
        let explicit_open = mastodon_ids(&pool, &mastodon_listing(Some(false)), page("")).await;
        assert_eq!(
            explicit_open, open,
            "resolved=false is the same queue as absence"
        );
        let acted = mastodon_ids(&pool, &mastodon_listing(Some(true)), page("")).await;
        assert_eq!(
            acted,
            vec![closed.id, resolved.id],
            "resolved=true keeps both acted-on states"
        );
    }

    /// The two account filters keep their own reporter's and their own
    /// target's filings, they stack with the state filter, an id that
    /// names no account matches no row, and the keyset pages the filtered
    /// walk rather than the whole table.
    #[tokio::test]
    async fn the_mastodon_admin_listing_filters_accounts_and_pages() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (closed, resolved, open_by_bob, open_by_alice) = mastodon_seed(&pool).await;
        let alice = open_by_alice.reporter_id;
        let carol = open_by_bob.target_id;

        let by_reporter = MastodonListParams {
            reporter_id: Some(alice),
            ..mastodon_listing(None)
        };
        assert_eq!(
            mastodon_ids(&pool, &by_reporter, page("")).await,
            vec![open_by_alice.id],
            "the reporter filter keeps alice's open filing, not her resolved one"
        );
        let acted_by_alice = MastodonListParams {
            reporter_id: Some(alice),
            ..mastodon_listing(Some(true))
        };
        assert_eq!(
            mastodon_ids(&pool, &acted_by_alice, page("")).await,
            vec![resolved.id],
            "the two filters stack"
        );
        let by_target = MastodonListParams {
            target_id: Some(carol),
            ..mastodon_listing(None)
        };
        assert_eq!(
            mastodon_ids(&pool, &by_target, page("")).await,
            vec![open_by_bob.id],
            "the target filter keeps reports about that account"
        );
        let nobody = MastodonListParams {
            target_id: Some(Uuid::now_v7()),
            ..mastodon_listing(None)
        };
        assert!(
            mastodon_ids(&pool, &nobody, page("")).await.is_empty(),
            "an id that names no account matches no row"
        );

        let paged = mastodon_ids(&pool, &mastodon_listing(Some(true)), page("limit=1")).await;
        assert_eq!(paged, vec![closed.id], "the window takes the newest match");
        let next = mastodon_ids(
            &pool,
            &mastodon_listing(Some(true)),
            page(&format!("limit=1&max_id={}", closed.id)),
        )
        .await;
        assert_eq!(
            next,
            vec![resolved.id],
            "and the cursor continues the filtered walk"
        );
    }
}
