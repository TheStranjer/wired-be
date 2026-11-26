//! The Mastodon admin account listing: [`mastodon_list`] over
//! [`MastodonListParams`], its clause builders and the two origin
//! fragments [`push_criterion`] weighs them against.

use sqlx::{PgPool, Postgres, QueryBuilder};

use super::super::User;
use super::criterion::{Criterion, push_criterion};
use super::pleroma_list::LIST_SQL;
use crate::{
    error::AppResult, models::user::NOT_INSTANCE_ACTOR, pagination::PageParams, util::like,
};

/// The criteria of one [`mastodon_list`] call: the seven booleans the two
/// admin account listings share and the four text criteria they read off the
/// same columns.
///
/// Every field is a criterion in its own right, so the struct is the union of
/// v1's and v2's parameter sets: v2 spells `origin=local` the `local` filter
/// and `status=pending` the `pending` one, and its reading of `permissions`
/// lands on [`staff`](Self::staff). The criteria the ticket has this instance
/// ignore (`silenced`, `sensitized`, `ip`, `invited_by`) are not
/// fields at all: their absence from the `WHERE` clause *is* the ignoring.
#[derive(Clone, Debug, Default)]
pub struct MastodonListParams {
    /// `local`: [`Only`](Criterion::Only) keeps this instance's accounts,
    /// [`Except`](Criterion::Except) the mirrors — Pleroma's `local: false`
    /// arm, which filters on `external` instead.
    pub local: Criterion,
    /// `remote` (and `origin=remote`), the same column read the other way.
    pub remote: Criterion,
    /// `active` (and `status=active`): Pleroma's full green light — active,
    /// approved and confirmed at once.
    pub active: Criterion,
    /// `pending` (and `status=pending`):
    /// [`approval_pending`](User::approval_pending) is set.
    pub pending: Criterion,
    /// `disabled` (and `status=disabled`): [`User::disabled_at`] is set,
    /// whether or not the account is also suspended (see
    /// [`suspended`](Self::suspended)); WBE-883.
    pub disabled: Criterion,
    /// `suspended` (and `status=suspended`): [`User::is_suspended`] — a local
    /// or remote suspension or a deletion under way (WBE-878).
    pub suspended: Criterion,
    /// `staff` (and `permissions=staff`): an admin or a moderator.
    pub staff: Criterion,
    /// `username`: a case-insensitive substring of the handle, Pleroma's
    /// `nickname` criterion — a mirror's stored `nick@host` carries both
    /// halves, so a bare username finds it too.
    pub username: Option<String>,
    /// `display_name`: a case-insensitive substring of the display name.
    pub display_name: Option<String>,
    /// `by_domain`: the exact host a mirror's [`domain`](User::domain) column
    /// holds.
    pub domain: Option<String>,
    /// `email`: an exact address (the citext column folds its case).
    pub email: Option<String>,
}

/// The `AND local` / `AND NOT local` pair [`mastodon_list`] weighs the two
/// origin criteria against.
const ONLY_LOCAL_SQL: &str = " AND local";
/// [`ONLY_LOCAL_SQL`]'s opposite.
const ONLY_REMOTE_SQL: &str = " AND NOT local";

/// The Mastodon admin account listing: every account `params` selects, the
/// instance actor excepted, one [`PageParams`] page of them newest first.
///
/// Unlike [`list`](super::pleroma_list::list), this one answers a bare array under a `Link` header, so
/// it counts nothing: the window is the keyset [`PageParams::apply_keyset`]
/// puts on `id` (decision 4) rather than Pleroma admin API's `page` /
/// `page_size` offset, and [`PageParams::finish`] is what makes a `min_id`
/// page come back newest-first like every other one.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn mastodon_list(
    pool: &PgPool,
    params: &MastodonListParams,
    page: PageParams,
) -> AppResult<Vec<User>> {
    let mut builder = QueryBuilder::<Postgres>::new(LIST_SQL);
    push_mastodon_where(&mut builder, params);
    page.apply_keyset(&mut builder, "id");
    let rows = builder.build_query_as::<User>().fetch_all(pool).await?;
    Ok(page.finish(rows))
}

/// Appends the `WHERE` clause [`mastodon_list`] runs: the instance-actor
/// exclusion, then the origin, state and text criteria, in that order.
pub(super) fn push_mastodon_where(
    builder: &mut QueryBuilder<Postgres>,
    params: &MastodonListParams,
) {
    let _where: &mut _ = builder.push(" WHERE true").push(NOT_INSTANCE_ACTOR);
    push_mastodon_location(builder, params);
    push_mastodon_state(builder, params);
    push_mastodon_text(builder, params);
}

/// Appends the two origin arms, both of which weigh the `local` column of
/// decision 7: `local=true` and `remote=false` want a local row, and
/// `local=false` and `remote=true` want a mirror. Sending both in the same
/// direction is one criterion stated twice; sending them opposed answers
/// nothing, which is what Pleroma's pair of `WHERE` arms does.
fn push_mastodon_location(builder: &mut QueryBuilder<Postgres>, params: &MastodonListParams) {
    push_criterion(builder, params.local, ONLY_LOCAL_SQL, ONLY_REMOTE_SQL);
    push_criterion(builder, params.remote, ONLY_REMOTE_SQL, ONLY_LOCAL_SQL);
}

/// Appends the four arms that weigh [`is_active`](User::is_active),
/// [`approval_pending`](User::approval_pending),
/// [`email_confirmed`](User::email_confirmed) and the two staff flags:
/// `active` wants all three of the first group green and `Except` wants any
/// one of them off (Pleroma's `compose_query({:active, false}, …)`), while
/// `pending`, `disabled` and `staff` each weigh one column or one pair.
fn push_mastodon_state(builder: &mut QueryBuilder<Postgres>, params: &MastodonListParams) {
    push_criterion(
        builder,
        params.active,
        " AND is_active AND disabled_at IS NULL AND NOT approval_pending AND email_confirmed",
        " AND (NOT is_active OR disabled_at IS NOT NULL OR approval_pending \
         OR NOT email_confirmed)",
    );
    push_criterion(
        builder,
        params.pending,
        " AND approval_pending",
        " AND NOT approval_pending",
    );
    push_criterion(
        builder,
        params.disabled,
        " AND disabled_at IS NOT NULL",
        " AND disabled_at IS NULL",
    );
    push_criterion(
        builder,
        params.suspended,
        " AND (suspended_at IS NOT NULL OR remote_suspended OR deleted_at IS NOT NULL)",
        " AND suspended_at IS NULL AND NOT remote_suspended AND deleted_at IS NULL",
    );
    push_criterion(
        builder,
        params.staff,
        " AND (is_admin OR is_moderator)",
        " AND NOT is_admin AND NOT is_moderator",
    );
}

/// Appends the text criteria: [`username`](MastodonListParams::username) and
/// [`display_name`](MastodonListParams::display_name) as escaped `ILIKE`
/// substrings, [`domain`](MastodonListParams::domain) as the exact host of a
/// mirror's row and [`email`](MastodonListParams::email) as an exact address,
/// citext-folded like [`push_criteria`](super::pleroma_filters::push_criteria) folds it.
fn push_mastodon_text(builder: &mut QueryBuilder<Postgres>, params: &MastodonListParams) {
    if let Some(needle) = &params.username {
        let _nick: &mut _ = builder
            .push(" AND nickname::text ILIKE ")
            .push_bind(like::contains(needle));
    }
    if let Some(needle) = &params.display_name {
        let _name: &mut _ = builder
            .push(" AND name ILIKE ")
            .push_bind(like::contains(needle));
    }
    if let Some(domain) = &params.domain {
        let _domain: &mut _ = builder.push(" AND domain = ").push_bind(domain.clone());
    }
    if let Some(address) = &params.email {
        let _email: &mut _ = builder
            .push(" AND email = ")
            .push_bind(address.clone())
            .push("::citext");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::user::admin::test_support::{
        INSTANCE_ACTOR, MastodonCase, first_page, fixture, listed_accounts, mastodon, weigh,
        weighed,
    };
    use crate::util::id::parse_id;

    /// The unfiltered Mastodon page answers the same eight accounts as
    /// Pleroma's admin listing, newest row first rather than by nickname, the
    /// instance actor still excluded.
    #[tokio::test]
    async fn the_mastodon_page_orders_newest_first_and_hides_the_actor() {
        let (_db, pool) = fixture().await;
        let accounts = mastodon(&pool, 50, |params| params).await;
        assert_eq!(
            listed_accounts(&accounts),
            vec![
                "faraway@remote.example",
                "beeper",
                "unminded",
                "newbie",
                "suspended",
                "moddy",
                "root",
                "plain",
            ],
            "the seeded rows in id order, newest first"
        );
        assert!(
            mastodon(&pool, 1, |params| params)
                .await
                .iter()
                .all(|found| found.nickname != INSTANCE_ACTOR),
            "the instance actor is no account to list"
        );
    }

    /// The two origin criteria, which weigh the one `local` column of
    /// decision 7 in both directions: `local=false` and `remote=true` are the
    /// mirrors' set, `local=true` and `remote=false` this instance's own.
    #[tokio::test]
    async fn the_mastodon_origin_criteria_split_the_table_in_two() {
        let (_db, pool) = fixture().await;
        let locals = [
            "beeper",
            "unminded",
            "newbie",
            "suspended",
            "moddy",
            "root",
            "plain",
        ];
        let mirror = ["faraway@remote.example"];
        let cases: &[MastodonCase<'_>] = &[
            ("local", weigh(|p| p.local = Criterion::Only), &locals),
            (
                "local=false",
                weigh(|p| p.local = Criterion::Except),
                &mirror,
            ),
            ("remote", weigh(|p| p.remote = Criterion::Only), &mirror),
            (
                "remote=false",
                weigh(|p| p.remote = Criterion::Except),
                &locals,
            ),
        ];
        for (name, params, expected) in cases {
            assert_eq!(
                listed_accounts(&weighed(&pool, params).await),
                expected.to_vec(),
                "the {name} criterion"
            );
        }
    }

    /// Marks the inactive fixture row as also disabled, the flag `disabled`
    /// weighs (WBE-883).
    async fn disable_inactive_row(pool: &PgPool) {
        let disabled =
            sqlx::query("UPDATE users SET disabled_at = now() WHERE nickname = $1::citext")
                .bind("suspended")
                .execute(pool)
                .await
                .expect("the disable writes")
                .rows_affected();
        assert_eq!(disabled, 1, "the inactive fixture row is also disabled");
    }

    /// The four state criteria and their negations, each against rows that
    /// differ from every other row in exactly one column: `active` wants all
    /// three green, `pending` the approval gate, `disabled` the flag off and
    /// `staff` either of the two roles.
    #[tokio::test]
    async fn the_mastodon_state_criteria_weigh_their_own_columns() {
        let (_db, pool) = fixture().await;
        disable_inactive_row(&pool).await;
        let cases: &[MastodonCase<'_>] = &[
            (
                "active",
                weigh(|p| p.active = Criterion::Only),
                &["faraway@remote.example", "beeper", "moddy", "root", "plain"],
            ),
            (
                "active=false",
                weigh(|p| p.active = Criterion::Except),
                &["unminded", "newbie", "suspended"],
            ),
            (
                "pending",
                weigh(|p| p.pending = Criterion::Only),
                &["newbie"],
            ),
            (
                "pending=false",
                weigh(|p| p.pending = Criterion::Except),
                &[
                    "faraway@remote.example",
                    "beeper",
                    "unminded",
                    "suspended",
                    "moddy",
                    "root",
                    "plain",
                ],
            ),
            (
                "disabled",
                weigh(|p| p.disabled = Criterion::Only),
                &["suspended"],
            ),
            (
                "disabled=false",
                weigh(|p| p.disabled = Criterion::Except),
                &[
                    "faraway@remote.example",
                    "beeper",
                    "unminded",
                    "newbie",
                    "moddy",
                    "root",
                    "plain",
                ],
            ),
            (
                "staff",
                weigh(|p| p.staff = Criterion::Only),
                &["moddy", "root"],
            ),
            (
                "staff=false",
                weigh(|p| p.staff = Criterion::Except),
                &[
                    "faraway@remote.example",
                    "beeper",
                    "unminded",
                    "newbie",
                    "suspended",
                    "plain",
                ],
            ),
        ];
        for (name, params, expected) in cases {
            assert_eq!(
                listed_accounts(&weighed(&pool, params).await),
                expected.to_vec(),
                "the {name} criterion"
            );
        }
    }

    /// The four text criteria read the columns the ticket names: the handle
    /// and the display name as case-insensitive substrings, the mirrored host
    /// and the address exactly — and a criterion that matches nothing answers
    /// the empty page rather than an error.
    #[tokio::test]
    async fn the_mastodon_text_criteria_read_their_own_columns() {
        let (_db, pool) = fixture().await;
        let mirror = ["faraway@remote.example"];
        let nobody: [&str; 0] = [];
        let cases: &[MastodonCase<'_>] = &[
            (
                "a username substring",
                weigh(|p| p.username = Some("MOD".to_owned())),
                &["moddy"],
            ),
            (
                "a username that rides in a handle",
                weigh(|p| p.username = Some("faraway".to_owned())),
                &mirror,
            ),
            (
                "a display name",
                weigh(|p| p.display_name = Some("person".to_owned())),
                &["plain"],
            ),
            (
                "a mirrored host",
                weigh(|p| p.domain = Some("remote.example".to_owned())),
                &mirror,
            ),
            (
                "an address in any case",
                weigh(|p| p.email = Some("ROOT@example.org".to_owned())),
                &["root"],
            ),
            (
                "a host nobody lives under",
                weigh(|p| p.domain = Some("nowhere.example".to_owned())),
                &nobody,
            ),
        ];
        for (name, params, expected) in cases {
            assert_eq!(
                listed_accounts(&weighed(&pool, params).await),
                expected.to_vec(),
                "the {name} criterion"
            );
        }
    }

    /// Two criteria narrow the set together, and the origin pair stated in
    /// opposite directions answers nothing — Pleroma's two `WHERE` arms on the
    /// one column.
    #[tokio::test]
    async fn the_mastodon_criteria_combine_with_and() {
        let (_db, pool) = fixture().await;
        let active_locals = mastodon(&pool, 50, |params| MastodonListParams {
            local: Criterion::Only,
            active: Criterion::Only,
            ..params
        })
        .await;
        assert_eq!(
            listed_accounts(&active_locals),
            vec!["beeper", "moddy", "root", "plain"],
            "the mirror is active but not local"
        );
        let impossible = mastodon(&pool, 50, |params| MastodonListParams {
            local: Criterion::Only,
            remote: Criterion::Only,
            ..params
        })
        .await;
        assert!(
            impossible.is_empty(),
            "no account is both this instance's and somebody else's"
        );
    }

    /// The cursor is the row id: `max_id` continues where a page ended,
    /// `since_id` skips everything at or before it, and a `min_id` page is
    /// read forward and answered back newest-first.
    #[tokio::test]
    async fn the_mastodon_page_walks_the_id_cursor() {
        let (_db, pool) = fixture().await;
        let first = mastodon(&pool, 3, |params| params).await;
        assert_eq!(
            listed_accounts(&first),
            vec!["faraway@remote.example", "beeper", "unminded"],
            "the three newest rows"
        );
        let oldest = first
            .last()
            .expect("the page came back full")
            .id
            .to_string();
        let next = PageParams {
            max_id: parse_id(&oldest),
            ..first_page(3)
        };
        let second = mastodon_list(&pool, &MastodonListParams::default(), next)
            .await
            .expect("the next page reads");
        assert_eq!(
            listed_accounts(&second),
            vec!["newbie", "suspended", "moddy"],
            "and the cursor moves to the page after it"
        );

        let newest = first
            .first()
            .expect("the page came back full")
            .id
            .to_string();
        let since = PageParams {
            since_id: parse_id(&newest),
            ..first_page(3)
        };
        assert!(
            mastodon_list(&pool, &MastodonListParams::default(), since)
                .await
                .expect("a cursor past the last row reads")
                .is_empty(),
            "nothing is newer than the newest account"
        );

        let flipped = PageParams {
            min_id: parse_id(&oldest),
            ..first_page(2)
        };
        let above = mastodon_list(&pool, &MastodonListParams::default(), flipped)
            .await
            .expect("a min_id page reads");
        assert_eq!(
            listed_accounts(&above),
            vec!["faraway@remote.example", "beeper"],
            "read forward from the cursor and answered back newest-first"
        );
    }
}
