//! The admin file writes under `/packs/files`: adding an emoji file to a
//! pack (uploaded or fetched), renaming an entry and deleting one.

use std::collections::BTreeMap;

use axum::{Json, extract::State};
use bytes::Bytes;
use serde_json::Value;
use tracing::debug;

use super::{
    AREA_EMOJI, CLASS_WRITE, FILE_FIELD, MAX_EMOJI_FILE_BYTES, MAX_FETCH_STATUS, MIN_FETCH_STATUS,
    MISSING_FILE, RENAME_EMPTY, UNSUPPORTED_FILE_TYPE,
    file_checks::{
        CreateFileParams, DeleteFileParams, Rename, UpdateFileParams, bad_file_name, bad_shortcode,
        detect_emoji_type, emoji_missing, file_too_large, load_for_file_write, rename_miss,
        shortcode_in_use, shortcode_taken, shortcode_valid, target_filename,
    },
    pack_writes::{resync, trimmed_name},
    params::PackParams,
};
use crate::{
    auth::admin::AdminUser,
    error::{AppError, AppResult},
    extract::{
        ApiParams,
        multipart::{MultipartForm, UploadedFile, UploadedFiles as _},
    },
    http_client::HttpError,
    services::emoji_packs,
    state::AppState,
};

/// The 422 for a fetch that never delivered a body to read: the client
/// named a URL this instance will not (SSRF guard) or cannot reach.
pub(super) fn fetch_refused(err: &HttpError) -> AppError {
    AppError::Unprocessable(format!("Could not fetch the file URL: {err}"))
}

/// Fetches the `file` URL with the state's [`HttpClient`] — the
/// SSRF-guarded client that refuses loopback, private and reserved
/// targets unless `HTTP_ALLOW_PRIVATE_HOSTS` — with the body read capped
/// at [`MAX_EMOJI_FILE_BYTES`], and insists on a 2xx answer whose bytes
/// name an accepted image type.
///
/// [`HttpClient`]: crate::http_client::HttpClient
///
/// # Errors
///
/// [`AppError::PayloadTooLarge`] when the response passes the cap,
/// [`AppError::Unprocessable`] for a refused or failed fetch, a status
/// outside 2xx, and bytes no accepted image type matches.
async fn fetch_emoji_file(state: &AppState, url: &str) -> AppResult<(Bytes, &'static str)> {
    let response = state
        .http_client
        .get_limited(url, &[], MAX_EMOJI_FILE_BYTES)
        .await
        .map_err(|err| fetch_refused(&err))?;
    if !(MIN_FETCH_STATUS..MAX_FETCH_STATUS).contains(&response.status) {
        return Err(AppError::Unprocessable(format!(
            "File URL answered {}",
            response.status
        )));
    }
    let content_type = detect_emoji_type(&response.body)?;
    Ok((response.body, content_type))
}

/// Where the new emoji's bytes come from, as [`file_source`] reads the
/// request: an uploaded part, or the URL the `file` parameter names.
#[derive(Debug)]
enum EmojiFile {
    /// The uploaded `file` part's bytes, already within the size cap.
    Upload(Bytes),
    /// The URL to fetch with the guarded client.
    Url(String),
}

/// Chooses the new emoji's file: the uploaded `file` part wins over the
/// text spelling of the same parameter — it is what a browser's
/// `FormData` sends — and its bytes must fit [`MAX_EMOJI_FILE_BYTES`].
/// A non-blank `file` string is read as a URL to fetch; nothing usable
/// at all is [`MISSING_FILE`]'s 422, and a parameter that is neither a
/// string nor absent names no file this endpoint can store.
///
/// # Errors
///
/// [`AppError::PayloadTooLarge`] for an oversize part,
/// [`AppError::Unprocessable`] for the missing and misplaced parameters.
fn file_source(files: &mut Vec<UploadedFile>, param: Option<&Value>) -> AppResult<EmojiFile> {
    if let Some(upload) = files.take(FILE_FIELD) {
        if upload.bytes.len() > MAX_EMOJI_FILE_BYTES {
            return Err(file_too_large());
        }
        return Ok(EmojiFile::Upload(upload.bytes));
    }
    match param {
        Some(Value::String(url)) if !url.trim().is_empty() => {
            Ok(EmojiFile::Url(url.trim().to_owned()))
        }
        Some(Value::String(_) | Value::Null) | None => {
            Err(AppError::Unprocessable(MISSING_FILE.to_owned()))
        }
        Some(_) => Err(AppError::Unprocessable(UNSUPPORTED_FILE_TYPE.to_owned())),
    }
}

/// The bytes to store and their detected image type: the uploaded
/// `file` part or the fetched URL, per [`file_source`]. Both paths cap
/// the payload at [`MAX_EMOJI_FILE_BYTES`] and demand image magic bytes.
///
/// # Errors
///
/// [`file_source`]'s refusals, [`fetch_emoji_file`]'s, and
/// [`AppError::Unprocessable`] with [`UNSUPPORTED_FILE_TYPE`] for bytes
/// no accepted image type matches.
async fn resolve_emoji_file(
    state: &AppState,
    files: &mut Vec<UploadedFile>,
    param: Option<&Value>,
) -> AppResult<(Bytes, &'static str)> {
    match file_source(files, param)? {
        EmojiFile::Upload(bytes) => {
            let content_type = detect_emoji_type(&bytes)?;
            Ok((bytes, content_type))
        }
        EmojiFile::Url(url) => fetch_emoji_file(state, &url).await,
    }
}

/// `POST /api/v1/pleroma/emoji/packs/files?name=…` — Pleroma's
/// `create` of the file controller: add one emoji file to the pack. The
/// `file` is an uploaded image part or a URL the guarded client fetches
/// (at most 2 MiB, png/gif/jpeg/webp by magic bytes); the `shortcode`
/// must fit `[A-Za-z0-9_-]+` and be new in the pack, else 409; the
/// `filename` defaults to `<shortcode>.<ext>`. The bytes land in the pack
/// directory, `files[shortcode] = filename` lands in `pack.json`, the
/// custom emojis resync, and the pack's full `files` object answers. An
/// unknown or unusable pack is the 400
/// `{"error":"Pack <name> does not exist"}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for an absent or
/// blank `name`, a refused shortcode pattern or file name, and an unknown
/// pack; [`AppError::Unprocessable`] for a body without `shortcode`, one
/// without any `file`, and non-image bytes;
/// [`AppError::PayloadTooLarge`] for a file over 2 MiB;
/// [`AppError::Conflict`] for a duplicate shortcode;
/// [`AppError::Internal`] for a filesystem failure; [`AppError::Database`]
/// from the resync.
pub(super) async fn create_file(
    State(state): State<AppState>,
    admin: AdminUser,
    params: PackParams,
    form: MultipartForm<CreateFileParams>,
) -> AppResult<Json<BTreeMap<String, String>>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let MultipartForm { fields, mut files } = form;
    let name = trimmed_name(fields.name.as_deref().or(params.name.as_deref()))?;
    let shortcode = fields.shortcode;
    let mut pack = load_for_file_write(&state, name).await?;
    if !shortcode_valid(&shortcode) {
        return Err(bad_shortcode(&shortcode));
    }
    if pack.files.contains_key(&shortcode) {
        return Err(shortcode_taken(&shortcode));
    }
    let (bytes, content_type) =
        resolve_emoji_file(&state, &mut files, fields.file.as_ref()).await?;
    let filename = target_filename(fields.filename, &shortcode, content_type)?;
    emoji_packs::write_pack_file(&state.config.emoji_dir, name, &filename, &bytes).await?;
    let _previous: Option<String> = pack.files.insert(shortcode, filename);
    emoji_packs::save_pack(&state.config.emoji_dir, name, &pack).await?;
    resync(&state).await?;
    debug!(
        pack = name,
        files = pack.files.len(),
        "emoji pack file added"
    );
    Ok(Json(pack.files))
}

/// Applies one [`Rename`] to `pack`: the entry's stored file name is the
/// starting point, the rename moves the file on disk (Pleroma's
/// `rename_file`, a self-rename for an unchanged name — which is also how
/// an entry whose file vanished off disk is refused), and the manifest
/// loses the old shortcode and gains the new one before `pack.json` is
/// saved.
///
/// # Errors
///
/// [`emoji_missing`]'s 400 for an entry that is not there to rename,
/// [`AppError::BadRequest`] for a new shortcode the pattern refuses and a
/// new file name [`emoji_packs::file_name_valid`] refuses,
/// [`shortcode_in_use`]'s 409 for an occupied new shortcode without
/// `force`; filesystem failures arrive as [`AppError::Internal`].
async fn apply_rename(
    state: &AppState,
    name: &str,
    pack: &mut emoji_packs::Pack,
    rename: &Rename<'_>,
) -> AppResult<()> {
    let old_filename = pack
        .files
        .get(rename.shortcode)
        .cloned()
        .ok_or_else(|| emoji_missing(rename.shortcode))?;
    let new_shortcode = rename.new_shortcode.unwrap_or(rename.shortcode);
    let same_shortcode = new_shortcode == rename.shortcode;
    if !same_shortcode {
        if !shortcode_valid(new_shortcode) {
            return Err(bad_shortcode(new_shortcode));
        }
        if !rename.force && pack.files.contains_key(new_shortcode) {
            return Err(shortcode_in_use(new_shortcode));
        }
    }
    let new_filename = rename.new_filename.unwrap_or(old_filename.as_str());
    if !emoji_packs::file_name_valid(new_filename) {
        return Err(bad_file_name(new_filename));
    }
    emoji_packs::rename_pack_file(&state.config.emoji_dir, name, &old_filename, new_filename)
        .await
        .map_err(|err| rename_miss(rename.shortcode, err))?;
    let _gone: Option<String> = pack.files.remove(rename.shortcode);
    let _taken: Option<String> = pack
        .files
        .insert(new_shortcode.to_owned(), new_filename.to_owned());
    emoji_packs::save_pack(&state.config.emoji_dir, name, pack).await
}

/// `PATCH /api/v1/pleroma/emoji/packs/files?name=…` — Pleroma's `update`
/// of the file controller: rename an entry's shortcode and/or its file.
/// A new shortcode already used by another entry is
/// [`shortcode_in_use`]'s 409 unless `force` overwrites that entry; an
/// unknown shortcode (or one whose stored file vanished off disk) is the
/// 400 `{"error":"Emoji \"<shortcode>\" does not exist"}`, as is an
/// empty `new_shortcode` or `new_filename` the 422. On success the
/// emojis resync and the pack's full `files` object answers.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for an absent or
/// blank `name` and an unknown pack; [`AppError::Unprocessable`] for a
/// body without `shortcode` or with empty rename values;
/// [`AppError::Conflict`] for an occupied new shortcode;
/// [`AppError::Internal`] for a filesystem failure; [`AppError::Database`]
/// from the resync.
pub(super) async fn update_file(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiParams(params): ApiParams<UpdateFileParams>,
) -> AppResult<Json<BTreeMap<String, String>>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let name = trimmed_name(params.name.as_deref())?;
    if params.new_shortcode.as_deref().is_some_and(str::is_empty)
        || params.new_filename.as_deref().is_some_and(str::is_empty)
    {
        return Err(AppError::Unprocessable(RENAME_EMPTY.to_owned()));
    }
    let mut pack = load_for_file_write(&state, name).await?;
    let rename = Rename {
        shortcode: &params.shortcode,
        new_shortcode: params.new_shortcode.as_deref(),
        new_filename: params.new_filename.as_deref(),
        force: params.force.unwrap_or_default(),
    };
    apply_rename(&state, name, &mut pack, &rename).await?;
    resync(&state).await?;
    debug!(
        pack = name,
        from = rename.shortcode,
        "emoji pack file renamed"
    );
    Ok(Json(pack.files))
}

/// `DELETE /api/v1/pleroma/emoji/packs/files?name=…` — Pleroma's
/// `delete` of the file controller: remove one entry and the file it
/// names. An unknown shortcode is the 400
/// `{"error":"Emoji \"<shortcode>\" does not exist"}`; an entry whose
/// stored file name is unusable or off disk loses the entry anyway (the
/// manifest is what must stop naming it, and an escaping stored name is
/// never touched). On success the emojis resync and the pack's full
/// `files` object answers.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for an absent or
/// blank `name`, an unknown pack, and an unknown shortcode;
/// [`AppError::Internal`] for a filesystem failure; [`AppError::Database`]
/// from the resync.
pub(super) async fn delete_file(
    State(state): State<AppState>,
    admin: AdminUser,
    ApiParams(params): ApiParams<DeleteFileParams>,
) -> AppResult<Json<BTreeMap<String, String>>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let name = trimmed_name(params.name.as_deref())?;
    let mut pack = load_for_file_write(&state, name).await?;
    let Some(filename) = pack.files.remove(&params.shortcode) else {
        return Err(emoji_missing(&params.shortcode));
    };
    if emoji_packs::file_name_valid(&filename) {
        emoji_packs::remove_pack_file(&state.config.emoji_dir, name, &filename).await?;
    }
    emoji_packs::save_pack(&state.config.emoji_dir, name, &pack).await?;
    resync(&state).await?;
    debug!(
        pack = name,
        shortcode = params.shortcode,
        "emoji pack file deleted"
    );
    Ok(Json(pack.files))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_file_source_prefers_the_part_and_names_a_missing_one() {
        let upload = UploadedFile {
            field: FILE_FIELD.to_owned(),
            filename: Some("whatever.png".to_owned()),
            content_type: Some("image/png".to_owned()),
            bytes: Bytes::from_static(b"\x89PNG\r\n\x1a\nrest"),
        };
        let mut files = vec![upload];
        let source = file_source(&mut files, Some(&json!("https://x.test/other")))
            .unwrap_or_else(|err| panic!("the part is the file: {err}"));
        assert!(
            matches!(source, EmojiFile::Upload(bytes) if bytes.len() == 12),
            "the part's bytes are the payload, its name is ignored"
        );
        assert!(files.is_empty(), "the part was taken, not peeked");

        let mut empty: Vec<UploadedFile> = Vec::new();
        let requested = file_source(&mut empty, Some(&json!(" https://x.test/cat.png ")))
            .unwrap_or_else(|err| panic!("a string is a URL to fetch: {err}"));
        assert_eq!(
            source_string(&requested).as_deref(),
            Some("https://x.test/cat.png"),
            "the URL is trimmed and read by the fetch"
        );
        for param in [None, Some(&Value::String("  ".to_owned()))] {
            let err = file_source(&mut empty, param).unwrap_err();
            assert_eq!(
                err.to_string(),
                MISSING_FILE,
                "no usable file parameter is the missing-file 422"
            );
        }
        let err = file_source(&mut empty, Some(&json!({"path": "x"}))).unwrap_err();
        assert_eq!(
            err.to_string(),
            UNSUPPORTED_FILE_TYPE,
            "an object is no file value"
        );
    }

    /// The URL of an [`EmojiFile::Url`] source, `None` for an upload.
    fn source_string(source: &EmojiFile) -> Option<String> {
        match source {
            EmojiFile::Url(url) => Some(url.clone()),
            EmojiFile::Upload(_) => None,
        }
    }

    #[test]
    fn an_oversize_upload_is_the_413_before_anything_else() {
        let upload = UploadedFile {
            field: FILE_FIELD.to_owned(),
            filename: Some("big.png".to_owned()),
            content_type: Some("image/png".to_owned()),
            bytes: Bytes::from(vec![0_u8; MAX_EMOJI_FILE_BYTES + 1]),
        };
        let mut files = vec![upload];
        let err = file_source(&mut files, None).unwrap_err();
        assert!(
            matches!(err, AppError::PayloadTooLarge(_)),
            "2 MiB and one byte is too large: {err}"
        );
    }
}
