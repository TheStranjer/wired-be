//! The account's own view of itself and its profile write:
//! `GET /api/v1/accounts/verify_credentials` and
//! `PATCH /api/v1/accounts/update_credentials` with the request shapes
//! they read. The merge of a submitted body over the stored row is
//! [`credentials_merge`] and [`credentials_pleroma`]'s, the profile
//! images are [`profile_images`] and [`profile_image_store`]'s, and the
//! name, note and field renders are [`profile_text`]'s.
//!
//! [`credentials_merge`]: super::credentials_merge
//! [`credentials_pleroma`]: super::credentials_pleroma
//! [`profile_images`]: super::profile_images
//! [`profile_image_store`]: super::profile_image_store
//! [`profile_text`]: super::profile_text
//!
//! `verify_credentials` is the account's own view of itself: [`AuthUser`]
//! plus the `read:accounts` scope, and the entity rendered with the
//! caller as its [`AccountViewer`], so the owner-only keys of the
//! `pleroma` block are in and nothing is hidden. Its additions over `show`
//! are the [`source`](crate::entities::account::AccountSource) object — the
//! raw note and fields a client edits, the posting defaults, and the size
//! of the pending follow-request queue, the one value here that is not on
//! the row and so arrives from
//! [`pending_count`](crate::models::follow::pending_count) — and the
//! top-level `also_known_as` alias list the account manages through
//! `/api/pleroma/aliases` (see
//! [`crate::api::pleroma_account_settings`]). Pleroma gates the `source`
//! block on the viewer being the account itself
//! (`AccountView.maybe_put_settings`), which is why no other route on this
//! module carries it.
//!
//! `PATCH /api/v1/accounts/update_credentials` (and the `POST` alias older
//! clients send) is Pleroma's `AccountController.update_credentials`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`):
//! [`AuthUser`] with the `write:accounts` scope, and only the fields the
//! body actually carries are written — [`MultipartForm`] parses JSON,
//! urlencoded and multipart bodies alike (decision 10) and every parameter
//! here is optional, a JSON `null` counting as absent as in Pleroma's
//! `Enum.filter(fn {_, value} -> not is_nil(value) end)`. The three
//! profile-image parameters are the exception to that rule: Pleroma's
//! `user_image_value` reads their empty string as "reset", so an empty
//! string or a JSON `null` *clears* `avatar`, `header` or
//! `pleroma_background_image` while an absent one keeps the stored image.
//! `display_name`
//! is length-checked against
//! [`max_display_name_chars`](Config::max_display_name_chars) (422 with
//! Mastodon's wording on overflow) and stored through
//! [`sanitize_display_name`]; an empty one is stored empty, not reset to
//! the nickname. `note` is length-checked against
//! [`max_bio_chars`](Config::max_bio_chars), stored verbatim as the row's
//! `raw_bio` and rendered to `bio` by [`text_to_html`] plus
//! [`linkify_entities`] — bare-URL links, hashtag links, and mention
//! links only for mentions naming a local user of this instance (unknown
//! and remote mentions stay plain text). `locked`, `bot` and
//! `discoverable` are [`flex::opt_bool`] booleans; an unrecognized
//! boolean string is the extractor's 422. `fields_attributes` is the
//! profile metadata: a JSON array of `{name, value}` objects, an index
//! map (`{"0": {…}}`) or the form spelling
//! `fields_attributes[0][name]=…` alike — [`index_map_to_vec`] folds all
//! three shapes into the submitted list (see `stored_fields`). An
//! entry whose name and value are both empty is dropped; more than
//! [`max_profile_fields`](Config::max_profile_fields) remaining entries
//! are the 422 "Too many fields", and a name over
//! `MAX_PROFILE_FIELD_NAME_CHARS` or a value over
//! `MAX_PROFILE_FIELD_VALUE_CHARS` characters is a length 422. Two
//! jsonb columns store the pair: `raw_fields` keeps the entries as
//! typed, `fields` the names reduced to plain text and the values
//! rendered by [`text_to_html`] — so a bare URL becomes a link with the
//! standard `rel`. An empty submission clears the metadata. The three
//! profile images — the `avatar`, `header` and `pleroma_background_image`
//! file parts, `users.avatar_url`, `banner_url` and `background_image_url`
//! on this side — are stored through [`crate::uploads`] under a
//! [`media_key`](uploads::media_key) of the caller's id, and their columns
//! hold the [`public_url`](uploads::UploaderBackend::public_url) of what
//! was stored, the address [`crate::api::media_files`] serves the bytes
//! from. What a part's bytes name decides everything: only `png`, `jpeg`,
//! `gif` and `webp` are profile images (any other bytes, and any other
//! type the client's part header claimed, are the 422 "Unsupported file
//! type"), and a part over [`avatar_limit_bytes`](Config::avatar_limit_bytes)
//! for the avatar or [`banner_limit_bytes`](Config::banner_limit_bytes) for
//! the banner and the background is the 422 "File is too large". Every slot
//! is checked before any file is stored, so a body with one good image and
//! one bad one changes nothing; the file a replacement or a clear left
//! behind is removed best-effort once the row stopped naming it.
//! `avatar_description` and `header_description` are accepted and ignored.
//!

use axum::{Json, extract::State};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::{
    auth::AuthUser,
    entities::account::Account,
    error::{AppError, AppResult},
    extract::{flex, multipart::MultipartForm},
    models::{
        follow,
        user::{self, User},
    },
    oauth::scopes::{READ_ACCOUNTS, WRITE_ACCOUNTS},
    services::account_emojis::AccountEmojis,
    state::AppState,
};

use super::{
    credentials_merge::profile_fields, profile_image_store::delete_replaced_files,
    profile_images::resolve_images, profile_text::after_profile_updated,
};

/// `GET /api/v1/accounts/verify_credentials` — the caller's own account,
/// rendered as they see it: the owner view of [`Account::from_user_for`]
/// (hidden counters visible, the owner-only `pleroma` keys present) with
/// the `source` block [`Account::for_owner`] attaches.
///
/// The only thing that block needs beyond the token's row is the pending
/// follow-request queue, counted by
/// [`pending_count`](crate::models::follow::pending_count).
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token is app-only or does not
/// grant `read:accounts`. Any database failure arrives through `?` as
/// [`AppError::Database`].
pub(super) async fn verify_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Account>> {
    auth.require(READ_ACCOUNTS)?;
    owner_view(&state, &auth.0).await
}

/// The owner render both credentials endpoints answer — the self view of
/// [`Account::for_owner`] with its [`source`](Account::source) block (the
/// pending queue counted by
/// [`pending_count`](crate::models::follow::pending_count) included) and a
/// stored move target attached as the nested `moved` entity (see
/// [`with_moved`]).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn owner_view(state: &AppState, owner: &User) -> AppResult<Json<Account>> {
    let requests = follow::pending_count(&state.db, owner.id).await?;
    let account = Account::for_owner(owner, &state.config, requests);
    let emojis = AccountEmojis::load_one(&state.db, owner).await?;
    let account = emojis.fill(account, owner, &state.config);
    Ok(Json(with_moved(state, account, owner).await?))
}

/// Resolves [`moved_to_ap_id`](User::moved_to_ap_id) for one rendered
/// [`Account`]: when the row names a move target this instance has a row
/// for, [`Account::with_moved`] nests the target's plain entity. A move
/// to an actor id not mirrored here keeps `moved: null` (the alias and
/// move flows are what create that row; federation of a remote `Move`
/// arrives with a later ticket).
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn with_moved(
    state: &AppState,
    account: Account,
    user: &User,
) -> AppResult<Account> {
    let Some(ap_id) = user.moved_to_ap_id.as_deref() else {
        return Ok(account);
    };
    Ok(match user::get_by_ap_id(&state.db, ap_id).await? {
        Some(target) => account.with_moved(&target, &state.config),
        None => account,
    })
}

/// The request parameters of `PATCH /api/v1/accounts/update_credentials`.
///
/// Every field is optional and only the ones present in the body are
/// written; a JSON `null` reads as absent (Pleroma drops nil-valued
/// parameters before the changeset). The booleans arrive through
/// [`flex::opt_bool`], so a form's `"true"`/`"on"`/`"1"` spells the same
/// as JSON `true`, and an unknown string is the extractor's 422. That
/// covers the eleven Pleroma preference flags and `source[sensitive]` as
/// well as the three Mastodon booleans.
///
/// The `source` block is [`SourceParams`], read from the form's
/// `source[privacy]=…` keys or the JSON object of the same shape;
/// `default_scope` is Pleroma's flat spelling of `source[privacy]` and
/// loses to it when a body carries both. `birthday` is `YYYY-MM-DD` text
/// ([`merged_birthday`]), `pleroma_settings_store` the JSON object — or,
/// from a form, the JSON *text* of one — that merges into the stored
/// settings blob ([`merged_store`]), and `actor_type` the actor kind
/// [`merged_actor`] pairs with `bot`.
///
/// The three profile images are the exception to the `null` rule, and so are
/// read through [`kept_value`] as raw [`Value`]s: Pleroma's
/// `user_image_value` treats their empty string as the reset, so an
/// *absent* parameter keeps the stored image while `null` or `""` clears it
/// (see [`resolve_images`]). `avatar_description` and `header_description`
/// are not fields of this shape on purpose: serde drops unknown keys, so
/// both are accepted and ignored, as this endpoint's ticket asks.
#[derive(Debug, Deserialize)]
pub(super) struct ProfileParams {
    /// The display name to store: at most
    /// [`max_display_name_chars`](Config::max_display_name_chars)
    /// characters, then reduced to one line of plain text (see
    /// [`sanitized_name`]).
    #[serde(default)]
    pub(super) display_name: Option<String>,
    /// The profile note: source text for `raw_bio`, at most
    /// [`max_bio_chars`](Config::max_bio_chars) characters, rendered to
    /// `bio` (see [`render_note`]).
    #[serde(default)]
    pub(super) note: Option<String>,
    /// Whether follow requests need manual approval.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) locked: Option<bool>,
    /// Whether the account is flagged as automated; the other half of the
    /// pair [`merged_actor`] writes.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) bot: Option<bool>,
    /// Whether the account opts into discovery.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) discoverable: Option<bool>,
    /// The `source` block of Mastodon's account entity, sent as
    /// `source[privacy]`/`source[sensitive]`/`source[language]` form keys or
    /// as the nested JSON object of the same shape (see [`SourceParams`]).
    #[serde(default)]
    pub(super) source: Option<SourceParams>,
    /// Pleroma's flat spelling of `source[privacy]`: the visibility new
    /// statuses default to, checked against [`SCOPES`].
    #[serde(default)]
    pub(super) default_scope: Option<String>,
    /// Pleroma's `pleroma_settings_store`: a JSON object (or its JSON text,
    /// from a form) merged key by key into the stored settings blob.
    #[serde(default)]
    pub(super) pleroma_settings_store: Option<Value>,
    /// The `ActivityPub` actor kind, one of [`ACTOR_TYPES`]; wins over
    /// [`bot`](Self::bot) and decides it (see [`merged_actor`]).
    #[serde(default)]
    pub(super) actor_type: Option<String>,
    /// The account's birthday as `YYYY-MM-DD` text, or the empty string to
    /// clear it (see [`merged_birthday`]).
    #[serde(default)]
    pub(super) birthday: Option<String>,
    /// Whether the follower collection is hidden from other accounts.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) hide_followers: Option<bool>,
    /// Whether the follow collection is hidden from other accounts.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) hide_follows: Option<bool>,
    /// Whether the follower count renders as zero for others.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) hide_followers_count: Option<bool>,
    /// Whether the follow count renders as zero for others.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) hide_follows_count: Option<bool>,
    /// Whether the favourites list is private to the account.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) hide_favorites: Option<bool>,
    /// Whether the admin/moderator flags may be shown to others.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) show_role: Option<bool>,
    /// Whether this account's statuses skip thread containment.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) skip_thread_containment: Option<bool>,
    /// Whether a `Move` of the account may take the following list with it.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) allow_following_move: Option<bool>,
    /// Whether the account accepts chat messages.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) accepts_chat_messages: Option<bool>,
    /// Whether clients should render this account's content as plain text.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) no_rich_text: Option<bool>,
    /// Whether strangers may see the stored birthday.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) show_birthday: Option<bool>,
    /// The profile metadata: Mastodon's `fields_attributes`, a JSON array
    /// of `{name, value}` objects, an object keyed by index or the
    /// `fields_attributes[0][name]=…` spelling of form and multipart
    /// bodies alike — read as `Value` so every shape survives the
    /// extractor and [`stored_fields`] folds them into one list.
    #[serde(default)]
    pub(super) fields_attributes: Option<Value>,
    /// The avatar: a `multipart/form-data` file part named `avatar`, or the
    /// text/JSON spelling that clears the stored one.
    #[serde(default, deserialize_with = "kept_value")]
    pub(super) avatar: Option<Value>,
    /// The profile banner, Mastodon's name for what Pleroma stores in
    /// `banner_url`.
    #[serde(default, deserialize_with = "kept_value")]
    pub(super) header: Option<Value>,
    /// The profile background image, Pleroma's own extension.
    #[serde(default, deserialize_with = "kept_value")]
    pub(super) pleroma_background_image: Option<Value>,
}

/// The `source` object of an `update_credentials` body: Mastodon's editing
/// half of the account entity, which Pleroma reads back into the posting
/// defaults. A form carries its keys as `source[privacy]=…`, which the
/// body parser nests into exactly this shape; a JSON body carries the object
/// itself.
#[derive(Debug, Deserialize)]
pub(super) struct SourceParams {
    /// The visibility new statuses default to, checked against [`SCOPES`]
    /// (the `users.default_scope` column).
    #[serde(default)]
    pub(super) privacy: Option<String>,
    /// Whether new statuses are flagged sensitive by default (the
    /// `users.sensitive_default` column).
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) sensitive: Option<bool>,
    /// The BCP 47 tag new statuses default to, at most
    /// [`MAX_LANGUAGE_CHARS`] characters and cleared by the empty string
    /// (the `users.language` column).
    #[serde(default)]
    pub(super) language: Option<String>,
}

/// Serde field helper for a parameter that must tell an absent key from a
/// JSON `null`: `deserialize_with` runs only when the key is there, and
/// [`Value`] keeps `null` as [`Value::Null`] instead of folding it into
/// absence the way `Option<T>` does. Paired with `#[serde(default)]` the
/// field is `None` for a key the body never carried and
/// `Some(Value::Null)` for one it carried as `null` — the difference the
/// profile-image clears depend on.
///
/// # Errors
///
/// Whatever the underlying [`Value`] deserializer reports (which, for the
/// JSON and form bodies the extractors hand over, is nothing: any value
/// becomes a [`Value`]).
fn kept_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// `PATCH /api/v1/accounts/update_credentials` (and the `POST` alias of
/// the older clients) — change the caller's own profile, the fields the
/// body carries and nothing else, answering the updated account as
/// [`verify_credentials`] answers it.
///
/// [`MultipartForm`] takes JSON, urlencoded and multipart bodies alike
/// (decision 10), its file parts arriving beside the text parameters: the
/// three profile images are the file parts this endpoint reads
/// ([`resolve_images`]), every other file part is ignored.
/// [`profile_fields`] validates what the body carries and merges it over the
/// token's row, the image writes land in the same merged struct after
/// [`ImageWrites::apply`] stored their files, and
/// [`update_profile`](crate::models::user::update_profile) writes every
/// profile column in one statement. The fresh row answers through
/// [`owner_view`] —
/// the write-behind render that states the stored fact rather than trusting
/// the request — and only after it does [`delete_replaced_files`] remove the
/// files the write replaced. A body that carries none of these fields is the
/// accepted no-op: 200 with the unchanged entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::Unprocessable`] when
/// `display_name` or `note` is too long, when `fields_attributes` breaks
/// one of its limits, when `source[privacy]` (or the flat `default_scope`)
/// names no visibility, when `source[language]` is over
/// [`MAX_LANGUAGE_CHARS`] characters, when `actor_type` is neither `Person`
/// nor `Service`, when `birthday` is no `YYYY-MM-DD` date or a future one,
/// when `pleroma_settings_store` is no JSON object or the merged store passes
/// the 1 MiB cap, when a profile image is not a `png`, `jpeg`, `gif` or
/// `webp` file or when one passes its configured byte limit, and from the
/// extractor for an invalid boolean string or a file over
/// `UPLOAD_LIMIT_BYTES` (413). Storage failures arrive as
/// [`AppError::Internal`], any database failure through `?` as
/// [`AppError::Database`], and the [`AppError::Internal`] arm of the write
/// only documents that the row behind a just-authenticated token is gone.
pub(super) async fn update_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
    form: MultipartForm<ProfileParams>,
) -> AppResult<Json<Account>> {
    auth.require(WRITE_ACCOUNTS)?;
    let MultipartForm { fields, mut files } = form;
    let images = resolve_images(&state.config, &mut files, &fields)?;
    let mut profile = profile_fields(&state, &auth.0, fields).await?;
    let applied = images.apply(&state, auth.0.id, &auth.0).await?;
    profile.avatar_url = applied.avatar_url;
    profile.banner_url = applied.banner_url;
    profile.background_image_url = applied.background_image_url;
    let updated = user::update_profile(&state.db, auth.0.id, &profile)
        .await?
        .ok_or_else(|| AppError::Internal("profile update matched no row".to_owned()))?;
    delete_replaced_files(&state, applied.replaced).await;
    after_profile_updated(&state, &updated).await;
    owner_view(&state, &updated).await
}

#[cfg(test)]
mod tests {
    use crate::api::accounts::test_support::{no_params, params};

    use super::*;

    #[test]
    fn kept_value_tells_absent_from_null() {
        let absent = no_params();
        assert_eq!(absent.avatar, None, "an absent key is no parameter");
        let nulled = params(r#"{"avatar": null}"#);
        assert_eq!(nulled.avatar, Some(Value::Null), "a JSON null is one");
        let blank = params(r#"{"avatar": ""}"#);
        assert_eq!(
            blank.avatar,
            Some(Value::String(String::new())),
            "and so is the form spelling"
        );
    }

    /// The `source` block of a body, `unwrap`ing the parse.
    fn source_params(text: &str) -> ProfileParams {
        params(&format!(r#"{{"source": {text}}}"#))
    }

    #[test]
    fn the_source_block_reads_both_spellings() {
        let json_body = source_params(r#"{"privacy": "unlisted", "sensitive": "1"}"#);
        let nested = json_body
            .source
            .as_ref()
            .expect("the JSON object is the block");
        assert_eq!(nested.privacy.as_deref(), Some("unlisted"), "privacy");
        assert_eq!(nested.sensitive, Some(true), "the string spelling coerces");
        assert_eq!(nested.language, None, "an absent key is no parameter");

        let form = crate::extract::parse_pairs([
            ("source[privacy]", "direct"),
            ("source[sensitive]", "off"),
            ("source[language]", "de"),
        ])
        .expect("the keys nest shallowly");
        let from_form: ProfileParams =
            serde_json::from_value(form).expect("the form nests into the same shape");
        let block = from_form
            .source
            .as_ref()
            .expect("`source[privacy]` is the block");
        assert_eq!(block.privacy.as_deref(), Some("direct"), "the form privacy");
        assert_eq!(block.sensitive, Some(false), "the form sensitive");
        assert_eq!(block.language.as_deref(), Some("de"), "the form language");
    }
}
