//! The request shapes and pure checks of the `/packs/files` routes: the
//! body parameters, the shortcode and file-name rules, the pinned error
//! wordings and the image type detection.

use serde::Deserialize;
use serde_json::Value;

use super::{ACCEPTED_EMOJI_TYPES, MAX_EMOJI_FILE_BYTES, pack_writes::fold_miss};
use crate::{
    error::{AppError, AppResult},
    extract::flex,
    services::emoji_packs,
    state::AppState,
    uploads,
};

/// The body parameters of `POST /packs/files`. `file` is the URL spelling
/// of the image (form or JSON value); an uploaded multipart file part
/// arrives in [`MultipartForm::files`] under the same name instead and
/// wins, as a browser's `FormData` sends it.
///
/// [`MultipartForm::files`]: crate::extract::multipart::MultipartForm::files
#[derive(Debug, Deserialize)]
pub(super) struct CreateFileParams {
    /// Raw pack `name` value, if present.
    #[serde(default)]
    pub(super) name: Option<String>,
    /// The required shortcode of the new emoji.
    pub(super) shortcode: String,
    /// The optional stored file name; defaulted from the shortcode.
    #[serde(default)]
    pub(super) filename: Option<String>,
    /// The `file` parameter: a URL to fetch when no file part arrived.
    #[serde(default)]
    pub(super) file: Option<Value>,
}

/// The body parameters of `PATCH /packs/files`: the entry to rename with
/// its optional new shortcode and new file name, and whether an occupied
/// new shortcode may be overwritten. The names and the flexible `force`
/// follow Pleroma's `update_request` schema.
#[derive(Debug, Deserialize)]
pub(super) struct UpdateFileParams {
    /// Raw pack `name` value, if present.
    #[serde(default)]
    pub(super) name: Option<String>,
    /// The required shortcode of the existing entry.
    pub(super) shortcode: String,
    /// The shortcode the entry gets; absent keeps it.
    #[serde(default)]
    pub(super) new_shortcode: Option<String>,
    /// The file name the entry gets; absent keeps it.
    #[serde(default)]
    pub(super) new_filename: Option<String>,
    /// Whether a new shortcode already used by another entry may
    /// overwrite that entry.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    pub(super) force: Option<bool>,
}

/// The body and query parameters of `DELETE /packs/files`.
#[derive(Debug, Deserialize)]
pub(super) struct DeleteFileParams {
    /// Raw pack `name` value, if present.
    #[serde(default)]
    pub(super) name: Option<String>,
    /// The required shortcode of the entry to remove.
    pub(super) shortcode: String,
}

/// One [`update_file`](super::file_writes::update_file) rename, assembled
/// from the request: the old entry's shortcode with what it should become.
#[derive(Debug)]
pub(super) struct Rename<'a> {
    /// The existing shortcode to rename.
    pub(super) shortcode: &'a str,
    /// The shortcode the entry gets; `None` keeps the old one.
    pub(super) new_shortcode: Option<&'a str>,
    /// The file name the entry gets; `None` keeps the stored one.
    pub(super) new_filename: Option<&'a str>,
    /// Whether an occupied new shortcode may be overwritten.
    pub(super) force: bool,
}

/// The 400 the file routes owe a pack that is not there to write, the
/// ticket pinning Pleroma's `"Pack <name> does not exist"` wording to the
/// 400 status (the pack-write routes answer the same message as a 404,
/// [`pack_missing`](super::pack_writes::pack_missing)). Like
/// [`write_miss`](super::pack_writes::write_miss) it folds an unusable name
/// in, so a file write cannot probe the filesystem either.
fn file_pack_missing(name: &str) -> AppError {
    AppError::BadRequest(format!("Pack {name} does not exist"))
}

/// Folds a service refusal that means "there is no pack here to write"
/// into [`file_pack_missing`]; every other failure arrives unchanged.
fn file_write_miss(name: &str, err: AppError) -> AppError {
    fold_miss(err, || file_pack_missing(name))
}

/// Loads pack `name` for a file route, refusing with
/// [`file_pack_missing`]'s 400 what is not there to write.
pub(super) async fn load_for_file_write(
    state: &AppState,
    name: &str,
) -> AppResult<emoji_packs::Pack> {
    emoji_packs::load_pack(&state.config.emoji_dir, name)
        .await
        .map_err(|err| file_write_miss(name, err))
}

/// The 409 body when the pack already names `code` (the ticket's pinned
/// message, Pleroma's `:already_exists` arm of the create).
pub(super) fn shortcode_taken(code: &str) -> AppError {
    AppError::Conflict(format!(
        "An emoji with the \"{code}\" shortcode already exists"
    ))
}

/// The 409 body when a rename's new shortcode belongs to another entry
/// and `force` was not given (Pleroma's `:already_exists` arm of the
/// update).
pub(super) fn shortcode_in_use(code: &str) -> AppError {
    AppError::Conflict(format!(
        "New shortcode \"{code}\" is already used. If you want to override emoji use 'force' option"
    ))
}

/// The 400 body when a rename or delete names a shortcode the pack does
/// not hold (Pleroma's `:doesnt_exist` arm).
pub(super) fn emoji_missing(code: &str) -> AppError {
    AppError::BadRequest(format!("Emoji \"{code}\" does not exist"))
}

/// The 400 for a shortcode the `[A-Za-z0-9_-]` pattern refuses.
pub(super) fn bad_shortcode(code: &str) -> AppError {
    AppError::BadRequest(format!("invalid emoji shortcode: {code}"))
}

/// The 400 for a file name [`emoji_packs::file_name_valid`] refuses — the
/// service's own wording for the same refusal.
pub(super) fn bad_file_name(file: &str) -> AppError {
    AppError::BadRequest(format!("invalid emoji file name: {file}"))
}

/// The 413 for a file over [`MAX_EMOJI_FILE_BYTES`], uploaded or fetched.
pub(super) fn file_too_large() -> AppError {
    AppError::PayloadTooLarge(format!(
        "Emoji file exceeds the limit of {MAX_EMOJI_FILE_BYTES} bytes"
    ))
}

/// Maps a rename that found nothing to rename — no entry, an entry whose
/// stored file name is unusable, or a stored file that is off disk (the
/// self-rename's `NotFound`) — into [`emoji_missing`]'s 400, Pleroma's
/// answer through `get_filename`; every other failure arrives unchanged.
pub(super) fn rename_miss(shortcode: &str, err: AppError) -> AppError {
    fold_miss(err, || emoji_missing(shortcode))
}

/// The media type of emoji file bytes, read from the bytes themselves —
/// never from the `content-type` the client or the remote server claimed:
/// the match of [`uploads::sniff_allowed`] when its type is one of
/// [`ACCEPTED_EMOJI_TYPES`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `Unsupported file type` for bytes no
/// matcher names and for media an emoji file may not be.
pub(super) fn detect_emoji_type(bytes: &[u8]) -> AppResult<&'static str> {
    uploads::sniff_allowed(bytes, &ACCEPTED_EMOJI_TYPES)
}

/// The extension the detected type gets in the default
/// `<shortcode>.<ext>` file name — the canonical spelling
/// [`crate::uploads`] pairs with each accepted type.
fn emoji_extension(content_type: &str) -> &'static str {
    match content_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        // `image/png`, the only accepted type left.
        _ => "png",
    }
}

/// The file name the new entry gets: `requested` when the body carried a
/// non-empty one, else `<shortcode>.<ext>` from the detected image type;
/// either way it must pass [`emoji_packs::file_name_valid`], so no client
/// name can traverse out of the pack directory.
pub(super) fn target_filename(
    requested: Option<String>,
    shortcode: &str,
    content_type: &str,
) -> AppResult<String> {
    let filename = match requested {
        Some(name) if !name.is_empty() => name,
        _ => format!("{shortcode}.{}", emoji_extension(content_type)),
    };
    if emoji_packs::file_name_valid(&filename) {
        Ok(filename)
    } else {
        Err(bad_file_name(&filename))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        api::emoji_packs::{UNSUPPORTED_FILE_TYPE, file_writes::fetch_refused},
        http_client::HttpError,
    };

    #[test]
    fn shortcodes_follow_the_pinned_pattern() {
        for code in ["cat", "a", "0", "dog_1", "well-known", "MiXeD42"] {
            assert!(emoji_packs::shortcode_valid(code), "{code} is a shortcode");
        }
        for code in [
            "", " ", "cat dog", "cat.png", "a/b", ".hidden", "..", "cat!", "cat;rm", "é", "cat\n",
        ] {
            assert!(
                !emoji_packs::shortcode_valid(code),
                "{code} is not a shortcode"
            );
        }
    }

    #[test]
    fn only_the_four_magic_named_images_are_emoji_files() {
        for (bytes, want) in [
            (b"\x89PNG\r\n\x1a\nrest".as_slice(), "image/png"),
            (b"GIF89a....".as_slice(), "image/gif"),
            (b"\xFF\xD8\xFF\xE0rest".as_slice(), "image/jpeg"),
            (b"RIFF\x00\x00\x00\x00WEBPVP8 ".as_slice(), "image/webp"),
        ] {
            assert_eq!(detect_emoji_type(bytes).unwrap(), want, "type of {want}");
        }
        for bytes in [
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".as_slice(),
            b"PK\x03\x04zipzip".as_slice(),
            b"just text".as_slice(),
            b"".as_slice(),
        ] {
            let err = detect_emoji_type(bytes).unwrap_err();
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "no accepted image hides in these bytes: {err}"
            );
            assert_eq!(err.to_string(), UNSUPPORTED_FILE_TYPE, "pinned wording");
        }
    }

    #[test]
    fn the_default_filename_follows_the_detected_type() {
        for (content_type, extension) in [
            ("image/png", "png"),
            ("image/gif", "gif"),
            ("image/jpeg", "jpg"),
            ("image/webp", "webp"),
        ] {
            assert_eq!(
                target_filename(None, "cat", content_type).unwrap(),
                format!("cat.{extension}"),
                "<shortcode>.<ext> for {content_type}"
            );
            assert_eq!(
                target_filename(Some(String::new()), "cat", content_type).unwrap(),
                format!("cat.{extension}"),
                "an empty request falls back to the default"
            );
        }
        assert_eq!(
            target_filename(Some("sub/cat-picture".to_owned()), "cat", "image/png").unwrap(),
            "sub/cat-picture".to_owned(),
            "the request's own plain file name stands"
        );
        for bad in [
            "../escape.png",
            "/abs.png",
            ".hidden",
            "a/../../b",
            "two words.png",
        ] {
            let err = target_filename(Some(bad.to_owned()), "cat", "image/png").unwrap_err();
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "{bad} is no file name: {err}"
            );
            assert_eq!(err.to_string(), format!("invalid emoji file name: {bad}"));
        }
    }

    #[test]
    fn the_file_error_wordings_are_the_pinned_ones() {
        assert_eq!(
            shortcode_taken("cat").to_string(),
            "An emoji with the \"cat\" shortcode already exists",
            "the create's 409, quoted shortcode"
        );
        assert_eq!(
            shortcode_in_use("dog").to_string(),
            "New shortcode \"dog\" is already used. If you want to override emoji use 'force' option",
            "the rename's 409, Pleroma's wording"
        );
        assert_eq!(
            emoji_missing("ghost").to_string(),
            "Emoji \"ghost\" does not exist",
            "the rename and delete 400"
        );
        assert_eq!(
            file_pack_missing("nope").to_string(),
            "Pack nope does not exist",
            "the file routes' pack 400"
        );
        assert_eq!(
            file_too_large().to_string(),
            format!("Emoji file exceeds the limit of {MAX_EMOJI_FILE_BYTES} bytes"),
            "the size refusal names the cap"
        );
        assert_eq!(
            fetch_refused(&HttpError::InvalidUrl).to_string(),
            format!("Could not fetch the file URL: {}", HttpError::InvalidUrl),
            "the fetch refusal keeps the cause"
        );
    }

    #[test]
    fn an_unusable_pack_folds_into_the_file_routes_400() {
        let absent = file_write_miss("nope", AppError::NotFound);
        assert!(
            matches!(absent, AppError::BadRequest(_)),
            "an absent pack is the named 400, not {absent}"
        );
        assert_eq!(
            absent.to_string(),
            "Pack nope does not exist",
            "Pleroma's wording, name included"
        );
        let escaping =
            file_write_miss("../x", AppError::BadRequest("invalid pack name".to_owned()));
        assert_eq!(
            escaping.to_string(),
            "Pack ../x does not exist",
            "an unusable name folds into the same 400"
        );
        let broken = file_write_miss("p", AppError::Internal("disk full".to_owned()));
        assert!(
            matches!(broken, AppError::Internal(_)),
            "a real server failure is not a 400"
        );
        let gone = rename_miss("cat", AppError::NotFound);
        assert_eq!(
            gone.to_string(),
            "Emoji \"cat\" does not exist",
            "a file off disk is the emoji's own 400"
        );
    }

    #[test]
    fn the_file_bodies_read_their_shapes() {
        let create: CreateFileParams = serde_json::from_value(json!({
            "name": "cats",
            "shortcode": "cat",
            "filename": "cat.png",
            "file": "https://x.test/cat.png",
        }))
        .unwrap();
        assert_eq!(create.name.as_deref(), Some("cats"));
        assert_eq!(create.shortcode, "cat");
        assert_eq!(create.filename.as_deref(), Some("cat.png"));
        assert_eq!(
            create.file,
            Some(Value::String("https://x.test/cat.png".to_owned())),
            "the URL rides in as the string it is"
        );

        let minimal: CreateFileParams =
            serde_json::from_value(json!({"shortcode": "cat", "file": "https://x.test/c"}))
                .unwrap();
        assert_eq!(minimal.name, None, "the query has the pack name");
        assert_eq!(minimal.filename, None, "and the default names the file");

        let no_file: Result<CreateFileParams, _> =
            serde_json::from_value(json!({"shortcode": "cat"}));
        assert!(no_file.is_ok(), "file is only required on the wire here");

        let rename: UpdateFileParams = serde_json::from_value(json!({
            "name": "cats",
            "shortcode": "cat",
            "new_shortcode": "kitty",
            "force": "on",
        }))
        .unwrap();
        assert_eq!(rename.new_shortcode.as_deref(), Some("kitty"));
        assert_eq!(
            rename.new_filename, None,
            "a rename may touch only the shortcode"
        );
        assert_eq!(
            rename.force,
            Some(true),
            "the form spelling reads through flex"
        );

        let drop: DeleteFileParams = serde_json::from_value(json!({"shortcode": "cat"})).unwrap();
        assert_eq!(drop.shortcode, "cat");
    }

    #[test]
    fn an_empty_rename_value_is_pinned_to_the_422() {
        for params in [
            json!({"shortcode": "a", "new_shortcode": ""}),
            json!({"shortcode": "a", "new_filename": ""}),
        ] {
            let update: UpdateFileParams = serde_json::from_value(params).unwrap();
            let empty = update.new_shortcode.as_deref().is_some_and(str::is_empty)
                || update.new_filename.as_deref().is_some_and(str::is_empty);
            assert!(empty, "an explicit empty is not an absent rename");
        }
        let absent: UpdateFileParams = serde_json::from_value(json!({"shortcode": "a"})).unwrap();
        assert!(
            absent.new_shortcode.is_none() && absent.new_filename.is_none(),
            "absent keys keep the entry as it stands"
        );
    }
}
