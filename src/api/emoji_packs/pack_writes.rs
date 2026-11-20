//! The admin pack writes: creating, updating the metadata of and deleting
//! a pack, plus the helpers every write route shares (the name check and
//! the `custom_emojis` resync).

use axum::{Json, extract::State};
use serde::Deserialize;
use tracing::debug;

use super::{AREA_EMOJI, CLASS_WRITE, NAME_EMPTY, OK_BODY, params::PackParams};
use crate::{
    auth::admin::AdminUser,
    error::{AppError, AppResult},
    extract::{ApiParams, flex},
    models::moderation_log,
    services::emoji_packs,
    state::AppState,
};

/// The `name` parameter of the create route, from the query string or the
/// body ([`ApiParams`] merges the two, the body wins); the handler trims
/// and demands it, as the reads' [`PackParams::required_name`] does.
#[derive(Debug, Deserialize)]
pub(super) struct CreateParams {
    /// Raw pack `name` value, if present.
    #[serde(default)]
    name: Option<String>,
}

/// The `name` parameter and `metadata` body of the update route.
#[derive(Debug, Deserialize)]
pub(super) struct UpdateParams {
    /// Raw pack `name` value, if present.
    #[serde(default)]
    name: Option<String>,
    /// The metadata to merge into the pack's manifest block; required, as
    /// Pleroma's `update` pattern-matches on it.
    metadata: MetadataPatch,
}

/// A `metadata` body: the keys of Pleroma's `update_request` schema, every
/// one optional. The dashed names are [`PackMeta`]'s manifest spellings;
/// `share-files` reads through [`flex::opt_bool`], so a form-encoded
/// `"true"` counts and anything that is no boolean at all is refused as
/// the extractor's 422.
///
/// [`PackMeta`]: emoji_packs::PackMeta
#[derive(Debug, Deserialize)]
struct MetadataPatch {
    /// The pack's license identifier.
    #[serde(default)]
    license: Option<String>,
    /// The pack's home page URL.
    #[serde(default)]
    homepage: Option<String>,
    /// Free-form description of the pack.
    #[serde(default)]
    description: Option<String>,
    /// URL of an external archive holding the pack's files; stored as
    /// given, never fetched.
    #[serde(default, rename = "fallback-src")]
    fallback_src: Option<String>,
    /// SHA-256 of the fallback archive, lowercase hex.
    #[serde(default, rename = "fallback-src-sha256")]
    fallback_src_sha256: Option<String>,
    /// Whether the pack may hand its files to others.
    #[serde(default, rename = "share-files", deserialize_with = "flex::opt_bool")]
    share_files: Option<bool>,
}

/// Merges `patch` into stored metadata: every key the request carried
/// replaces what stands, every key it left out keeps its stored value.
fn merge_metadata(into: &mut emoji_packs::PackMeta, patch: MetadataPatch) {
    if let Some(license) = patch.license {
        into.license = Some(license);
    }
    if let Some(homepage) = patch.homepage {
        into.homepage = Some(homepage);
    }
    if let Some(description) = patch.description {
        into.description = Some(description);
    }
    if let Some(fallback_src) = patch.fallback_src {
        into.fallback_src = Some(fallback_src);
    }
    if let Some(fallback_src_sha256) = patch.fallback_src_sha256 {
        into.fallback_src_sha256 = Some(fallback_src_sha256);
    }
    if let Some(share_files) = patch.share_files {
        into.share_files = share_files;
    }
}

/// The trimmed pack `name` a write route received from the query or the
/// body; absent or blank is Pleroma's empty-value 400, shared with the
/// reads' [`PackParams::required_name`].
pub(super) fn trimmed_name(raw: Option<&str>) -> AppResult<&str> {
    let name = raw.unwrap_or_default().trim();
    if name.is_empty() {
        return Err(AppError::BadRequest(NAME_EMPTY.to_owned()));
    }
    Ok(name)
}

/// The 404 the write routes owe a name they will not serve, Pleroma's
/// `"Pack <name> does not exist"` (the controller's `:enoent` arms). Like
/// the reads' shared 404 it covers an absent pack and an unusable name
/// alike, so a write cannot probe the filesystem either.
fn pack_missing(name: &str) -> AppError {
    AppError::NotFoundMessage(format!("Pack {name} does not exist"))
}

/// Folds a service refusal that means "there is no pack here to write" —
/// [`AppError::NotFound`] for an absent directory, [`AppError::BadRequest`]
/// for a name [`emoji_packs::pack_name_valid`] rejects — into
/// [`pack_missing`]; every other failure arrives unchanged.
fn write_miss(name: &str, err: AppError) -> AppError {
    if matches!(err, AppError::NotFound | AppError::BadRequest(_)) {
        return pack_missing(name);
    }
    err
}

/// Loads pack `name` for a write, refusing with [`pack_missing`] what is
/// not there to write.
async fn load_for_write(state: &AppState, name: &str) -> AppResult<emoji_packs::Pack> {
    let loaded = emoji_packs::load_pack(&state.config.emoji_dir, name).await;
    loaded.map_err(|err| write_miss(name, err))
}

/// Reconciles `custom_emojis` with the packs directory after a write, so
/// `GET /api/v1/custom_emojis` and the util emoji map see the change.
///
/// # Errors
///
/// Whatever [`sync_custom_emojis`](emoji_packs::sync_custom_emojis) fails
/// with; its stored count is logged, not answered.
pub(super) async fn resync(state: &AppState) -> AppResult<()> {
    let stored: usize =
        emoji_packs::sync_custom_emojis(&state.db, &state.config.base_url, &state.config.emoji_dir)
            .await?;
    debug!(emojis = stored, "emoji pack write resynced custom emojis");
    Ok(())
}

/// `POST /api/v1/pleroma/emoji/pack?name=…` — Pleroma's `create`: the
/// pack's directory and a default `{"pack": {}, "files": {}}` manifest,
/// the name also accepted in the body. An occupied name answers the
/// ticket's 400 `{"error":"Pack already exists"}` (Pleroma's 409), an
/// absent or blank one its 400 `"pack name cannot be empty"`. On success
/// the emojis resync and an `emoji_pack_create` moderation entry records
/// the act; the answer is the JSON string `"ok"`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] from the name
/// checks; [`AppError::Internal`] for a filesystem failure;
/// [`AppError::Database`] from the resync and the log write.
pub(super) async fn create(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiParams(params): ApiParams<CreateParams>,
) -> AppResult<Json<&'static str>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let name = trimmed_name(params.name.as_deref())?;
    emoji_packs::create_pack(&state.config.emoji_dir, name).await?;
    resync(&state).await?;
    let entry = moderation_log::emoji_pack_create(&state.db, &admin.0, name).await?;
    debug!(id = %entry.id, pack = name, "emoji pack created");
    Ok(Json(OK_BODY))
}

/// `PATCH /api/v1/pleroma/emoji/pack?name=…` — Pleroma's `update`: the
/// body's `metadata` merged into the pack manifest's `pack` block, absent
/// keys keeping their stored values. A `fallback-src` is stored as given —
/// the archive is never fetched or checked here — and `share-files` must
/// be a boolean. An unknown or unusable pack is the 404
/// `{"error":"Pack <name> does not exist"}`. On success the emojis resync,
/// an `emoji_pack_update` moderation entry records the act, and the
/// resulting metadata object answers.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for an absent or
/// blank `name`; [`AppError::Unprocessable`] for a non-object `metadata`
/// or a non-boolean `share-files` (the [`ApiParams`] extractor's refusals);
/// [`pack_missing`]'s 404 for a pack that is not there to write;
/// [`AppError::Internal`] for a filesystem failure; [`AppError::Database`]
/// from the resync and the log write.
pub(super) async fn update(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiParams(params): ApiParams<UpdateParams>,
) -> AppResult<Json<emoji_packs::PackMeta>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let name = trimmed_name(params.name.as_deref())?;
    let mut pack = load_for_write(&state, name).await?;
    merge_metadata(&mut pack.pack, params.metadata);
    emoji_packs::save_pack(&state.config.emoji_dir, name, &pack).await?;
    resync(&state).await?;
    let entry = moderation_log::emoji_pack_update(&state.db, &admin.0, name).await?;
    debug!(id = %entry.id, pack = name, "emoji pack metadata updated");
    Ok(Json(pack.pack))
}

/// `DELETE /api/v1/pleroma/emoji/pack?name=…` — Pleroma's `delete`: the
/// pack's whole directory, files and manifest, removed from `EMOJI_DIR`;
/// an unusable name can never name a directory and answers the same 404
/// as an absent pack. On success the emojis resync — the pack's shortcodes
/// leave `custom_emojis` with it — an `emoji_pack_delete` moderation entry
/// records the act, and the answer is the JSON string `"ok"`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for an absent or
/// blank `name`; [`pack_missing`]'s 404 for a pack that is not there;
/// [`AppError::Internal`] for a filesystem failure; [`AppError::Database`]
/// from the resync and the log write.
pub(super) async fn destroy(
    State(state): State<AppState>,
    admin: AdminUser,
    params: PackParams,
) -> AppResult<Json<&'static str>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let name = params.required_name()?;
    let removed = emoji_packs::delete_pack(&state.config.emoji_dir, name).await;
    removed.map_err(|err| write_miss(name, err))?;
    resync(&state).await?;
    let entry = moderation_log::emoji_pack_delete(&state.db, &admin.0, name).await?;
    debug!(id = %entry.id, pack = name, "emoji pack deleted");
    Ok(Json(OK_BODY))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_metadata_patch_reads_pleromas_dashed_names() {
        let patch: MetadataPatch = serde_json::from_value(json!({
            "license": "MIT",
            "homepage": "https://emoji.example",
            "description": "Cats",
            "fallback-src": "https://emoji.example/cats.zip",
            "fallback-src-sha256": "beef",
            "share-files": true,
        }))
        .unwrap();
        assert_eq!(patch.license.as_deref(), Some("MIT"));
        assert_eq!(patch.homepage.as_deref(), Some("https://emoji.example"));
        assert_eq!(patch.description.as_deref(), Some("Cats"));
        assert_eq!(
            patch.fallback_src.as_deref(),
            Some("https://emoji.example/cats.zip"),
            "the dashed key renamed"
        );
        assert_eq!(patch.fallback_src_sha256.as_deref(), Some("beef"));
        assert_eq!(patch.share_files, Some(true), "a real boolean");

        let form: MetadataPatch = serde_json::from_value(json!({"share-files": "off"})).unwrap();
        assert_eq!(
            form.share_files,
            Some(false),
            "the form spelling reads through flex"
        );

        let empty: MetadataPatch = serde_json::from_value(json!({})).unwrap();
        assert!(
            empty.license.is_none()
                && empty.homepage.is_none()
                && empty.description.is_none()
                && empty.fallback_src.is_none()
                && empty.fallback_src_sha256.is_none()
                && empty.share_files.is_none(),
            "an empty patch carries no key to merge"
        );
    }

    #[test]
    fn metadata_merge_replaces_present_keys_and_keeps_the_rest() {
        let mut stored = emoji_packs::PackMeta {
            license: Some("CC0-1.0".to_owned()),
            homepage: Some("https://old.example".to_owned()),
            description: Some("keep me".to_owned()),
            fallback_src: Some("https://old.example/old.zip".to_owned()),
            fallback_src_sha256: Some("dead".to_owned()),
            share_files: true,
        };
        let patch: MetadataPatch = serde_json::from_value(json!({
            "license": "MIT",
            "fallback-src": "https://new.example/new.zip",
            "share-files": false,
        }))
        .unwrap();
        merge_metadata(&mut stored, patch);
        assert_eq!(
            stored.license.as_deref(),
            Some("MIT"),
            "present key replaced"
        );
        assert_eq!(
            stored.fallback_src.as_deref(),
            Some("https://new.example/new.zip"),
            "the new fallback stored as given"
        );
        assert_eq!(
            stored.fallback_src_sha256.as_deref(),
            Some("dead"),
            "the sha the patch left out stands"
        );
        assert!(!stored.share_files, "present boolean replaced");
        assert_eq!(
            (stored.homepage.as_deref(), stored.description.as_deref()),
            (Some("https://old.example"), Some("keep me")),
            "untouched keys keep their values"
        );
    }

    #[test]
    fn a_fallback_src_without_its_sha_still_merges_alone() {
        let mut stored = emoji_packs::PackMeta::default();
        let patch: MetadataPatch =
            serde_json::from_value(json!({"fallback-src": "https://x.test/p.zip"})).unwrap();
        merge_metadata(&mut stored, patch);
        assert_eq!(
            stored.fallback_src.as_deref(),
            Some("https://x.test/p.zip"),
            "the url is stored without any fetch"
        );
        assert_eq!(
            stored.fallback_src_sha256, None,
            "and no sha appears on its own"
        );
    }

    #[test]
    fn write_names_are_trimmed_and_required_like_the_reads() {
        assert_eq!(
            trimmed_name(Some("  cofe  ")).unwrap(),
            "cofe",
            "the name is trimmed"
        );
        for raw in [None, Some(""), Some("   ")] {
            let err = trimmed_name(raw).unwrap_err();
            assert_eq!(err.to_string(), NAME_EMPTY, "absent or blank is a 400");
        }
    }

    #[test]
    fn the_write_404_names_the_missing_pack_and_absorbs_the_service_400s() {
        assert_eq!(
            pack_missing("ghost").to_string(),
            "Pack ghost does not exist",
            "Pleroma's wording, name included"
        );
        let err = write_miss("ghost", AppError::NotFound);
        assert!(
            matches!(err, AppError::NotFoundMessage(_)),
            "an absent pack is the named 404"
        );
        let bad_name = write_miss("../x", AppError::BadRequest("invalid pack name".to_owned()));
        assert_eq!(
            bad_name.to_string(),
            "Pack ../x does not exist",
            "an unusable name folds into the same 404"
        );
        let internal = write_miss("p", AppError::Internal("disk full".to_owned()));
        assert!(
            matches!(internal, AppError::Internal(_)),
            "a real server failure is not a 404"
        );
    }

    #[test]
    fn the_create_body_also_carries_the_name() {
        let params: CreateParams = serde_json::from_value(json!({"name": "from-body"})).unwrap();
        assert_eq!(params.name.as_deref(), Some("from-body"));
        let none: CreateParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(
            none.name, None,
            "no name in an empty body: the query has it"
        );
    }

    #[test]
    fn the_update_body_needs_a_metadata_object() {
        let with: UpdateParams = serde_json::from_value(json!({
            "name": "cats",
            "metadata": {"share-files": true},
        }))
        .unwrap();
        assert_eq!(with.name.as_deref(), Some("cats"));
        assert_eq!(with.metadata.share_files, Some(true));
        let err = serde_json::from_value::<UpdateParams>(json!({"name": "cats"}))
            .expect_err("metadata is required");
        assert!(
            err.to_string().contains("metadata"),
            "the error names it: {err}"
        );

        let not_a_bool = serde_json::from_value::<MetadataPatch>(json!({"share-files": "maybe"}))
            .expect_err("share-files must be a boolean");
        assert!(
            not_a_bool.to_string().contains("boolean"),
            "the refusal says why: {not_a_bool}"
        );
    }
}
