//! The admin API's reads and writes over `statuses` (WBE-201).
//!
//! Three listings share one statement here, because Pleroma runs all three
//! through the same `ActivityPub.fetch_statuses/2` with a different filter
//! set: the whole-network ledger behind
//! `GET /api/v1/pleroma/admin/statuses` (`AdminAPI.StatusController.index`,
//! `lib/pleroma/web/admin_api/controllers/status_controller.ex`), one
//! account's posts behind `GET …/users/{nickname}/statuses`
//! (`AdminAPIController.list_user_statuses`) and one domain's behind
//! `GET …/instances/{instance}/statuses` (`AdminAPI.InstanceController.list_statuses`).
//! [`list`] answers a page of them and [`count`] the whole set the same
//! filters select, which is the `{activities, total}` envelope the two
//! narrower routes carry.
//!
//! The filters are Pleroma's, restated over decision 6's relational rows:
//!
//! - `godmode` is the visibility question. Off, the listing is what an
//!   anonymous reader may see — `public` and `unlisted` only; on, it is
//!   every stored visibility, `private`, `direct` and Pleroma's `local`
//!   included (`PUBLIC_ARMS` against `ALL_ARMS`). Pleroma reaches the
//!   same pair through `Filters.visible(nil, opts)`, whose `godmode: true`
//!   arm lifts the scope list to all five.
//! - `local_only` keeps the statuses whose author this instance owns
//!   (`users.local`), the same distinction
//!   [`super::status_visibility_counts`] draws between its own counters and
//!   one domain's.
//! - `with_reblogs` decides whether boost rows are listed at all: off is
//!   Pleroma's `exclude_reblogs: true`, and one fewer row per boosted post
//!   in the ledger (`LIVE` holds the arms [`list`] always applies).
//! - the author and the domain narrow one listing to one account's posts
//!   or to one host's mirrors; a domain no account came from simply
//!   selects nothing.
//!
//! Pages are offset windows — `page` from 1, `page_size` rows — because
//! that is what the admin API's clients ask for, newest post first on the
//! `UUIDv7` id (decision 4) rather than the keyset cursors the client
//! timelines page by.
//!
//! The one writer is [`set_scope`]: the admin's change of who may see a
//! post and whether it starts folded — Pleroma's
//! `CommonAPI.update_activity_scope` (`toggle_sensitive` then
//! `set_visibility`), which rewrites exactly those two fields of the object
//! and nothing else. Unlike [`crate::models::status_edit`]'s edit write it
//! records no revision and does not move `edited_at`: a scope change is not
//! a change of what was said.

use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        status::{Status, Visibility},
        user::Flag,
    },
};

/// The head of [`list`]'s statement: every status joined to its author,
/// which is the row `local_only` and the domain filter weigh.
const LIST_SQL: &str = "SELECT s.* FROM statuses s JOIN users u ON u.id = s.user_id";

/// The head of [`count`]'s statement: the same rows, counted.
const COUNT_SQL: &str = "SELECT count(*) FROM statuses s JOIN users u ON u.id = s.user_id";

/// The arms every admin listing applies: a tombstone is nobody's ledger
/// entry, on either side of `godmode`.
const LIVE: &str = " WHERE s.deleted_at IS NULL";

/// The visibility arms of a listing without `godmode` — what an anonymous
/// reader may see, which is the audience Pleroma's `fetch_statuses(nil, …)`
/// states for a staff listing that has not asked for more.
const PUBLIC_ARMS: &str = " AND s.visibility IN ('public', 'unlisted')";

/// The visibility arms of a `godmode` listing: all five stored visibilities,
/// stated as the `CHECK` constraint lists them. Pleroma's
/// `Constants.visibility_scopes/1` for the same case.
const ALL_ARMS: &str = " AND s.visibility IN ('public', 'unlisted', 'private', 'direct', 'local')";

/// The arm that drops boost rows, which a listing without `with_reblogs`
/// never shows (Pleroma's `exclude_reblogs: true`).
const NO_REBLOGS: &str = " AND s.reblog_of_id IS NULL";

/// The arm `local_only` adds: the author is this instance's own account.
const LOCAL_AUTHORS: &str = " AND u.local";

/// The filters and window of one [`list`] (or [`count`]) read, as the three
/// admin status listings parse them (Pleroma's `StatusController.index`,
/// `list_user_statuses` and `InstanceController.list_statuses` params).
///
/// The three filters are [`Flag`]s for the reason [`TimelineFilters`] gives
/// its own: a params struct of bare booleans is what Clippy's denied
/// `struct_excessive_bools` refuses.
///
/// [`Flag`]: crate::models::user::Flag
/// [`TimelineFilters`]: crate::services::timeline::TimelineFilters
#[derive(Debug)]
pub struct ListParams {
    /// One-based page number; the caller defaults and validates it (1).
    pub page: i64,
    /// Rows per page; the caller defaults and caps it (50, max 200).
    pub page_size: i64,
    /// Whether the private visibilities are listed: off is `public`
    /// and `unlisted` only.
    pub godmode: Flag,
    /// Whether only statuses of local accounts are listed.
    pub local_only: Flag,
    /// Whether boost rows are listed beside the posts themselves.
    pub with_reblogs: Flag,
    /// Only this account's statuses; `None` is every author.
    pub user_id: Option<Uuid>,
    /// Only statuses of accounts mirrored from this domain; `None` is every
    /// host. The `local` literal of `GET /instances/{instance}/statuses` is
    /// the caller's `local_only`, which is why this is a plain domain.
    pub domain: Option<String>,
}

/// One admin status page: `params`' window of the ledger, newest first.
///
/// The window is the whole answer of the network-wide listing; the two
/// narrower listings put it beside [`count`]'s total, which is the
/// `{activities, total}` envelope Pleroma's `AdminAPI.StatusView` renders
/// for them.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list(pool: &PgPool, params: &ListParams) -> AppResult<Vec<Status>> {
    let mut builder = QueryBuilder::<Postgres>::new(LIST_SQL);
    push_filters(&mut builder, params);
    let _window: &mut _ = builder
        .push(" ORDER BY s.id DESC LIMIT ")
        .push_bind(params.page_size)
        .push(" OFFSET ")
        .push_bind((params.page - 1).saturating_mul(params.page_size));
    let statuses = builder.build_query_as::<Status>().fetch_all(pool).await?;
    Ok(statuses)
}

/// The number of statuses `params`' filters select, whatever page they
/// land on — the `total` of the two envelope listings.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn count(pool: &PgPool, params: &ListParams) -> AppResult<i64> {
    let mut builder = QueryBuilder::<Postgres>::new(COUNT_SQL);
    push_filters(&mut builder, params);
    let total = builder.build_query_scalar::<i64>().fetch_one(pool).await?;
    Ok(total)
}

/// Appends the shared `WHERE` clause of [`list`] and [`count`]: the live
/// arm always, then the visibility arms `godmode` decides, the boost arm
/// `with_reblogs` decides, and the three narrowing filters — `local_only`,
/// one author, one domain — each pushed only when it selects something.
/// The clause leads with [`LIVE`] so every filter after it is one more
/// `AND`, the shape [`super::status_visibility_counts`] gives its own
/// counters.
fn push_filters(builder: &mut QueryBuilder<Postgres>, params: &ListParams) {
    let _live: &mut _ = builder.push(LIVE);
    let _visibility: &mut _ = builder.push(if params.godmode.get() {
        ALL_ARMS
    } else {
        PUBLIC_ARMS
    });
    if !params.with_reblogs.get() {
        let _boosts: &mut _ = builder.push(NO_REBLOGS);
    }
    if params.local_only.get() {
        let _local: &mut _ = builder.push(LOCAL_AUTHORS);
    }
    if let Some(user_id) = params.user_id {
        let _author: &mut _ = builder.push(" AND s.user_id = ").push_bind(user_id);
    }
    if let Some(domain) = &params.domain {
        let _host: &mut _ = builder.push(" AND u.domain = ").push_bind(domain);
    }
}

/// [`set_scope`]'s write: the two scope columns, each held by `COALESCE`
/// when its parameter is `NULL` (the absent field), `updated_at` moved, and
/// the `deleted_at IS NULL` guard that keeps a tombstone from taking a new
/// scope. `RETURNING *` answers the row as it now stands for the render.
///
/// `$3` is cast to `text` so the `COALESCE` has the column's own type to
/// fall back to: the `Visibility` enum arrives as `sqlx`'s `text`, and an
/// absent one is a plain `NULL` of that type.
const SET_SCOPE_SQL: &str = "UPDATE statuses
     SET sensitive = COALESCE($2, sensitive),
         visibility = COALESCE($3::text, visibility),
         updated_at = now()
   WHERE id = $1 AND deleted_at IS NULL
RETURNING *";

/// The admin's scope write: the two fields of one live status.
///
/// `PUT /api/v1/pleroma/admin/statuses/{id}`'s write moves the two scope
/// fields of the live row `id` names, each only where the request presented
/// it (`None` is "leave this column alone", Pleroma's `toggle_sensitive`
/// and `set_visibility` clauses that fall through when the parameter is
/// absent).
///
/// `RETURNING *` answers the post as it now stands, which is what the
/// endpoint renders; `None` is the row a concurrent delete tombstoned
/// between the endpoint's gate and this write — the `deleted_at IS NULL`
/// arm of the update statement is the whole guard.
///
/// Nothing else moves: the text, the warnings, the mentions, the counters
/// and [`edited_at`](Status::edited_at) are the author's edit surface, not
/// the moderation one, and only `updated_at` rides the change.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_scope(
    pool: &PgPool,
    id: Uuid,
    sensitive: Option<bool>,
    visibility: Option<Visibility>,
) -> AppResult<Option<Status>> {
    let row = sqlx::query_as::<_, Status>(SET_SCOPE_SQL)
        .bind(id)
        .bind(sensitive)
        .bind(visibility)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            status::{self, NewStatus},
            user::{self, NewUser, User},
        },
    };

    /// The window every filter test reads: the whole first page with every
    /// arm open, so the assertions are about the filters and nothing else.
    fn open() -> ListParams {
        ListParams {
            page: 1,
            page_size: 100,
            godmode: Flag::new(true),
            local_only: Flag::new(false),
            with_reblogs: Flag::new(true),
            user_id: None,
            domain: None,
        }
    }

    /// A local account named `nickname`.
    async fn author(pool: &PgPool, nickname: &str) -> User {
        let ap_id = user::local_ap_id(TEST_BASE_URL, nickname);
        user::insert(pool, &NewUser::local(nickname, &ap_id))
            .await
            .expect("a local author inserts")
    }

    /// `poster`'s public status, so a test can name its id.
    async fn post(pool: &PgPool, poster: &User, text: &str, visibility: Visibility) -> Status {
        let new = NewStatus {
            visibility,
            ..NewStatus::local(poster, text)
        };
        status::insert(pool, TEST_BASE_URL, &new)
            .await
            .expect("a local status inserts")
    }

    /// A mirror of a remote post: the row an inbound `Create` would store,
    /// authored by `remote` and owned by nobody here.
    async fn mirror(pool: &PgPool, remote: &User, slug: &str) -> Status {
        let new = NewStatus {
            uri: Some(format!("https://remote.example/objects/{slug}")),
            user_id: remote.id,
            content: format!("a remote post {slug}"),
            raw_text: format!("a remote post {slug}"),
            ..NewStatus::default()
        };
        status::insert(pool, TEST_BASE_URL, &new)
            .await
            .expect("a mirrored status inserts")
    }

    /// The account this instance does not own, with its host.
    async fn remote_author(pool: &PgPool, nickname: &str, host: &str) -> User {
        user::insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@{host}"),
                ap_id: format!("https://{host}/users/{nickname}"),
                domain: Some(host.to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("a remote mirror inserts")
    }

    /// The ids of `rows` in the order they arrived, for order assertions.
    fn ids(rows: &[Status]) -> Vec<Uuid> {
        rows.iter().map(|row| row.id).collect()
    }

    #[tokio::test]
    async fn godmode_decides_which_visibilities_are_listed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = author(pool, "alice").await;
        let public = post(pool, &alice, "public one", Visibility::Public).await;
        let unlisted = post(pool, &alice, "unlisted one", Visibility::Unlisted).await;
        let private = post(pool, &alice, "private one", Visibility::Private).await;
        let direct = post(pool, &alice, "direct one", Visibility::Direct).await;
        let local = post(pool, &alice, "local one", Visibility::Local).await;

        let every = list(pool, &open()).await.expect("the ledger reads");
        assert_eq!(
            ids(&every),
            vec![local.id, direct.id, private.id, unlisted.id, public.id],
            "godmode sees every visibility, newest id first, which is the \
             UUIDv7 creation order"
        );
        assert_eq!(
            count(pool, &open()).await.expect("the ledger counts"),
            5,
            "and the total is the same set"
        );

        let quiet = list(
            pool,
            &ListParams {
                godmode: Flag::new(false),
                ..open()
            },
        )
        .await
        .expect("the ledger reads");
        assert_eq!(
            ids(&quiet),
            vec![unlisted.id, public.id],
            "without godmode only the two public visibilities are listed"
        );
        assert_eq!(
            count(
                pool,
                &ListParams {
                    godmode: Flag::new(false),
                    ..open()
                }
            )
            .await
            .expect("the ledger counts"),
            2,
            "and the total weighs the same filter the page does"
        );
    }

    #[tokio::test]
    async fn reblogs_local_authors_and_domains_narrow_the_ledger() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = author(pool, "alice").await;
        let root = post(pool, &alice, "a public post", Visibility::Public).await;
        let boost = status::create_reblog(pool, TEST_BASE_URL, &alice, &root, Visibility::Public)
            .await
            .expect("a reblog writes")
            .0;
        let stranger = remote_author(pool, "bob", "remote.example").await;
        let mirrored = mirror(pool, &stranger, "one").await;

        let all = list(pool, &open()).await.expect("the ledger reads");
        assert_eq!(
            ids(&all),
            vec![mirrored.id, boost.id, root.id],
            "every live row is listed, whoever authored it"
        );

        let posts = list(
            pool,
            &ListParams {
                with_reblogs: Flag::new(false),
                ..open()
            },
        )
        .await
        .expect("the ledger reads");
        assert_eq!(
            ids(&posts),
            vec![mirrored.id, root.id],
            "the boost row is Pleroma's excluded reblog"
        );

        let own = list(
            pool,
            &ListParams {
                local_only: Flag::new(true),
                ..open()
            },
        )
        .await
        .expect("the ledger reads");
        assert_eq!(
            ids(&own),
            vec![boost.id, root.id],
            "local_only is the local authors' set, boost included"
        );

        let hosts = list(
            pool,
            &ListParams {
                domain: Some("remote.example".to_owned()),
                ..open()
            },
        )
        .await
        .expect("the ledger reads");
        assert_eq!(
            ids(&hosts),
            vec![mirrored.id],
            "the domain filter is the author's stored host"
        );
    }

    #[tokio::test]
    async fn a_domain_nobody_came_from_selects_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = author(pool, "alice").await;
        let kept = post(pool, &alice, "a local post", Visibility::Public).await;
        assert!(
            ids(&list(pool, &open()).await.expect("the ledger reads")).contains(&kept.id),
            "the fixture post is on the open ledger, so the empty answer \
             below is the filter's doing and not an empty table"
        );

        let absent = ListParams {
            domain: Some("absent.example".to_owned()),
            ..open()
        };
        assert!(
            list(pool, &absent)
                .await
                .expect("the ledger reads")
                .is_empty(),
            "an unknown domain lists nothing"
        );
        assert_eq!(
            count(pool, &absent).await.expect("the ledger counts"),
            0,
            "and its total says so too"
        );
    }

    #[tokio::test]
    async fn one_author_and_one_window_of_the_ledger() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = author(pool, "alice").await;
        let bob = author(pool, "bob").await;
        let mut alices = Vec::new();
        for n in 0..3 {
            alices.push(post(pool, &alice, &format!("alice {n}"), Visibility::Public).await);
        }
        let bobs = post(pool, &bob, "bob's one", Visibility::Public).await;

        let scoped = ListParams {
            user_id: Some(alice.id),
            ..open()
        };
        let page = list(pool, &scoped)
            .await
            .expect("the author's ledger reads");
        assert_eq!(
            ids(&page),
            vec![alices[2].id, alices[1].id, alices[0].id],
            "the author filter is that account's whole ledger, newest first"
        );
        assert_eq!(
            count(pool, &scoped)
                .await
                .expect("the author's ledger counts"),
            3,
            "and its total is that account's count"
        );
        assert!(
            ids(&list(pool, &open()).await.expect("the ledger reads")).contains(&bobs.id),
            "the unscoped ledger still holds the other account's post"
        );

        let tail = list(
            pool,
            &ListParams {
                page: 2,
                page_size: 2,
                ..scoped
            },
        )
        .await
        .expect("the second page reads");
        assert_eq!(
            ids(&tail),
            vec![alices[0].id],
            "page 2 of a two-row window is what the first window left"
        );
    }

    #[tokio::test]
    async fn set_scope_moves_the_two_fields_and_nothing_else() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = author(pool, "alice").await;
        let row = post(pool, &alice, "text", Visibility::Public).await;

        let both = set_scope(pool, row.id, Some(true), Some(Visibility::Private))
            .await
            .expect("the write runs")
            .expect("the live row answers");
        assert!(both.sensitive, "the sensitive flag moved");
        assert_eq!(both.visibility, Visibility::Private, "and so did it");
        assert_eq!(both.content, row.content, "the text is where it was");
        assert_eq!(
            both.edited_at, row.edited_at,
            "a scope change is no content edit: no revision stamp"
        );

        let untouched = set_scope(pool, row.id, None, None)
            .await
            .expect("the write runs")
            .expect("the live row answers");
        assert!(untouched.sensitive, "an absent sensitive keeps the value");
        assert_eq!(
            untouched.visibility,
            Visibility::Private,
            "an absent visibility keeps the value"
        );

        let folded = set_scope(pool, row.id, Some(false), None)
            .await
            .expect("the write runs")
            .expect("the live row answers");
        assert!(
            !folded.sensitive,
            "one field moves while the other holds its ground"
        );
        assert_eq!(
            folded.visibility,
            Visibility::Private,
            "the visibility the first write moved still stands"
        );
    }

    #[tokio::test]
    async fn set_scope_answers_nothing_for_a_row_that_is_not_there() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = author(pool, "alice").await;
        let row = post(pool, &alice, "text", Visibility::Public).await;

        assert!(
            set_scope(pool, row.id, Some(true), None)
                .await
                .expect("the write runs")
                .is_some(),
            "the live row answers with its own new state"
        );
        let deleted: Option<Status> = status::soft_delete(pool, row.id)
            .await
            .expect("the fixture delete runs");
        assert!(deleted.is_some(), "the fixture deleted a live row");
        assert!(
            set_scope(pool, row.id, Some(true), None)
                .await
                .expect("the write runs")
                .is_none(),
            "a tombstone takes no scope change"
        );
        assert!(
            set_scope(pool, Uuid::now_v7(), Some(true), None)
                .await
                .expect("the write runs")
                .is_none(),
            "an id that names no row answers nothing"
        );
    }
}
