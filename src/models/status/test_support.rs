//! The fixtures the `status` module's unit tests share: the users, posts,
//! replies, quotes and boosts a test starts with, and the little readers
//! that check what a write moved. Test-only.

use sqlx::{Error as SqlxError, PgPool};
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    error::AppError,
    models::user::{self, NewUser, User, insert as insert_user, local_ap_id},
    pagination::PageParams,
};

use super::{Counter, NewStatus, Status, get_by_id_including_deleted, insert};

/// The instance origin the fixtures pretend to serve.
pub(super) const BASE: &str = TEST_BASE_URL;

/// Inserts a local user named `nickname`.
pub(super) async fn user(pool: &PgPool, nickname: &str) -> User {
    insert_user(
        pool,
        &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
    )
    .await
    .expect("fixture user inserts")
}

/// Inserts `text` as a local public status of `author`.
pub(super) async fn post(pool: &PgPool, author: &User, text: &str) -> Status {
    insert(pool, BASE, &NewStatus::local(author, text))
        .await
        .expect("a plain local post inserts")
}

/// Inserts `text` as `author`'s answer to `parent`.
pub(super) async fn reply(pool: &PgPool, author: &User, parent: &Status, text: &str) -> Status {
    insert(
        pool,
        BASE,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a reply to a stored status inserts")
}

/// `author`'s quote of the status `original`, as its row.
pub(super) async fn quote(pool: &PgPool, author: &User, original: Uuid, text: &str) -> Status {
    insert(
        pool,
        BASE,
        &NewStatus {
            quote_of_id: Some(original),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a quote of a stored status inserts")
}

/// The row of user `id` as it stands now, for reading counters back.
pub(super) async fn fresh_user(pool: &PgPool, id: Uuid) -> User {
    user::get_by_id(pool, id)
        .await
        .unwrap()
        .expect("the user exists")
}

/// The stored status behind `id`, tombstone included.
pub(super) async fn stored(pool: &PgPool, id: Uuid) -> Status {
    get_by_id_including_deleted(pool, id)
        .await
        .unwrap()
        .expect("the status exists")
}

/// The stored value of one counter of status `id`, read from the column
/// the counter names.
pub(super) async fn counter(pool: &PgPool, id: Uuid, which: Counter) -> i32 {
    let sql = match which {
        Counter::Replies => "SELECT replies_count FROM statuses WHERE id = $1",
        Counter::Reblogs => "SELECT reblogs_count FROM statuses WHERE id = $1",
        Counter::Favourites => "SELECT favourites_count FROM statuses WHERE id = $1",
        Counter::Quotes => "SELECT quotes_count FROM statuses WHERE id = $1",
    };
    let row: (i32,) = sqlx::query_as(sql).bind(id).fetch_one(pool).await.unwrap();
    row.0
}

/// The ids of `statuses`, in the order they came back.
pub(super) fn ids(statuses: &[Status]) -> Vec<Uuid> {
    statuses.iter().map(|status| status.id).collect()
}

/// The SQLSTATE a database failure carries, or an empty string when the
/// failure is not the database reporting a constraint.
pub(super) fn sqlstate_of(err: AppError) -> String {
    if let AppError::Database(SqlxError::Database(db_err)) = err {
        return db_err.code().unwrap_or_default().into_owned();
    }
    String::new()
}

/// A remote mirror of `nickname` on `remote.example`, whose posts the
/// federation-shaped fixtures insert.
pub(super) async fn remote_user(pool: &PgPool, nickname: &str) -> User {
    let ap_id = format!("https://remote.example/users/{nickname}");
    insert_user(
        pool,
        &NewUser {
            nickname: format!("{nickname}@remote.example"),
            ap_id,
            domain: Some("remote.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("fixture remote user inserts")
}

/// A [`NewStatus`] mirroring the remote post `uri`, by `author`.
pub(super) fn mirrored(author: &User, uri: &str) -> NewStatus {
    NewStatus {
        uri: Some(uri.to_owned()),
        user_id: author.id,
        content: "<p>from elsewhere</p>".to_owned(),
        raw_text: "from elsewhere".to_owned(),
        local: false,
        ..NewStatus::default()
    }
}

/// The page params `query` names, the way the extractor makes them.
pub(super) fn page(query: &str) -> PageParams {
    PageParams::from_query(Some(query), crate::pagination::MAX_LIMIT)
        .expect("the fixture page parses")
}
