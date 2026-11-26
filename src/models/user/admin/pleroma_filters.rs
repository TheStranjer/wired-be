//! The `WHERE` arms and the page window of the Pleroma admin listing:
//! the boolean filter arms grouped by what they weigh, the text
//! criteria, the `ILIKE` pattern they bind and the `LIMIT`/`OFFSET`
//! window.

use sqlx::{Postgres, QueryBuilder};

use super::pleroma_list::{ListFilters, ListParams};
use crate::util::like;

/// Appends the `WHERE` arms of [`ListFilters`], grouped by what they weigh:
/// origin, account state, roles.
pub(super) fn push_filters(builder: &mut QueryBuilder<Postgres>, filters: ListFilters) {
    push_location(builder, filters);
    push_state(builder, filters);
    push_roles(builder, filters);
}

/// The `local` / `external` arms: Pleroma's `location_query` over the
/// `local` column of decision 7.
fn push_location(builder: &mut QueryBuilder<Postgres>, filters: ListFilters) {
    if filters.local.get() {
        let _local: &mut _ = builder.push(" AND local");
    }
    if filters.external.get() {
        let _external: &mut _ = builder.push(" AND NOT local");
    }
}

/// The arms that weigh [`is_active`](super::super::User::is_active),
/// [`approval_pending`](super::super::User::approval_pending) and
/// [`email_confirmed`](super::super::User::email_confirmed): Pleroma's `active`
/// wants all three green — the mirror's as much as a local account's —
/// `deactivated` a false `is_active`, `need_approval` a pending approval and
/// `unconfirmed` an address nobody confirmed.
fn push_state(builder: &mut QueryBuilder<Postgres>, filters: ListFilters) {
    if filters.active.get() {
        let _active: &mut _ = builder.push(" AND is_active AND disabled_at IS NULL");
        let _green: &mut _ = builder.push(" AND NOT approval_pending AND email_confirmed");
    }
    if filters.deactivated.get() {
        let _deactivated: &mut _ = builder.push(" AND (NOT is_active OR disabled_at IS NOT NULL)");
    }
    if filters.need_approval.get() {
        let _pending: &mut _ = builder.push(" AND approval_pending");
    }
    if filters.unconfirmed.get() {
        let _unconfirmed: &mut _ = builder.push(" AND NOT email_confirmed");
    }
}

/// The role arms: Pleroma's `is_admin` / `is_moderator` criteria, each an
/// equality on `true`.
fn push_roles(builder: &mut QueryBuilder<Postgres>, filters: ListFilters) {
    if filters.is_admin.get() {
        let _admin: &mut _ = builder.push(" AND is_admin");
    }
    if filters.is_moderator.get() {
        let _moderator: &mut _ = builder.push(" AND is_moderator");
    }
}

/// Appends the text and list criteria: [`query`](ListParams::query) over the
/// three identity columns, [`name`](ListParams::name),
/// [`email`](ListParams::email), the [`tags`](ListParams::tags) overlap and
/// the [`actor_types`](ListParams::actor_types) membership.
pub(super) fn push_criteria(builder: &mut QueryBuilder<Postgres>, params: &ListParams) {
    if let Some(needle) = &params.query {
        let pattern = like::contains(needle);
        let _three: &mut _ = builder
            .push(" AND (nickname::text ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR name ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR email::text ILIKE ")
            .push_bind(pattern)
            .push(")");
    }
    if let Some(needle) = &params.name {
        let _name: &mut _ = builder
            .push(" AND name ILIKE ")
            .push_bind(like::contains(needle));
    }
    if let Some(address) = &params.email {
        let _email: &mut _ = builder
            .push(" AND email = ")
            .push_bind(address.clone())
            .push("::citext");
    }
    if !params.tags.is_empty() {
        let _tags: &mut _ = builder.push(" AND tags && ").push_bind(params.tags.clone());
    }
    if !params.actor_types.is_empty() {
        let _types: &mut _ = builder
            .push(" AND actor_type = ANY(")
            .push_bind(params.actor_types.clone())
            .push(")");
    }
}

/// Appends the order and the window: the nickname order the ticket pins
/// (citext, so case-insensitive ascending) and Pleroma's
/// `User.Query.paginate` window, whose bounds come from [`page_window`] so
/// neither can reach Postgres negative.
pub(super) fn push_page(builder: &mut QueryBuilder<Postgres>, params: &ListParams) {
    let (size, offset) = page_window(params);
    let _window: &mut _ = builder
        .push(" ORDER BY nickname LIMIT ")
        .push_bind(size)
        .push(" OFFSET ")
        .push_bind(offset);
}

/// The `LIMIT` and `OFFSET` of one page: `page_size` rows, `page_size` of
/// them skipped for each page before the requested one. A page below the
/// first starts at the first row; a negative size lists nothing rather than
/// failing the statement.
fn page_window(params: &ListParams) -> (i64, i64) {
    let size = params.page_size.max(0);
    (size, (params.page - 1).max(0).saturating_mul(size))
}

#[cfg(test)]
mod tests {
    use super::super::pleroma_list::{ListParams, list};
    use super::*;

    use crate::models::user::admin::test_support::{everything, fixture, listed};

    /// `query` is a case-insensitive substring of nickname, display name or
    /// email — the three columns the ticket names — while `name` is the
    /// display name only and `email` an exact address. The registration
    /// reason is none of those three, so it is not searched.
    #[tokio::test]
    async fn the_text_criteria_search_the_columns_the_ticket_names() {
        let (_db, pool) = fixture().await;
        let cases: &[(&str, &str)] = &[
            ("far", "faraway@remote.example"),
            ("PLAIN", "plain"),
            ("person", "plain"),
            ("ROOT@EXAMPLE.ORG", "root"),
            ("BEEPER", "beeper"),
        ];
        for (needle, expected) in cases {
            let params = ListParams {
                query: Some((*needle).to_owned()),
                ..everything(50)
            };
            let page = list(&pool, &params).await.expect("a query reads");
            assert_eq!(listed(&page), vec![*expected], "the {needle} substring");
            assert_eq!(page.count, 1, "the {needle} total");
        }
        let reason = ListParams {
            query: Some("please let me in".to_owned()),
            ..everything(50)
        };
        assert_eq!(
            list(&pool, &reason).await.expect("a query reads").count,
            0,
            "the registration reason is no search column"
        );
        let name = ListParams {
            name: Some("far".to_owned()),
            ..everything(50)
        };
        assert_eq!(
            listed(&list(&pool, &name).await.expect("a name reads")),
            vec!["faraway@remote.example"],
            "the display name holds the only `far`"
        );
        let address = ListParams {
            email: Some("ROOT@example.org".to_owned()),
            ..everything(50)
        };
        assert_eq!(
            listed(&list(&pool, &address).await.expect("an email reads")),
            vec!["root"],
            "citext folds the address's case"
        );
        let partial = ListParams {
            email: Some("root@example".to_owned()),
            ..everything(50)
        };
        assert!(
            list(&pool, &partial)
                .await
                .expect("an email reads")
                .users
                .is_empty(),
            "but `email` is no substring match"
        );
    }

    /// A `query` made of `ILIKE` metacharacters matches them literally, and
    /// a needle nothing carries answers the empty page with a zero total.
    #[tokio::test]
    async fn the_query_escapes_wildcards_and_can_miss() {
        let (_db, pool) = fixture().await;
        let wildcards = ListParams {
            query: Some("%a%".to_owned()),
            ..everything(50)
        };
        assert!(
            list(&pool, &wildcards)
                .await
                .expect("a query reads")
                .users
                .is_empty(),
            "a percent in the needle is a literal percent"
        );
        let miss = ListParams {
            query: Some("nobody".to_owned()),
            ..everything(50)
        };
        assert_eq!(
            list(&pool, &miss).await.expect("a query reads").count,
            0,
            "an unmatched needle counts nothing"
        );
    }

    /// `tags` overlaps the profile tags and `actor_types` keeps the actor
    /// types it names; a list that names nothing selects nothing, and an
    /// empty list is no criterion at all.
    #[tokio::test]
    async fn tags_and_actor_types_select_by_membership() {
        let (_db, pool) = fixture().await;
        let by_tag = ListParams {
            tags: vec!["bots".to_owned(), "staff".to_owned()],
            ..everything(50)
        };
        assert_eq!(
            listed(&list(&pool, &by_tag).await.expect("a tag reads")),
            vec!["beeper"],
            "one of two tags is enough"
        );
        let by_type = ListParams {
            actor_types: vec!["Service".to_owned(), "Application".to_owned()],
            ..everything(50)
        };
        assert_eq!(
            listed(&list(&pool, &by_type).await.expect("a type reads")),
            vec!["beeper"],
            "one of two types is enough"
        );
        let unknown = ListParams {
            actor_types: vec!["Group".to_owned()],
            tags: vec!["nobody".to_owned()],
            ..everything(50)
        };
        assert_eq!(
            list(&pool, &unknown)
                .await
                .expect("a criterion reads")
                .count,
            0,
            "both lists are criteria, and both miss"
        );
    }

    /// An unusable window cannot reach Postgres as a syntax error: a page
    /// below the first is the first page and a negative size lists nothing,
    /// while the total still counts.
    #[tokio::test]
    async fn an_unusable_window_is_clamped_not_rejected() {
        let (_db, pool) = fixture().await;
        let params = ListParams {
            page: 0,
            page_size: -5,
            ..ListParams::default()
        };
        let page = list(&pool, &params).await.expect("a clamped window reads");
        assert!(page.users.is_empty(), "a negative size lists nothing");
        assert_eq!(page.count, 8, "and the count is untouched");
        assert_eq!(page_window(&params), (0, 0), "both bounds fold to zero");
        assert_eq!(
            page_window(&ListParams {
                page: -3,
                page_size: 10,
                ..ListParams::default()
            }),
            (10, 0),
            "a page before the first is the first page"
        );
        assert_eq!(
            page_window(&ListParams {
                page: 4,
                page_size: 25,
                ..ListParams::default()
            }),
            (25, 75),
            "an ordinary page is Pleroma's offset"
        );
    }
}
