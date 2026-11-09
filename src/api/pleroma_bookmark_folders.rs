//! Bookmark folders over HTTP: one account's named shelves for filing
//! bookmarks (`docs/ARCHITECTURE.md` decision 1).
//!
//! The reference is Pleroma's `BookmarkFolderController`
//! (`lib/pleroma/web/pleroma_api/controllers/bookmark_folder_controller.ex`)
//! over `Pleroma.BookmarkFolder` (`lib/pleroma/bookmark_folder.ex`), routed
//! in `lib/pleroma/web/router.ex` under `/api/v1/pleroma` as:
//!
//! ```text
//! get    "/api/v1/pleroma/bookmark_folders"     -> index
//! post   "/api/v1/pleroma/bookmark_folders"     -> create
//! patch  "/api/v1/pleroma/bookmark_folders/:id" -> update
//! delete "/api/v1/pleroma/bookmark_folders/:id" -> delete
//! ```
//!
//! Those four routes are this module; the fact behind them is the
//! `bookmark_folders` table ([`crate::models::bookmark_folder`]) and the
//! entity is the four-key Pleroma `BookmarkFolder` shape
//! ([`crate::entities::bookmark_folder::BookmarkFolder`]). Folders are
//! strictly per-account: a foreign `{id}` is indistinguishable from a
//! missing one, and both answer the one 404 (Pleroma's `belongs_to_user?`
//! gate, rendered as `not_found` rather than its `forbidden`).
//!
//! - `GET /api/v1/pleroma/bookmark_folders` answers the caller's folders as
//!   an array in creation order (id ascending, decision 4; [`bookmark_folder::list`]).
//! - `POST /api/v1/pleroma/bookmark_folders` creates one: `name` (required,
//!   1–50 characters) and an optional `emoji`. A name the owner already
//!   uses is the 422 `{"error":"name: has already been taken"}` — the
//!   table's `(user_id, name)` unique key.
//! - `PATCH /api/v1/pleroma/bookmark_folders/{id}` changes the parameters
//!   it names (present-only, like the lists `PUT`); a foreign or unknown id
//!   is the 404, a rename onto a taken name the 422.
//! - `DELETE /api/v1/pleroma/bookmark_folders/{id}` removes one and answers
//!   the deleted entity; the bookmarks filed in it are kept (their
//!   `folder_id` is set `NULL` through the foreign key).
//!
//! # The emoji
//!
//! `emoji` is either one unicode emoji or a `:shortcode:` naming a local
//! custom emoji, exactly the two shapes [`crate::util::emoji`] classifies;
//! the writes refuse anything else with the 422 `{"error":"emoji: Invalid
//! emoji"}`, as Pleroma's `validate_emoji` does. The entity's `emoji_url`
//! is the custom emoji's image URL when the shortcode names one
//! this instance has ([`crate::models::custom_emoji`]), and `null` for a
//! unicode emoji or none — the whole page's share of lookups is one batched
//! [`custom_emoji::get_by_shortcodes`] read, so the list never queries per
//! folder (`docs/ARCHITECTURE.md` decision 13).
//!
//! Every route requires a user token ([`AuthUser`]) and, per the scope
//! pair, `read:bookmarks` to look and `write:bookmarks` to change.
//!
//! [`bookmark_folder::list`]: crate::models::bookmark_folder::list
//! [`AuthUser`]: crate::auth::AuthUser

use std::collections::{HashMap, HashSet};

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, patch},
};
use serde::Deserialize;
use sqlx::{Error as SqlxError, PgPool};
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    entities::bookmark_folder::BookmarkFolder,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{bookmark_folder, custom_emoji},
    oauth::scopes::{READ_BOOKMARKS, WRITE_BOOKMARKS},
    state::AppState,
    util::{emoji::Emoji, id::parse_id},
};

/// The longest name the endpoint accepts (the ticket's 1–50).
const MAX_NAME_CHARS: usize = 50;
/// 422 body for an absent or blank name (Pleroma's changeset wording).
const NAME_BLANK: &str = "name: can't be blank";
/// 422 body for a name over [`MAX_NAME_CHARS`] characters (likewise).
const NAME_TOO_LONG: &str = "name: should be at most 50 characters";
/// 422 body for a name the owner already uses (the ticket's exact text,
/// the table's `(user_id, name)` unique key).
const NAME_TAKEN: &str = "name: has already been taken";
/// 422 body for an emoji that is neither one unicode emoji nor a
/// `:shortcode:` naming a local custom emoji (Pleroma's `validate_emoji`).
const EMOJI_INVALID: &str = "emoji: Invalid emoji";
/// SQLSTATE of a Postgres unique violation.
const SQLSTATE_UNIQUE_VIOLATION: &str = "23505";

/// The `/api/v1/pleroma/bookmark_folders` router, nested into
/// [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_folders).post(create_folder))
        .route("/{id}", patch(update_folder).delete(delete_folder))
}

/// `GET /api/v1/pleroma/bookmark_folders` — the caller's folders, oldest
/// first (creation order), as an array.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:bookmarks` check. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn list_folders(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<BookmarkFolder>>> {
    auth.require(READ_BOOKMARKS)?;
    let rows = bookmark_folder::list(&state.db, auth.0.id).await?;
    debug!(
        count = rows.len(),
        user = %auth.0.id,
        "bookmark folders listed"
    );
    Ok(Json(render_entities(&state.db, &rows).await?))
}

/// `POST /api/v1/pleroma/bookmark_folders` — create a folder for the caller
/// and answer it.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:bookmarks` check; [`AppError::Unprocessable`] for a blank
/// or over-long `name`, an invalid `emoji`, or a `name` the owner already
/// uses; any database failure arrives through `?` as [`AppError::Database`].
async fn create_folder(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<FolderParams>,
) -> AppResult<Json<BookmarkFolder>> {
    auth.require(WRITE_BOOKMARKS)?;
    // An absent name arrives as the empty string: the same blank refusal
    // a present-but-empty one gets, like the lists' title.
    let name = validate_name(params.name.unwrap_or_default())?;
    let emoji = validate_emoji(&state.db, params.emoji).await?;
    let row = create_or_422(&state.db, auth.0.id, &name, emoji.as_deref()).await?;
    debug!(
        folder = %row.id,
        user = %auth.0.id,
        "bookmark folder created"
    );
    Ok(Json(single_entity(&state.db, &row).await?))
}

/// `PATCH /api/v1/pleroma/bookmark_folders/{id}` — change the named columns
/// of one owned folder and answer it. Parameters are those of `POST`,
/// present-only: an absent one keeps the stored value.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:bookmarks` check; [`AppError::Unprocessable`] for an
/// invalid `name`/`emoji` or a rename onto a taken name — validation runs
/// ahead of the lookup, as Pleroma's `CastAndValidate` plug does;
/// [`AppError::NotFound`] when the id is malformed, unknown or foreign. Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn update_folder(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<FolderParams>,
) -> AppResult<Json<BookmarkFolder>> {
    auth.require(WRITE_BOOKMARKS)?;
    let Some(folder_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let changes = bookmark_folder::FolderChanges {
        name: params.name.map(validate_name).transpose()?,
        emoji: validate_emoji(&state.db, params.emoji).await?,
    };
    let row = update_or_422(&state.db, auth.0.id, folder_id, &changes).await?;
    match row {
        Some(row) => Ok(Json(single_entity(&state.db, &row).await?)),
        None => Err(AppError::NotFound),
    }
}

/// `DELETE /api/v1/pleroma/bookmark_folders/{id}` — remove one owned folder
/// and answer the deleted entity. The bookmarks filed in it are kept, their
/// `folder_id` freed to `NULL` through the foreign key.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:bookmarks` check; [`AppError::NotFound`] when the id is
/// malformed, unknown or foreign. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn delete_folder(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<BookmarkFolder>> {
    auth.require(WRITE_BOOKMARKS)?;
    let Some(folder_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    match bookmark_folder::delete(&state.db, auth.0.id, folder_id).await? {
        Some(row) => {
            debug!(
                folder = %row.id,
                user = %auth.0.id,
                "bookmark folder deleted"
            );
            Ok(Json(single_entity(&state.db, &row).await?))
        }
        None => Err(AppError::NotFound),
    }
}

/// Creates `name`/`emoji` for `user_id`, turning the table's unique-key
/// violation into the 422 [`NAME_TAKEN`]; anything else stays the database's
/// own failure.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`NAME_TAKEN`] on a duplicate; any other
/// database failure arrives as [`AppError::Database`].
async fn create_or_422(
    pool: &PgPool,
    user_id: Uuid,
    name: &str,
    emoji: Option<&str>,
) -> AppResult<bookmark_folder::BookmarkFolder> {
    bookmark_folder::create(pool, user_id, name, emoji)
        .await
        .map_err(match_taken)
}

/// Applies `changes` to the owned folder `id`, turning the unique-key
/// violation into the 422 [`NAME_TAKEN`]; a foreign or missing id answers
/// `None` (the caller's 404), and any other failure stays the database's.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`NAME_TAKEN`] on a rename onto a taken
/// name; any other database failure arrives as [`AppError::Database`].
async fn update_or_422(
    pool: &PgPool,
    user_id: Uuid,
    id: Uuid,
    changes: &bookmark_folder::FolderChanges,
) -> AppResult<Option<bookmark_folder::BookmarkFolder>> {
    bookmark_folder::update(pool, user_id, id, changes)
        .await
        .map_err(match_taken)
}

/// A database failure that is the `(user_id, name)` unique violation becomes
/// the 422 duplicate-name refusal; every other error passes through.
fn match_taken(err: AppError) -> AppError {
    if is_taken(&err) {
        AppError::Unprocessable(NAME_TAKEN.to_owned())
    } else {
        err
    }
}

/// Whether `err` is the database refusing the `(user_id, name)` unique key.
fn is_taken(err: &AppError) -> bool {
    matches!(
        err,
        AppError::Database(SqlxError::Database(db))
            if db.code().as_deref() == Some(SQLSTATE_UNIQUE_VIOLATION)
    )
}

/// Renders `rows` as entities, resolving every folder emoji's image URL with
/// one batched [`custom_emoji::get_by_shortcodes`] read for the whole page —
/// a folder whose shortcode names no local emoji renders a `null` URL, and a
/// folder with no emoji asks the database nothing.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn render_entities(
    pool: &PgPool,
    rows: &[bookmark_folder::BookmarkFolder],
) -> AppResult<Vec<BookmarkFolder>> {
    let codes = distinct_shortcodes(rows);
    if codes.is_empty() {
        return Ok(rows
            .iter()
            .map(|row| BookmarkFolder::from_row(row, None))
            .collect());
    }
    let urls = image_urls(pool, &codes).await?;
    Ok(rows
        .iter()
        .map(|row| {
            let url = custom_shortcode(row.emoji.as_deref()).and_then(|code| urls.get(code));
            BookmarkFolder::from_row(row, url.map(String::as_str))
        })
        .collect())
}

/// Renders one `row` as an entity — the single-row case of
/// [`render_entities`], and the shape the three writes answer.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn single_entity(
    pool: &PgPool,
    row: &bookmark_folder::BookmarkFolder,
) -> AppResult<BookmarkFolder> {
    let rows = [row.clone()];
    let entities = render_entities(pool, &rows).await?;
    Ok(entities
        .into_iter()
        .next()
        .unwrap_or_else(|| BookmarkFolder::from_row(row, None)))
}

/// The distinct local-custom shortcodes named by `rows`' emojis, in first-
/// appearance order — one batched read asks for all of them.
fn distinct_shortcodes(rows: &[bookmark_folder::BookmarkFolder]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut codes = Vec::new();
    for row in rows {
        if let Some(code) = custom_shortcode(row.emoji.as_deref())
            && seen.insert(code.to_owned())
        {
            codes.push(code.to_owned());
        }
    }
    codes
}

/// The image URLs of the local, enabled custom emojis named by `codes`,
/// keyed by shortcode. A remote or disabled emoji of the same name never
/// answers ([`custom_emoji::get_by_shortcodes`] asked with a `NULL` domain,
/// then the `disabled` rows dropped — Pleroma's `Emoji.get` reads the same
/// enabled-local store).
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn image_urls(pool: &PgPool, codes: &[String]) -> AppResult<HashMap<String, String>> {
    let found = custom_emoji::get_by_shortcodes(pool, codes, None).await?;
    Ok(found
        .into_iter()
        .filter(|emoji| !emoji.disabled)
        .map(|emoji| (emoji.shortcode, emoji.image_url))
        .collect())
}

/// The bare shortcode inside a `:shortcode:` folder emoji, `None` for a
/// unicode emoji or no emoji at all.
fn custom_shortcode(emoji: Option<&str>) -> Option<&str> {
    match emoji.map(Emoji::classify) {
        Some(Some(Emoji::Custom(code))) => Some(code),
        _ => None,
    }
}

/// The request parameters of `POST` and `PATCH`.
///
/// Both fields are `Option` at the serde layer: `POST` demands the name
/// ([`validate_name`]), `PATCH` changes only what is present (an absent
/// field stays `None` and keeps its stored column).
#[derive(Debug, Deserialize)]
struct FolderParams {
    /// The folder name; required on create, 1 to [`MAX_NAME_CHARS`]
    /// characters. A JSON `null` reads as absent, like the lists' title.
    #[serde(default)]
    name: Option<String>,
    /// The folder emoji; one unicode emoji or a `:shortcode:`, optional,
    /// and a JSON `null` likewise reads as absent.
    #[serde(default)]
    emoji: Option<String>,
}

/// The name, checked: blank (absent counts as blank on create) is 422
/// [`NAME_BLANK`], longer than [`MAX_NAME_CHARS`] characters is 422
/// [`NAME_TOO_LONG`]; a kept name is stored verbatim.
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn validate_name(name: String) -> AppResult<String> {
    if name.trim().is_empty() {
        return Err(AppError::Unprocessable(NAME_BLANK.to_owned()));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(AppError::Unprocessable(NAME_TOO_LONG.to_owned()));
    }
    Ok(name)
}

/// The emoji, checked: absent or null stays absent (`None`); anything that is
/// neither one unicode emoji nor a `:shortcode:` naming a local, enabled
/// custom emoji is 422 [`EMOJI_INVALID`] (Pleroma's `validate_emoji`). A kept
/// emoji is stored verbatim as the client sent it.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`EMOJI_INVALID`] as described; any
/// database failure arrives as [`AppError::Database`].
async fn validate_emoji(pool: &PgPool, emoji: Option<String>) -> AppResult<Option<String>> {
    let Some(text) = emoji else {
        return Ok(None);
    };
    match Emoji::classify(&text) {
        // A `:shortcode:` must name an emoji this instance actually serves.
        Some(Emoji::Custom(shortcode)) => {
            if local_enabled_emoji(pool, shortcode).await?.is_some() {
                Ok(Some(text))
            } else {
                Err(AppError::Unprocessable(EMOJI_INVALID.to_owned()))
            }
        }
        Some(Emoji::Unicode(_)) => Ok(Some(text)),
        None => Err(AppError::Unprocessable(EMOJI_INVALID.to_owned())),
    }
}

/// The local, enabled custom emoji `shortcode` names — the same single-
/// shortcode lookup [`crate::api::emoji_reactions`] makes for a reaction.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
async fn local_enabled_emoji(
    pool: &PgPool,
    shortcode: &str,
) -> AppResult<Option<custom_emoji::CustomEmoji>> {
    let wanted = [shortcode.to_owned()];
    let found = custom_emoji::get_by_shortcodes(pool, &wanted, None).await?;
    Ok(found.into_iter().find(|emoji| !emoji.disabled))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Validates `value`'s `name` as a create would: an absent name reaches
    /// the check as the empty string it defaults to.
    fn created_name(value: serde_json::Value) -> AppResult<String> {
        let params: FolderParams = crate::extract::into_params(value)?;
        validate_name(params.name.unwrap_or_default())
    }

    /// The 422 message a create with `value` is rejected on for its name.
    fn rejection(value: serde_json::Value) -> String {
        created_name(value)
            .expect_err("the fixture name is invalid")
            .to_string()
    }

    #[test]
    fn a_present_name_of_one_to_fifty_chars_passes() {
        assert_eq!(
            created_name(json!({"name": "Read later"})).expect("a plain name passes"),
            "Read later",
            "the name is kept verbatim"
        );
        let edge = "x".repeat(MAX_NAME_CHARS);
        assert_eq!(
            created_name(json!({"name": edge.clone()})).expect("fifty fit"),
            edge,
            "50 characters fit"
        );
    }

    #[test]
    fn an_absent_or_blank_name_is_refused() {
        assert_eq!(rejection(json!({})), NAME_BLANK, "absent is blank");
        for value in [json!(""), json!("   ")] {
            assert_eq!(
                rejection(json!({"name": value.clone()})),
                NAME_BLANK,
                "`{value}` is blank"
            );
        }
    }

    #[test]
    fn a_name_over_the_limit_is_refused() {
        let over = "x".repeat(MAX_NAME_CHARS + 1);
        assert_eq!(
            rejection(json!({"name": over})),
            NAME_TOO_LONG,
            "51 characters do not fit"
        );
    }

    #[test]
    fn a_non_ascii_name_is_counted_in_characters() {
        let dots = "…".repeat(MAX_NAME_CHARS);
        assert_eq!(
            created_name(json!({"name": dots.clone()})).expect("50 three-byte chars fit"),
            dots,
            "the limit counts characters, not bytes"
        );
    }

    #[test]
    fn the_params_deserialize_both_names_as_optional() {
        let empty: FolderParams =
            crate::extract::into_params(json!({})).expect("an empty body deserializes");
        assert!(empty.name.is_none(), "absent name is None");
        assert!(empty.emoji.is_none(), "absent emoji is None");

        let full: FolderParams = crate::extract::into_params(json!({
            "name": "n",
            "emoji": ":cat:",
        }))
        .expect("a full body deserializes");
        assert_eq!(full.name.as_deref(), Some("n"), "the name came through");
        assert_eq!(full.emoji.as_deref(), Some(":cat:"), "and so did the emoji");
    }

    /// Classifies `text` the way the endpoint's emoji gate reads it: whether
    /// it is the custom-shortcode shape, and the shortcode it names.
    fn custom(text: &str) -> Option<&str> {
        custom_shortcode(Some(text))
    }

    #[test]
    fn a_shortcode_is_the_lookup_key_a_unicode_emoji_is_not() {
        assert_eq!(
            custom(":cat:"),
            Some("cat"),
            "the colons are stripped for the lookup"
        );
        assert_eq!(custom("📚"), None, "a unicode emoji names no custom image");
        assert_eq!(custom("cat"), None, "a bare word is not a shortcode shape");
    }
}
