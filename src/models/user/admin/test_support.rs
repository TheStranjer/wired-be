//! Fixtures more than one child's tests build on: the nine-account
//! listing fixture, its seeders and reading helpers, and the
//! [`mastodon_list`] page helpers. Test-only.

use sqlx::PgPool;

use super::super::{Flag, NewUser, User, insert};
use super::mastodon::{MastodonListParams, mastodon_list};
use super::pleroma_list::{ListFilters, ListPage, ListParams};
use crate::pagination::PageParams;

/// The account the listing must never answer with, and the direct
/// lookup may.
pub(super) const INSTANCE_ACTOR: &str = "internal.fetch";

/// Inserts a local account named `nickname` with `customize` applied on
/// top of [`NewUser::local`]; the closure is how a fixture turns that
/// into a remote mirror or an account in some other state.
pub(super) async fn seeded<F>(pool: &PgPool, nickname: &str, customize: F)
where
    F: FnOnce(NewUser) -> NewUser,
{
    let new = customize(NewUser::local(
        nickname,
        &format!("https://test.example/users/{nickname}"),
    ));
    let _row: User = insert(pool, &new).await.expect("seed inserts");
}

/// The nine accounts the tests below read: the instance actor, six
/// locals each in a state of its own, a tagged `Service` and a remote
/// mirror. One slot per test, which the fixture empties between tests.
pub(super) async fn fixture() -> (crate::test_db::TestDb, PgPool) {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    seeded(&pool, INSTANCE_ACTOR, |user| user).await;
    seeded(&pool, "plain", |user| NewUser {
        name: "Plain Person".to_owned(),
        email: Some("plain@example.org".to_owned()),
        ..user
    })
    .await;
    seeded(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        email: Some("root@example.org".to_owned()),
        ..user
    })
    .await;
    seeded(&pool, "moddy", |user| NewUser {
        is_moderator: Flag::new(true),
        ..user
    })
    .await;
    seeded(&pool, "suspended", |user| NewUser {
        is_active: Flag::new(false),
        ..user
    })
    .await;
    seeded(&pool, "newbie", |user| NewUser {
        approval_pending: Flag::new(true),
        registration_reason: Some("please let me in".to_owned()),
        ..user
    })
    .await;
    seeded(&pool, "unminded", |user| NewUser {
        email_confirmed: Flag::new(false),
        ..user
    })
    .await;
    seeded(&pool, "beeper", |user| NewUser {
        actor_type: "Service".to_owned(),
        tags: vec!["bots".to_owned()],
        ..user
    })
    .await;
    seeded(&pool, "faraway@remote.example", |user| NewUser {
        local: Flag::new(false),
        ap_id: "https://remote.example/users/faraway".to_owned(),
        domain: Some("remote.example".to_owned()),
        name: "Far Away".to_owned(),
        ..user
    })
    .await;
    (db, pool)
}

/// The nicknames one page answers with, in the order it answers them.
pub(super) fn listed(page: &ListPage) -> Vec<&str> {
    page.users
        .iter()
        .map(|found| found.nickname.as_str())
        .collect()
}

/// Everything, on one page of `size` rows.
pub(super) fn everything(size: i64) -> ListParams {
    ListParams {
        page: 1,
        page_size: size,
        ..ListParams::default()
    }
}

/// The listing with one `filters` parameter and no other criterion.
pub(super) fn filtered(names: &str) -> ListParams {
    ListParams {
        filters: ListFilters::parse(Some(names)),
        ..everything(50)
    }
}

/// The first page of a [`mastodon_list`] call: `limit` rows, no cursor.
pub(super) const fn first_page(limit: i64) -> PageParams {
    PageParams {
        max_id: None,
        min_id: None,
        since_id: None,
        limit,
        offset: None,
    }
}

/// The nicknames one [`mastodon_list`] page answers with, in that order.
pub(super) fn listed_accounts(users: &[User]) -> Vec<&str> {
    users.iter().map(|found| found.nickname.as_str()).collect()
}

/// One [`mastodon_list`] call with the criteria the closure states.
pub(super) async fn mastodon<F>(pool: &PgPool, limit: i64, customize: F) -> Vec<User>
where
    F: FnOnce(MastodonListParams) -> MastodonListParams,
{
    mastodon_list(
        pool,
        &customize(MastodonListParams::default()),
        first_page(limit),
    )
    .await
    .expect("the listing reads")
}

/// The criteria that weigh one boolean in one of its states, and nothing
/// else: the matrix below reads one row per criterion, and building the
/// row through a closure keeps each case to the set it names.
pub(super) fn weigh(pick: fn(&mut MastodonListParams)) -> MastodonListParams {
    let mut params = MastodonListParams::default();
    pick(&mut params);
    params
}

/// The page of one [`mastodon_list`] call over the shared fixture.
pub(super) async fn weighed(pool: &PgPool, params: &MastodonListParams) -> Vec<User> {
    mastodon_list(pool, params, first_page(50))
        .await
        .expect("a criterion reads")
}

/// One row of the two criterion matrices below: what the criterion was
/// called, the criteria it states, and the nicknames the page answers with.
pub(super) type MastodonCase<'a> = (&'a str, MastodonListParams, &'a [&'a str]);
