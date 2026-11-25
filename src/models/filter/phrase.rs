//! The v1 view of v2 filters: one phrase filter per keyword.
//!
//! Mastodon serves `/api/v1/filters` from the v2 tables: a v1 filter *is* a
//! keyword, named by the keyword's id, carrying its parent filter's
//! contexts, expiry and action (`hide` reads as `irreversible`). Creating a
//! v1 filter makes a v2 filter titled by the phrase with that one keyword;
//! this module builds exactly that, and gives the new rows the same id so a
//! phrase filter's filter and keyword answer to one name.
//!
//! [`Phrase`] is the v1 shape the API renders; [`NewPhrase`] is what a v1
//! write carries.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    Filter, FilterAction, get_owned as get_filter,
    keyword::{self, Keyword, KeywordPatch, NewKeyword},
    list_active,
};
use crate::{
    error::{AppError, AppResult},
    util::id::new_id,
};

/// [`create_phrase`]'s filter insert: the shared id is chosen by the caller.
const INSERT_FILTER_SQL: &str = "INSERT INTO custom_filters
         (id, user_id, title, context, filter_action, expires_at)
     VALUES ($1, $2, $3, $4, $5, $6)";

/// [`update_phrase`]'s filter write: the contexts, action and expiry, and the
/// title when the filter holds this keyword alone.
const UPDATE_FILTER_SQL: &str = "UPDATE custom_filters
     SET title = CASE WHEN $3 THEN $4 ELSE title END,
         context = $5, filter_action = $6, expires_at = $7, updated_at = now()
     WHERE id = $1 AND user_id = $2";

/// [`delete_phrase`]'s clean-up: a filter left with nothing to match goes.
const DELETE_EMPTY_FILTER_SQL: &str = "DELETE FROM custom_filters f
     WHERE f.id = $1 AND f.user_id = $2
       AND NOT EXISTS (SELECT 1 FROM custom_filter_keywords WHERE filter_id = f.id)
       AND NOT EXISTS (SELECT 1 FROM custom_filter_statuses WHERE filter_id = f.id)";

/// One v1 phrase filter: a keyword and what its parent filter says about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phrase {
    /// The keyword; its id names the v1 filter.
    pub keyword: Keyword,
    /// The parent filter's contexts.
    pub context: Vec<String>,
    /// The parent filter's action; `hide` is v1's `irreversible`.
    pub action: FilterAction,
    /// The parent filter's expiry.
    pub expires_at: Option<DateTime<Utc>>,
}

impl Phrase {
    /// The phrase view of `keyword` under its parent `filter`.
    #[must_use]
    pub fn new(filter: &Filter, keyword: &Keyword) -> Self {
        Self {
            keyword: keyword.clone(),
            context: filter.context.clone(),
            action: filter.action,
            expires_at: filter.expires_at,
        }
    }

    /// Whether clients hide matches instead of warning: the `hide` action.
    #[must_use]
    pub fn irreversible(&self) -> bool {
        self.action == FilterAction::Hide
    }
}

/// What a v1 write carries: every mutable column of a phrase filter.
///
/// [`Default::default`] mirrors the column defaults, so a caller sets only
/// what it decides on; the usual minimum is the phrase and the contexts.
#[derive(Clone, Debug)]
pub struct NewPhrase {
    /// The phrase to look for; empty in the default.
    pub phrase: String,
    /// The contexts the filter applies in; empty in the default.
    pub context: Vec<String>,
    /// Whole-word matching; `true` by default.
    pub whole_word: bool,
    /// Hide rather than warn; `false` by default.
    pub irreversible: bool,
    /// When the filter expires; `None` (never) by default.
    pub expires_at: Option<DateTime<Utc>>,
}

impl Default for NewPhrase {
    /// Empty phrase and contexts, whole-word on, irreversible off and no
    /// expiry.
    fn default() -> Self {
        Self {
            phrase: String::new(),
            context: Vec::new(),
            whole_word: true,
            irreversible: false,
            expires_at: None,
        }
    }
}

impl NewPhrase {
    /// The action the `irreversible` flag spells.
    const fn action(&self) -> FilterAction {
        if self.irreversible {
            FilterAction::Hide
        } else {
            FilterAction::Warn
        }
    }
}

/// Creates the v2 filter a v1 write describes — titled by the phrase, with
/// the one keyword, both under one fresh id — and returns it.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn create_phrase(pool: &PgPool, user_id: Uuid, new: &NewPhrase) -> AppResult<Filter> {
    let id = new_id();
    let mut tx = pool.begin().await?;
    let _inserted = sqlx::query(INSERT_FILTER_SQL)
        .bind(id)
        .bind(user_id)
        .bind(&new.phrase)
        .bind(&new.context)
        .bind(new.action().as_str())
        .bind(new.expires_at)
        .execute(&mut *tx)
        .await?;
    let entry = NewKeyword {
        keyword: new.phrase.clone(),
        whole_word: new.whole_word,
    };
    let _stored: Keyword = keyword::insert_with_id(&mut tx, id, id, &entry).await?;
    tx.commit().await?;
    get_filter(pool, id, user_id)
        .await?
        .ok_or(AppError::NotFound)
}

/// The phrase filters of `user_id` that are active now — one per keyword of
/// every unexpired filter — newest first.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_active_phrases(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<Phrase>> {
    let filters = list_active(pool, user_id).await?;
    let mut phrases: Vec<Phrase> = filters
        .iter()
        .flat_map(|filter| {
            filter
                .keywords
                .iter()
                .map(move |found| Phrase::new(filter, found))
        })
        .collect();
    phrases.sort_by_key(|found| std::cmp::Reverse(found.keyword.id));
    Ok(phrases)
}

/// The phrase filter named by the keyword `id` if `user_id` owns it; expiry
/// is not consulted.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_phrase(pool: &PgPool, id: Uuid, user_id: Uuid) -> AppResult<Option<Phrase>> {
    let Some(found) = keyword::get_owned(pool, id, user_id).await? else {
        return Ok(None);
    };
    let Some(filter) = get_filter(pool, found.filter_id, user_id).await? else {
        return Ok(None);
    };
    Ok(Some(Phrase::new(&filter, &found)))
}

/// Replaces every mutable column of the phrase filter `id` of `user_id` and
/// returns it; `None` when no owned keyword carries that id.
///
/// The parent filter takes the new contexts, action and expiry, and the new
/// phrase as its title when this keyword is the only one it holds.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update_phrase(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    new: &NewPhrase,
) -> AppResult<Option<Phrase>> {
    let Some(current) = keyword::get_owned(pool, id, user_id).await? else {
        return Ok(None);
    };
    let Some(filter) = get_filter(pool, current.filter_id, user_id).await? else {
        return Ok(None);
    };
    let sole = filter.keywords.len() == 1;
    let mut tx = pool.begin().await?;
    let _updated = sqlx::query(UPDATE_FILTER_SQL)
        .bind(filter.id)
        .bind(user_id)
        .bind(sole)
        .bind(&new.phrase)
        .bind(&new.context)
        .bind(new.action().as_str())
        .bind(new.expires_at)
        .execute(&mut *tx)
        .await?;
    let patch = KeywordPatch {
        keyword: Some(new.phrase.clone()),
        whole_word: Some(new.whole_word),
    };
    let _kept: Option<Keyword> = keyword::update_owned(&mut tx, id, user_id, &patch).await?;
    tx.commit().await?;
    get_phrase(pool, id, user_id).await
}

/// Deletes the phrase filter `id` of `user_id` — its keyword, and its parent
/// filter when nothing else is left in it — reporting whether one went.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn delete_phrase(pool: &PgPool, id: Uuid, user_id: Uuid) -> AppResult<bool> {
    let Some(found) = keyword::get_owned(pool, id, user_id).await? else {
        return Ok(false);
    };
    let mut tx = pool.begin().await?;
    let removed = keyword::delete_owned(&mut tx, id, user_id).await?;
    let _emptied = sqlx::query(DELETE_EMPTY_FILTER_SQL)
        .bind(found.filter_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(removed)
}
