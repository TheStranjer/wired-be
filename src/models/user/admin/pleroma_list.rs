//! The Pleroma admin user listing: [`ListFilters`], [`ListParams`] and
//! [`ListPage`], the paged [`list`] read beside its [`count_matching`]
//! twin, and the [`push_where`] head both statements share.

use sqlx::{PgPool, Postgres, QueryBuilder};

use super::super::{Flag, User};
use super::pleroma_filters::{push_criteria, push_filters, push_page};
use crate::{error::AppResult, models::user::NOT_INSTANCE_ACTOR};

/// The head of [`list`]'s and
/// [`mastodon_list`](super::mastodon::mastodon_list)'s page statements;
/// [`push_where`] and
/// [`push_mastodon_where`](super::mastodon::push_mastodon_where) append the
/// `WHERE` clause each listing runs,
/// [`push_page`](super::pleroma_filters::push_page) and
/// [`PageParams::apply_keyset`](crate::pagination::PageParams::apply_keyset)
/// the order and the window.
pub(super) const LIST_SQL: &str = "SELECT * FROM users";

/// The head of [`count_matching`]: the same rows, counted without the
/// window (Pleroma's `Repo.aggregate(query, :count, :id)`).
const COUNT_SQL: &str = "SELECT count(*) FROM users";

/// The boolean criteria of Pleroma's admin user listing: the eight names of
/// its `@filters` list, each set when the request's comma list carried it.
///
/// Every one of them narrows the same set — Pleroma's reduce over the
/// filters map adds one `WHERE` arm per name, so `local,deactivated` is a
/// deactivated local account and `local,external` answers nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListFilters {
    /// `local`: accounts this instance owns.
    pub local: Flag,
    /// `external`: remote mirrors.
    pub external: Flag,
    /// `active`: Pleroma's full green light — active, approved, confirmed.
    pub active: Flag,
    /// `deactivated`: [`is_active`](super::super::User::is_active) is false.
    pub deactivated: Flag,
    /// `need_approval`: [`approval_pending`](super::super::User::approval_pending)
    /// is set, so staff still have to approve the registration.
    pub need_approval: Flag,
    /// `unconfirmed`: [`email_confirmed`](super::super::User::email_confirmed) is
    /// false.
    pub unconfirmed: Flag,
    /// `is_admin`: the account holds the admin role.
    pub is_admin: Flag,
    /// `is_moderator`: the account holds the moderator role.
    pub is_moderator: Flag,
}

impl ListFilters {
    /// The filters of one request, from the names its comma-separated
    /// `filters` parameter carries: [`local`](Self::local) and its seven
    /// siblings, matched exactly — Pleroma's `maybe_parse_filters` splits
    /// on `,` and keeps only the names on its list, so an unknown, an empty
    /// or a spaced spelling flips nothing. A missing parameter is the all
    /// unset default.
    #[must_use]
    pub fn parse(text: Option<&str>) -> Self {
        let mut found = Self::default();
        for name in text.unwrap_or_default().split(',') {
            found.set(name);
        }
        found
    }

    /// Flips the one filter `name` names, leaving the rest as they were.
    fn set(&mut self, name: &str) {
        let selected = Flag::new(true);
        match name {
            "local" => self.local = selected,
            "external" => self.external = selected,
            "active" => self.active = selected,
            "deactivated" => self.deactivated = selected,
            "need_approval" => self.need_approval = selected,
            "unconfirmed" => self.unconfirmed = selected,
            "is_admin" => self.is_admin = selected,
            "is_moderator" => self.is_moderator = selected,
            _ => {}
        }
    }
}

/// The criteria of one [`list`] call: Pleroma's `search_params` as this
/// schema can answer them, plus the page window.
///
/// Each optional text criterion is `None` when the request carried neither
/// the key nor a value worth filtering on, and each list is empty by
/// default; an empty criterion narrows nothing, the way
/// `User.Query.compose_query` guards `not_empty_string` and ignores an
/// empty list.
#[derive(Clone, Debug, Default)]
pub struct ListParams {
    /// Case-insensitive substring of nickname, display name or email.
    pub query: Option<String>,
    /// Case-insensitive substring of the display name.
    pub name: Option<String>,
    /// Exact email (the citext column folds its case, as Pleroma's equality
    /// criterion on its own citext column does).
    pub email: Option<String>,
    /// Profile tags to overlap with: a row matches when it carries any one
    /// of them (Pleroma's `tags && ^tags`).
    pub tags: Vec<String>,
    /// Actor types to keep — `Person`, `Service`, ... — matched exactly.
    pub actor_types: Vec<String>,
    /// The boolean criteria of the `filters` parameter.
    pub filters: ListFilters,
    /// 1-based page number; [`list`] treats anything below the first page as
    /// the first page.
    pub page: i64,
    /// Rows in the page. The endpoint owns the default and the cap
    /// (Pleroma's `@users_page_size` of 50 under the ticket's 200 ceiling);
    /// [`list`] never hands a negative bound to Postgres.
    pub page_size: i64,
}

/// One [`list`] answer: the page's rows and the total the same criteria
/// select across every page.
#[derive(Debug)]
pub struct ListPage {
    /// The page, in [`list`]'s nickname order.
    pub users: Vec<User>,
    /// How many rows the criteria match, pages included — the `count` of
    /// the endpoint's answer.
    pub count: i64,
}

/// The admin listing: every account `params` selects, the instance actor
/// excepted, one page at a time beside the whole total.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool, params: &ListParams) -> AppResult<ListPage> {
    let count = count_matching(pool, params).await?;
    let mut builder = QueryBuilder::<Postgres>::new(LIST_SQL);
    push_where(&mut builder, params);
    push_page(&mut builder, params);
    let users = builder.build_query_as::<User>().fetch_all(pool).await?;
    Ok(ListPage { users, count })
}

/// The number of rows `params`' criteria select, whatever page they land
/// on.
async fn count_matching(pool: &PgPool, params: &ListParams) -> AppResult<i64> {
    let mut builder = QueryBuilder::<Postgres>::new(COUNT_SQL);
    push_where(&mut builder, params);
    let total = builder.build_query_scalar::<i64>().fetch_one(pool).await?;
    Ok(total)
}

/// Appends the `WHERE` clause [`list`] and [`count_matching`] share: an
/// always-true predicate leads (the way [`crate::models::moderation_log`]
/// arranges it) so every criterion is one more `AND`, then the instance-actor
/// exclusion, the boolean filters and the text criteria.
fn push_where(builder: &mut QueryBuilder<Postgres>, params: &ListParams) {
    let _where: &mut _ = builder.push(" WHERE true").push(NOT_INSTANCE_ACTOR);
    push_filters(builder, params.filters);
    push_criteria(builder, params);
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::user::admin::test_support::{everything, filtered, fixture, listed};

    /// The criterion-free page lists every account but the instance actor,
    /// in nickname order, with the same eight as its total.
    #[tokio::test]
    async fn the_unfiltered_page_orders_by_nickname_and_hides_the_actor() {
        let (_db, pool) = fixture().await;
        let page = list(&pool, &everything(50))
            .await
            .expect("the listing reads");
        assert_eq!(
            listed(&page),
            vec![
                "beeper",
                "faraway@remote.example",
                "moddy",
                "newbie",
                "plain",
                "root",
                "suspended",
                "unminded",
            ],
            "eight accounts, `internal.fetch` excluded"
        );
        assert_eq!(page.count, 8, "the total is the same eight");
    }

    /// Each of the eight filters selects exactly its own accounts, weighed
    /// against rows that differ from every other row in exactly one state.
    #[tokio::test]
    async fn every_filter_selects_its_own_rows() {
        let (_db, pool) = fixture().await;
        let cases: &[(&str, &[&str])] = &[
            (
                "local",
                &[
                    "beeper",
                    "moddy",
                    "newbie",
                    "plain",
                    "root",
                    "suspended",
                    "unminded",
                ],
            ),
            ("external", &["faraway@remote.example"]),
            (
                "active",
                &["beeper", "faraway@remote.example", "moddy", "plain", "root"],
            ),
            ("deactivated", &["suspended"]),
            ("need_approval", &["newbie"]),
            ("unconfirmed", &["unminded"]),
            ("is_admin", &["root"]),
            ("is_moderator", &["moddy"]),
        ];
        for (name, expected) in cases {
            let page = list(&pool, &filtered(name)).await.expect("a filter reads");
            assert_eq!(listed(&page), expected.to_vec(), "the {name} filter");
            assert_eq!(
                page.count,
                i64::try_from(expected.len()).expect("a small count"),
                "the {name} total"
            );
        }
    }

    /// Two filters narrow the set together, and a pair no row satisfies
    /// answers nothing with a zero total: `count` runs the same `WHERE`
    /// clause as the page.
    #[tokio::test]
    async fn filters_combine_with_and() {
        let (_db, pool) = fixture().await;
        let both = list(&pool, &filtered("external,active"))
            .await
            .expect("a filter pair reads");
        assert_eq!(
            listed(&both),
            vec!["faraway@remote.example"],
            "the mirror is active too"
        );
        assert_eq!(both.count, 1, "and counts as one");
        let impossible = list(&pool, &filtered("local,external"))
            .await
            .expect("a filter pair reads");
        assert!(
            impossible.users.is_empty(),
            "no account is both local and remote"
        );
        assert_eq!(impossible.count, 0, "and the total says so");
    }

    /// The window moves while the total stays: page 2 answers the next
    /// `page_size` nicknames, a page past the last row answers nothing, and
    /// `count` is the whole set on every page.
    #[tokio::test]
    async fn the_page_window_moves_while_the_total_stays() {
        let (_db, pool) = fixture().await;
        let first = list(&pool, &everything(3))
            .await
            .expect("the first page reads");
        assert_eq!(
            listed(&first),
            vec!["beeper", "faraway@remote.example", "moddy"],
            "the first three nicknames"
        );
        assert_eq!(first.count, 8, "the total counts every page");
        let second = ListParams {
            page: 2,
            ..everything(3)
        };
        let next = list(&pool, &second).await.expect("a page reads");
        assert_eq!(
            listed(&next),
            vec!["newbie", "plain", "root"],
            "the next three"
        );
        assert_eq!(next.count, 8, "and the total is untouched by the window");
        let last = ListParams {
            page: 3,
            ..everything(3)
        };
        assert_eq!(
            listed(&list(&pool, &last).await.expect("a page reads")),
            vec!["suspended", "unminded"],
            "the third page is the short one"
        );
        let past = ListParams {
            page: 9,
            ..everything(3)
        };
        assert!(
            list(&pool, &past)
                .await
                .expect("a page past the end reads")
                .users
                .is_empty(),
            "and a page past the last row answers nothing"
        );
    }

    /// The filter parser: a known name flips its own flag; absent, empty,
    /// unknown, spaced and wrongly-cased spellings flip nothing; and every
    /// name on Pleroma's `@filters` list is a flag of [`ListFilters`].
    #[test]
    fn the_filter_parser_keeps_only_known_names() {
        assert_eq!(
            ListFilters::parse(None),
            ListFilters::default(),
            "an absent parameter selects nothing"
        );
        assert_eq!(
            ListFilters::parse(Some("")),
            ListFilters::default(),
            "so does an empty one"
        );
        let one = ListFilters::parse(Some("is_moderator"));
        assert!(
            one.is_moderator.get() && !one.is_admin.get(),
            "one name flips one flag"
        );
        let junk = ListFilters::parse(Some("local,ghost, LOCAL,,need_approval "));
        assert_eq!(
            junk,
            ListFilters {
                local: Flag::new(true),
                ..ListFilters::default()
            },
            "an unknown, a spaced, an empty and a wrongly-cased name add nothing"
        );
        let all = ListFilters::parse(Some(
            "local,external,active,deactivated,need_approval,unconfirmed,is_admin,is_moderator",
        ));
        for flag in [
            all.local,
            all.external,
            all.active,
            all.deactivated,
            all.need_approval,
            all.unconfirmed,
            all.is_admin,
            all.is_moderator,
        ] {
            assert!(flag.get(), "every name on the list flips its flag");
        }
    }
}
