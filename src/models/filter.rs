//! The `custom_filters` table and the keywords and statuses hanging off it:
//! Mastodon v2 filters.
//!
//! Reference: Mastodon's `CustomFilter` (`app/models/custom_filter.rb`). A
//! filter is a titled rule of one account: the `context`s it applies in, the
//! [`FilterAction`] a match triggers, an optional expiry, and what it looks
//! for — [`Keyword`]s ([`keyword`]) and whole statuses ([`status`]). Both of
//! those load with the filter, so a [`Filter`] is everything the
//! application step ([`crate::services::filter`]) and the entities need.
//!
//! The v1 phrase filters are served from the same rows ([`phrase`]): one v1
//! filter is one keyword of a v2 filter, as in Mastodon.
//!
//! Ids are `UUIDv7`s made in Rust (decision 4), so newest-first is
//! `ORDER BY id DESC`. Expiry is a query condition ([`list_active`] reads
//! `expires_at IS NULL OR expires_at > now()`), so no cleanup job exists.
//! Filters are strictly per-user: [`get_owned`], [`update`] and [`delete`]
//! carry the owner in their `WHERE` clause, so a foreign id is
//! indistinguishable from a missing one. Deleting a user takes their filters
//! with them through the foreign key's cascade.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use self::{
    keyword::{Keyword, KeywordPatch, NewKeyword},
    status::FilterStatus,
};
use crate::{error::AppResult, util::id::new_id};

pub mod keyword;
pub mod phrase;
pub mod status;

/// [`create`]: the id this module mints, the owner and what the caller
/// decided, with the timestamps left to the column defaults.
const INSERT_SQL: &str =
    "INSERT INTO custom_filters (id, user_id, title, context, filter_action, expires_at)
     VALUES ($1, $2, $3, $4, $5, $6) RETURNING *";

/// [`get_owned`]: one filter, only of its owner.
const GET_OWNED_SQL: &str = "SELECT * FROM custom_filters WHERE id = $1 AND user_id = $2";

/// [`list_active`]: the owner's filters that have not expired, newest first.
/// A `NULL` expiry is an open end.
const LIST_ACTIVE_SQL: &str = "SELECT * FROM custom_filters
     WHERE user_id = $1 AND (expires_at IS NULL OR expires_at > now())
  ORDER BY id DESC";

/// [`list_all`]: every filter of the owner, expired ones too, newest first.
const LIST_ALL_SQL: &str = "SELECT * FROM custom_filters WHERE user_id = $1 ORDER BY id DESC";

/// [`update`]: the columns the patch names (a `NULL` argument keeps the
/// stored value; `$6` says whether `$7` replaces the expiry, `NULL` included).
const UPDATE_SQL: &str = "UPDATE custom_filters
     SET title = COALESCE($3, title),
         context = COALESCE($4, context),
         filter_action = COALESCE($5, filter_action),
         expires_at = CASE WHEN $6 THEN $7 ELSE expires_at END,
         updated_at = now()
     WHERE id = $1 AND user_id = $2
  RETURNING *";

/// [`delete`]: one owned filter.
const DELETE_SQL: &str = "DELETE FROM custom_filters WHERE id = $1 AND user_id = $2";

/// [`update`]'s keyword removal: the keyword, only inside its own filter.
const DELETE_KEYWORD_SQL: &str =
    "DELETE FROM custom_filter_keywords WHERE id = $1 AND filter_id = $2";

/// What a matching status is subjected to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FilterAction {
    /// The status stays, annotated with the matching filter, and clients
    /// put it behind a warning.
    #[default]
    Warn,
    /// The status leaves the response entirely.
    Hide,
    /// The status stays, annotated, and clients blur its media.
    Blur,
}

impl FilterAction {
    /// The name the column and the API spell the action with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Hide => "hide",
            Self::Blur => "blur",
        }
    }

    /// The action called `name`, `None` for anything but the three names.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "warn" => Some(Self::Warn),
            "hide" => Some(Self::Hide),
            "blur" => Some(Self::Blur),
            _ => None,
        }
    }
}

/// One row of `custom_filters`, as the database hands it back.
#[derive(Clone, Debug, sqlx::FromRow)]
struct FilterRow {
    /// The primary key.
    id: Uuid,
    /// The owner.
    user_id: Uuid,
    /// The title.
    title: String,
    /// The contexts.
    context: Vec<String>,
    /// The action's name; the check constraint keeps it to the three.
    filter_action: String,
    /// The expiry.
    expires_at: Option<DateTime<Utc>>,
    /// The creation stamp.
    inserted_at: DateTime<Utc>,
    /// The last-change stamp.
    updated_at: DateTime<Utc>,
}

/// A v2 filter with its keywords and statuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    /// `UUIDv7` primary key; for a migrated v1 filter, the v1 filter's id.
    pub id: Uuid,
    /// The owning account; every read and write of a filter is scoped to it.
    pub user_id: Uuid,
    /// The name its owner gave it; for a v1 phrase filter, the phrase.
    pub title: String,
    /// Where the filter applies: a non-empty subset of `home`,
    /// `notifications`, `public`, `thread` and `account` (the API
    /// validates; the column stores what passed).
    pub context: Vec<String>,
    /// What a match does.
    pub action: FilterAction,
    /// When the filter stops being active; `None` means it never does.
    pub expires_at: Option<DateTime<Utc>>,
    /// When the row was created.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed.
    pub updated_at: DateTime<Utc>,
    /// The phrases it looks for, oldest first.
    pub keywords: Vec<Keyword>,
    /// The single statuses it matches, oldest first.
    pub statuses: Vec<FilterStatus>,
}

impl Filter {
    /// `row` with its `keywords` and `statuses`.
    fn assemble(row: FilterRow, keywords: Vec<Keyword>, statuses: Vec<FilterStatus>) -> Self {
        Self {
            id: row.id,
            user_id: row.user_id,
            title: row.title,
            context: row.context,
            action: FilterAction::from_name(&row.filter_action).unwrap_or_default(),
            expires_at: row.expires_at,
            inserted_at: row.inserted_at,
            updated_at: row.updated_at,
            keywords,
            statuses,
        }
    }
}

/// What to create: the filter's own columns; its keywords go beside it.
#[derive(Clone, Debug, Default)]
pub struct NewFilter {
    /// The title; empty in the default.
    pub title: String,
    /// The contexts the filter applies in; empty in the default.
    pub context: Vec<String>,
    /// What a match does; [`FilterAction::Warn`] by default.
    pub action: FilterAction,
    /// When the filter expires; `None` (never) by default.
    pub expires_at: Option<DateTime<Utc>>,
}

/// The columns [`update`] may change; a `None` keeps the stored value.
#[derive(Clone, Debug, Default)]
pub struct FilterPatch {
    /// The new title.
    pub title: Option<String>,
    /// The new contexts.
    pub context: Option<Vec<String>>,
    /// The new action.
    pub action: Option<FilterAction>,
    /// What happens to the expiry.
    pub expires_at: ExpiryChange,
}

/// What a [`FilterPatch`] does to a filter's expiry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExpiryChange {
    /// The stored expiry stays.
    #[default]
    Keep,
    /// The filter stops expiring.
    Clear,
    /// The filter expires at the given instant.
    Set(DateTime<Utc>),
}

impl ExpiryChange {
    /// The instant the filter ends up expiring at, `None` when never.
    const fn instant(self) -> Option<DateTime<Utc>> {
        match self {
            Self::Keep | Self::Clear => None,
            Self::Set(at) => Some(at),
        }
    }

    /// Whether the stored expiry is replaced at all.
    const fn replaces(self) -> bool {
        !matches!(self, Self::Keep)
    }
}

/// One entry of Mastodon's `keywords_attributes`, applied by [`update`].
#[derive(Clone, Debug, Default)]
pub struct KeywordChange {
    /// The keyword to change or remove; `None` adds a new one.
    pub id: Option<Uuid>,
    /// The phrase: required when adding, the new phrase when changing.
    pub keyword: Option<String>,
    /// The whole-word flag, `true` when adding without one.
    pub whole_word: Option<bool>,
    /// Remove the keyword `id` names.
    pub destroy: bool,
}

/// Inserts a filter owned by `user_id` with `keywords` and returns it.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create(
    pool: &PgPool,
    user_id: Uuid,
    filter: &NewFilter,
    keywords: &[NewKeyword],
) -> AppResult<Filter> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, FilterRow>(INSERT_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(&filter.title)
        .bind(&filter.context)
        .bind(filter.action.as_str())
        .bind(filter.expires_at)
        .fetch_one(&mut *tx)
        .await?;
    let mut stored = Vec::with_capacity(keywords.len());
    for entry in keywords {
        stored.push(keyword::insert(&mut tx, row.id, entry).await?);
    }
    tx.commit().await?;
    Ok(Filter::assemble(row, stored, Vec::new()))
}

/// Fetches the filter `id` if `user_id` owns it.
///
/// `None` for a missing or foreign id. Expiry is not consulted: an expired
/// filter is still a row its owner can read.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_owned(pool: &PgPool, id: Uuid, user_id: Uuid) -> AppResult<Option<Filter>> {
    let row = sqlx::query_as::<_, FilterRow>(GET_OWNED_SQL)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    match row {
        Some(found) => Ok(load(pool, vec![found]).await?.pop()),
        None => Ok(None),
    }
}

/// The filters of `user_id` that are active *now* — every one whose
/// expiry is `NULL` or still in the future — newest first.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_active(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Filter>> {
    let rows = sqlx::query_as::<_, FilterRow>(LIST_ACTIVE_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    load(pool, rows).await
}

/// Every filter of `user_id`, expired ones included, newest first.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_all(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Filter>> {
    let rows = sqlx::query_as::<_, FilterRow>(LIST_ALL_SQL)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    load(pool, rows).await
}

/// Applies `patch` and the keyword `changes` to one owned filter.
///
/// It runs in one transaction and returns the result; `None` when no owned
/// filter carries that id, or when a change names a keyword the filter does
/// not hold (nothing is applied then).
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    patch: &FilterPatch,
    changes: &[KeywordChange],
) -> AppResult<Option<Filter>> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, FilterRow>(UPDATE_SQL)
        .bind(id)
        .bind(user_id)
        .bind(patch.title.as_deref())
        .bind(patch.context.as_deref())
        .bind(patch.action.map(FilterAction::as_str))
        .bind(patch.expires_at.replaces())
        .bind(patch.expires_at.instant())
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    for change in changes {
        if !apply_change(&mut tx, row.id, user_id, change).await? {
            return Ok(None);
        }
    }
    tx.commit().await?;
    load(pool, vec![row]).await.map(|mut loaded| loaded.pop())
}

/// Applies one keyword `change` to the filter `filter_id` on `conn`;
/// `false` when it names a keyword the filter does not hold.
async fn apply_change(
    conn: &mut sqlx::PgConnection,
    filter_id: Uuid,
    user_id: Uuid,
    change: &KeywordChange,
) -> AppResult<bool> {
    let Some(keyword_id) = change.id else {
        let added = NewKeyword {
            keyword: change.keyword.clone().unwrap_or_default(),
            whole_word: change.whole_word.unwrap_or(true),
        };
        let _stored: Keyword = keyword::insert(conn, filter_id, &added).await?;
        return Ok(true);
    };
    if change.destroy {
        let removed = sqlx::query(DELETE_KEYWORD_SQL)
            .bind(keyword_id)
            .bind(filter_id)
            .execute(conn)
            .await?;
        return Ok(removed.rows_affected() > 0);
    }
    let patch = KeywordPatch {
        keyword: change.keyword.clone(),
        whole_word: change.whole_word,
    };
    let changed = keyword::update_owned(conn, keyword_id, user_id, &patch).await?;
    Ok(changed.is_some_and(|kept| kept.filter_id == filter_id))
}

/// Deletes the filter `id` of `user_id` with its keywords and statuses,
/// reporting whether one went.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete(pool: &PgPool, id: Uuid, user_id: Uuid) -> AppResult<bool> {
    let deleted = sqlx::query(DELETE_SQL)
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Joins `rows` with their keywords and statuses, keeping the row order.
async fn load(pool: &PgPool, rows: Vec<FilterRow>) -> AppResult<Vec<Filter>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let keywords = keyword::for_filters(pool, &ids).await?;
    let statuses = status::for_filters(pool, &ids).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let own_keywords = keywords
                .iter()
                .filter(|found| found.filter_id == row.id)
                .cloned()
                .collect();
            let own_statuses = statuses
                .iter()
                .filter(|found| found.filter_id == row.id)
                .cloned()
                .collect();
            Filter::assemble(row, own_keywords, own_statuses)
        })
        .collect())
}

#[cfg(test)]
mod tests;
