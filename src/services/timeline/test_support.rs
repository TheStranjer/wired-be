//! The fixtures the `timeline` module's unit tests share: accounts, posts,
//! lists and the unfiltered page builders every timeline read starts from.
//! Test-only.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    models::{
        list::{self, List, NewList},
        status::{self, NewStatus, Status, Visibility},
        user::{self, NewUser, User, insert as insert_user, local_ap_id},
    },
    pagination::PageParams,
};

use super::{TimelineFilters, TimelineQuery, TimelineSource};

/// The instance origin the fixtures pretend to serve.
pub(super) const BASE: &str = TEST_BASE_URL;

/// Inserts a local user named `nickname`.
pub(super) async fn local(pool: &PgPool, nickname: &str) -> User {
    insert_user(
        pool,
        &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// Inserts a mirror of the remote actor `nickname` on `domain`.
pub(super) async fn remote(pool: &PgPool, nickname: &str, domain: &str) -> User {
    insert_user(
        pool,
        &NewUser {
            nickname: format!("{nickname}@{domain}"),
            ap_id: format!("https://{domain}/users/{nickname}"),
            domain: Some(domain.to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// Reads `viewer` back from the table: a [`TimelineQuery`] owns its
/// viewer, so a test that asks twice hands out one row per ask.
pub(super) async fn as_viewer(pool: &PgPool, viewer: &User) -> User {
    user::get_by_id(pool, viewer.id)
        .await
        .expect("the viewer reads")
        .expect("the fixture viewer exists")
}

/// Inserts `text` as `author`'s live local status of `visibility`.
pub(super) async fn post(
    pool: &PgPool,
    author: &User,
    visibility: Visibility,
    text: &str,
) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            visibility,
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a fixture post inserts")
}

/// Inserts `author`'s `direct` status mentioning everyone in `mentioned`.
pub(super) async fn direct(
    pool: &PgPool,
    author: &User,
    mentioned: &[&User],
    text: &str,
) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            visibility: Visibility::Direct,
            mentioned_user_ids: mentioned.iter().map(|member| member.id).collect(),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a fixture direct message inserts")
}

/// Inserts `author`'s public reply to `parent`, @-mentioning everyone in
/// `mentioned` (empty for none).
pub(super) async fn reply(
    pool: &PgPool,
    author: &User,
    parent: &Status,
    mentioned: &[&User],
    text: &str,
) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            mentioned_user_ids: mentioned.iter().map(|member| member.id).collect(),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a fixture reply inserts")
}

/// `reblogger`'s public reblog of `original`.
pub(super) async fn reblog(pool: &PgPool, reblogger: &User, original: &Status) -> Status {
    status::create_reblog(pool, BASE, reblogger, original, Visibility::Public)
        .await
        .expect("a fixture reblog writes")
        .0
}

/// Inserts a mirrored (`local = false`) public status whose object uri
/// and text are keyed by `uri_key`, carrying `hashtags` by stored name.
pub(super) async fn remote_post(
    pool: &PgPool,
    author: &User,
    uri_key: &str,
    hashtags: &[&str],
) -> Status {
    status::insert(
        pool,
        BASE,
        &NewStatus {
            uri: Some(format!("https://origin.test.example/objects/{uri_key}")),
            user_id: author.id,
            raw_text: uri_key.to_owned(),
            hashtags: hashtags.iter().map(|name| (*name).to_owned()).collect(),
            ..NewStatus::default()
        },
    )
    .await
    .expect("a fixture remote status inserts")
}

/// Attaches one media row to `status_id`.
pub(super) async fn attach_media(pool: &PgPool, status_id: Uuid) {
    let written = sqlx::query(
        "INSERT INTO media_attachments (id, status_id, url, content_type)
             VALUES ($1, $2, $3, 'image/png')",
    )
    .bind(Uuid::now_v7())
    .bind(status_id)
    .bind(format!("{BASE}/media/fixture.png"))
    .execute(pool)
    .await
    .expect("the media fixture inserts");
    assert_eq!(written.rows_affected(), 1, "one attachment lands");
}

/// Creates `owner`'s list titled `circle` with `policy` as its replies
/// policy and `exclusive` as its exclusiveness.
pub(super) async fn owned_list(pool: &PgPool, owner: &User, policy: &str, exclusive: bool) -> List {
    list::create(
        pool,
        owner.id,
        &NewList {
            title: "circle".to_owned(),
            replies_policy: policy.to_owned(),
            exclusive,
        },
    )
    .await
    .expect("the fixture list creates")
}

/// Adds `member` to `list` through the model's own write (the API's
/// follow rule is [`crate::api::lists`]'s business, not membership's).
pub(super) async fn add_member(pool: &PgPool, list: &List, member: &User) {
    let added = list::add_members(pool, list.id, &[member.id])
        .await
        .expect("the fixture membership adds");
    assert_eq!(added, 1, "exactly one membership went in");
}

/// The whole-table page: no cursors and a limit above every fixture
/// this module seeds (the cap of 40 is the endpoint extractor's, not
/// [`TimelineQuery::fetch`]'s).
pub(super) const fn full_page() -> PageParams {
    PageParams {
        max_id: None,
        min_id: None,
        since_id: None,
        limit: 40,
        offset: None,
    }
}

/// A timeline `source` as `viewer`, unfiltered, one full page.
pub(super) fn timeline(viewer: Option<User>, source: TimelineSource) -> TimelineQuery {
    TimelineQuery {
        viewer,
        source,
        filters: TimelineFilters::default(),
        page: full_page(),
    }
}

/// A timeline `source` as `viewer` with `filters`, one full page.
pub(super) fn filtered(
    viewer: Option<User>,
    source: TimelineSource,
    filters: TimelineFilters,
) -> TimelineQuery {
    TimelineQuery {
        viewer,
        source,
        filters,
        page: full_page(),
    }
}

/// Runs `query` and answers its page as ids, in the order it came back.
pub(super) async fn page_ids(pool: &PgPool, query: &TimelineQuery) -> Vec<Uuid> {
    let statuses = query.fetch(pool).await.expect("the timeline query runs");
    statuses.iter().map(|status| status.id).collect()
}

/// The ids of `statuses`, id descending — the order
/// [`fetch`](TimelineQuery::fetch) answers in, since `UUIDv7` text sorts
/// by creation (decision 4).
pub(super) fn newest_first(statuses: &[&Status]) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = statuses.iter().map(|status| status.id).collect();
    ids.sort_by(|a, b| b.cmp(a));
    ids
}
