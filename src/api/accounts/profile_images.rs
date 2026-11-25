//! The three profile-image parameters of `update_credentials`: what one
//! parameter asks for ([`ImageWrite`]), the three resolved together so
//! a body with one good image and one bad one stores nothing
//! ([`resolve_images`]), the columns the writes leave behind
//! ([`ImageWrites::apply`] and [`Applied`]) and the URL a write
//! replaced ([`replaced_url`]). The bytes' type check and the file
//! writes themselves are [`profile_image_store`]'s.
//!
//! [`profile_image_store`]: super::profile_image_store

use bytes::Bytes;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    extract::multipart::{UploadedFile, UploadedFiles as _},
    models::user::User,
    services::media_processing::normalize_upload,
    state::AppState,
};

use super::{
    credentials::ProfileParams,
    profile_image_store::{detect_image_type, store_image},
};

/// The form field `PATCH /api/v1/accounts/update_credentials` reads the
/// avatar image from (Mastodon's and Pleroma's `avatar`).
pub(super) const AVATAR_FIELD: &str = "avatar";

/// The field the profile banner arrives under: Mastodon calls it `header`,
/// Pleroma stores it in the `banner` column.
const HEADER_FIELD: &str = "header";

/// The field Pleroma's profile background image arrives under.
const BACKGROUND_FIELD: &str = "pleroma_background_image";

/// The 422 body for a profile-image part whose bytes are none of
/// [`ACCEPTED_IMAGE_TYPES`] — the same wording the media upload uses for
/// bytes it cannot name an accepted type for.
pub(super) const UNSUPPORTED_FILE_TYPE: &str = "Unsupported file type";

/// The 422 body for a profile-image part over its configured byte limit
/// ([`avatar_limit_bytes`](Config::avatar_limit_bytes) for the avatar,
/// [`banner_limit_bytes`](Config::banner_limit_bytes) for the banner and the
/// background); Pleroma's `update_credentials` wording, which this
/// endpoint's ticket pins to 422 rather than its 413.
const FILE_TOO_LARGE: &str = "File is too large";

/// What one profile-image parameter of an `update_credentials` body asks
/// for, after its bytes passed the checks and before anything was stored.
#[derive(Debug)]
pub(super) enum ImageWrite {
    /// The parameter is absent: the column keeps its stored value.
    Unchanged,
    /// The empty string (the form and multipart spelling) or JSON `null`:
    /// the column is cleared, Pleroma's `user_image_value` reset.
    Clear,
    /// A file part whose bytes named an accepted image type and stayed
    /// within the column's byte limit.
    Store {
        /// The image's bytes.
        bytes: Bytes,
        /// The client's file name, used only for the key's extension
        /// fallback (the type comes from the bytes).
        filename: Option<String>,
        /// The media type the magic bytes named.
        content_type: &'static str,
    },
}

/// The three profile-image writes of one body, resolved together so that a
/// body with one good image and one bad one stores nothing at all.
#[derive(Debug)]
pub(super) struct ImageWrites {
    /// The write for `users.avatar_url`, from the `avatar` parameter.
    avatar: ImageWrite,
    /// The write for `users.banner_url`, from the `header` parameter.
    banner: ImageWrite,
    /// The write for `users.background_image_url`, from Pleroma's
    /// `pleroma_background_image` parameter.
    background: ImageWrite,
}

/// One profile-image column after its write was applied: the value the row
/// will hold, and the URL the write replaced.
type MergedImage = (Option<String>, Option<String>);

impl ImageWrites {
    /// Stores every upload of `writes` through the uploader and answers the
    /// three merged column values beside the list of URLs the writes
    /// replaced: [`ImageWrite::Unchanged`] leaves the `current` row's value
    /// in place, [`ImageWrite::Clear`] replaces it with `None` and
    /// [`ImageWrite::Store`] with the new file's public URL. Whatever the
    /// column held before and no longer holds is the file
    /// [`delete_replaced_files`] removes, once the row stopped naming it.
    ///
    /// Every uploaded image is normalized first
    /// ([`normalize_upload`]: decoded, held to
    /// `MAX_IMAGE_PIXELS` and encoded again without its metadata) and
    /// nothing is stored until all three passed, so a body with one good
    /// image and one undecodable one still writes nothing.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] with `Invalid image` or `Image too large`
    /// from the normalization; storage failures arrive from
    /// [`store_image`] as [`AppError::Internal`] / [`AppError::BadRequest`].
    pub(super) async fn apply(
        self,
        state: &AppState,
        user_id: Uuid,
        current: &User,
    ) -> AppResult<Applied> {
        let avatar = self.avatar.normalized(&state.config).await?;
        let banner = self.banner.normalized(&state.config).await?;
        let background = self.background.normalized(&state.config).await?;
        let (avatar_url, avatar_replaced) = avatar
            .merged(state, user_id, current.avatar_url.as_deref())
            .await?;
        let (banner_url, banner_replaced) = banner
            .merged(state, user_id, current.banner_url.as_deref())
            .await?;
        let (background_image_url, background_replaced) = background
            .merged(state, user_id, current.background_image_url.as_deref())
            .await?;
        Ok(Applied {
            avatar_url,
            banner_url,
            background_image_url,
            replaced: [avatar_replaced, banner_replaced, background_replaced]
                .into_iter()
                .flatten()
                .collect(),
        })
    }
}

/// The three merged profile-image columns and the files that replaced them
/// (see [`ImageWrites::apply`]).
#[derive(Debug)]
pub(super) struct Applied {
    /// The value for `users.avatar_url`.
    pub(super) avatar_url: Option<String>,
    /// The value for `users.banner_url`.
    pub(super) banner_url: Option<String>,
    /// The value for `users.background_image_url`.
    pub(super) background_image_url: Option<String>,
    /// The URLs the writes replaced, in column order: empty for a slot that
    /// was untouched, or cleared while already empty.
    pub(super) replaced: Vec<String>,
}

impl ImageWrite {
    /// This write with an uploaded image replaced by its normalized
    /// re-encoding — the bytes and the media type the encoder produced —
    /// and any other write unchanged.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] from [`normalize_upload`]: `Invalid
    /// image` for bytes that name an image type but do not decode, `Image
    /// too large` for a pixel count over `MAX_IMAGE_PIXELS`.
    async fn normalized(self, config: &Config) -> AppResult<Self> {
        match self {
            Self::Store {
                bytes, filename, ..
            } => {
                let image = normalize_upload(config, bytes).await?;
                Ok(Self::Store {
                    bytes: Bytes::from(image.bytes),
                    filename,
                    content_type: image.content_type,
                })
            }
            other @ (Self::Unchanged | Self::Clear) => Ok(other),
        }
    }

    /// The value this write leaves in its column and the URL it replaced:
    /// see [`ImageWrites::apply`] for the three cases. `current` is what the
    /// row holds now, so an untouched slot round-trips it.
    ///
    /// # Errors
    ///
    /// As [`store_image`]'s for a [`ImageWrite::Store`].
    async fn merged(
        self,
        state: &AppState,
        user_id: Uuid,
        current: Option<&str>,
    ) -> AppResult<MergedImage> {
        let value = match self {
            Self::Unchanged => current.map(str::to_owned),
            Self::Clear => None,
            Self::Store {
                bytes,
                filename,
                content_type,
            } => Some(store_image(state, user_id, bytes, filename.as_deref(), content_type).await?),
        };
        Ok((value.clone(), replaced_url(current, value.as_deref())))
    }
}

/// The URL a write leaves behind: `Some` only when the column held one and
/// the write stores a different value — a replacement, or a clear. Re-sending
/// the URL already stored replaces nothing (and a body cannot send one
/// anyway, since this endpoint only ever writes URLs it minted itself).
fn replaced_url(before: Option<&str>, after: Option<&str>) -> Option<String> {
    before
        .filter(|held| Some(*held) != after)
        .map(str::to_owned)
}

/// Resolves the three profile-image parameters of one body against the file
/// parts that rode along, each against its own configured cap: the avatar
/// against [`avatar_limit_bytes`](Config::avatar_limit_bytes), the banner and
/// the background against [`banner_limit_bytes`](Config::banner_limit_bytes),
/// Pleroma's one number for both. A file part wins over a text or JSON
/// spelling of the same parameter — that is what a browser's `FormData`
/// sends — and every slot is resolved before any file is stored, so a body
/// with one good image and one bad one writes nothing.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`FILE_TOO_LARGE`] or
/// [`UNSUPPORTED_FILE_TYPE`] from [`resolve_image`].
pub(super) fn resolve_images(
    config: &Config,
    files: &mut Vec<UploadedFile>,
    params: &ProfileParams,
) -> AppResult<ImageWrites> {
    Ok(ImageWrites {
        avatar: resolve_image(
            files,
            AVATAR_FIELD,
            params.avatar.as_ref(),
            config.avatar_limit_bytes,
        )?,
        banner: resolve_image(
            files,
            HEADER_FIELD,
            params.header.as_ref(),
            config.banner_limit_bytes,
        )?,
        background: resolve_image(
            files,
            BACKGROUND_FIELD,
            params.pleroma_background_image.as_ref(),
            config.banner_limit_bytes,
        )?,
    })
}

/// Resolves one profile-image slot: the file part uploaded under `field`
/// when the body carried one, else the parameter's text or JSON `value` —
/// absent keeps the column, `null` and the empty string clear it, and
/// anything else is a value this endpoint has no way to store, answered with
/// the same 422 as bytes it cannot name an image for.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`FILE_TOO_LARGE`] or
/// [`UNSUPPORTED_FILE_TYPE`].
fn resolve_image(
    files: &mut Vec<UploadedFile>,
    field: &str,
    value: Option<&Value>,
    max_bytes: usize,
) -> AppResult<ImageWrite> {
    if let Some(file) = files.take(field) {
        return validated_write(file, max_bytes);
    }
    match value {
        None => Ok(ImageWrite::Unchanged),
        Some(Value::Null) => Ok(ImageWrite::Clear),
        Some(Value::String(text)) if text.is_empty() => Ok(ImageWrite::Clear),
        Some(_) => Err(AppError::Unprocessable(UNSUPPORTED_FILE_TYPE.to_owned())),
    }
}

/// The write for one image file part: its size checked against `max_bytes`
/// and its bytes against [`ACCEPTED_IMAGE_TYPES`], before anything reached
/// storage.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`FILE_TOO_LARGE`] over the cap or
/// [`UNSUPPORTED_FILE_TYPE`] from [`detect_image_type`].
fn validated_write(file: UploadedFile, max_bytes: usize) -> AppResult<ImageWrite> {
    if file.bytes.len() > max_bytes {
        return Err(AppError::Unprocessable(FILE_TOO_LARGE.to_owned()));
    }
    let content_type = detect_image_type(&file.bytes)?;
    Ok(ImageWrite::Store {
        bytes: file.bytes,
        filename: file.filename,
        content_type,
    })
}

#[cfg(test)]
mod tests {
    use crate::api::accounts::test_support::{PNG, no_params, params, part};

    use super::*;

    /// Resolves `value` as the `avatar` slot with no file part, `unwrap`ing
    /// the success or naming the rejection the test asserts on.
    fn avatar_slot(value: Option<&str>) -> Result<ImageWrite, AppError> {
        let parsed: Option<Value> =
            value.map(|text| serde_json::from_str(text).expect("the fixture value parses"));
        let mut files = Vec::new();
        resolve_image(
            &mut files,
            AVATAR_FIELD,
            parsed.as_ref(),
            Config::for_tests().avatar_limit_bytes,
        )
    }

    #[test]
    fn an_absent_image_parameter_keeps_the_stored_column() {
        assert!(
            matches!(avatar_slot(None).unwrap(), ImageWrite::Unchanged),
            "no key in the body is no write"
        );
    }

    #[test]
    fn null_and_empty_image_parameters_clear_the_column() {
        // The two spellings of Pleroma's reset: JSON `null` and the empty
        // string a form or multipart text part carries.
        for value in [Some("null"), Some(r#""""#)] {
            assert!(
                matches!(avatar_slot(value).unwrap(), ImageWrite::Clear),
                "`{value:?}` clears"
            );
        }
        let body = params(r#"{"avatar": "", "header": null}"#);
        let mut files = Vec::new();
        let writes = resolve_images(&Config::for_tests(), &mut files, &body).unwrap();
        assert!(
            matches!(writes.avatar, ImageWrite::Clear)
                && matches!(writes.banner, ImageWrite::Clear)
                && matches!(writes.background, ImageWrite::Unchanged),
            "the two sent slots clear, the third stays"
        );
    }

    #[test]
    fn a_url_string_is_not_an_image_the_endpoint_can_store() {
        for value in [Some(r#""https://elsewhere.example/a.png""#), Some("[1,2]")] {
            let err = avatar_slot(value).expect_err("a string is no upload");
            assert_eq!(
                err.to_string(),
                UNSUPPORTED_FILE_TYPE,
                "`{value:?}` is refused with the type message"
            );
        }
    }

    #[test]
    fn a_file_part_wins_over_the_text_spelling_of_its_field() {
        let mut files = vec![part(AVATAR_FIELD, PNG)];
        let writes = resolve_images(
            &Config::for_tests(),
            &mut files,
            &params(r#"{"avatar": ""}"#),
        )
        .unwrap();
        assert!(
            matches!(writes.avatar, ImageWrite::Store { .. }),
            "the uploaded file is what the slot reads"
        );
        assert!(
            files.is_empty(),
            "and the part is taken out of the body's files"
        );
    }

    #[test]
    fn each_slot_is_measured_against_its_own_limit() {
        // [`PNG`] is 20 bytes: over the 8-byte avatar cap the fixture sets,
        // under the 32-byte banner one — which is also what measures the
        // background image.
        let config = Config {
            avatar_limit_bytes: 8,
            banner_limit_bytes: 32,
            ..Config::for_tests()
        };
        let err = resolve_one(&config, AVATAR_FIELD).expect_err("the avatar cap is 8 bytes");
        assert_eq!(
            err.to_string(),
            FILE_TOO_LARGE,
            "the avatar is over its own cap"
        );
        for field in [HEADER_FIELD, BACKGROUND_FIELD] {
            assert!(
                matches!(
                    resolve_one(&config, field).unwrap(),
                    ImageWrite::Store { .. }
                ),
                "{field} fits the banner cap"
            );
        }
    }

    /// Resolves `field` as the whole of a body: one [`PNG`] part under that
    /// name, no text parameters, measured by `config`'s limits.
    fn resolve_one(config: &Config, field: &str) -> AppResult<ImageWrite> {
        let mut files = vec![part(field, PNG)];
        resolve_images(config, &mut files, &no_params()).map(|writes| write_for(writes, field))
    }

    /// The slot of `writes` that arrived under `field`, for the tests that
    /// resolve one field at a time.
    fn write_for(writes: ImageWrites, field: &str) -> ImageWrite {
        if field == AVATAR_FIELD {
            writes.avatar
        } else if field == HEADER_FIELD {
            writes.banner
        } else {
            writes.background
        }
    }

    #[test]
    fn an_unrelated_file_part_is_ignored_and_no_file_is_stored() {
        let mut files = vec![part("file", PNG)];
        let writes = resolve_images(&Config::for_tests(), &mut files, &no_params()).unwrap();
        assert!(
            matches!(
                (writes.avatar, writes.banner, writes.background),
                (
                    ImageWrite::Unchanged,
                    ImageWrite::Unchanged,
                    ImageWrite::Unchanged
                )
            ),
            "a file under another field name writes no column"
        );
        assert_eq!(files.len(), 1, "and the part is left where it was");
    }

    #[test]
    fn replaced_urls_are_only_named_by_writes_that_overwrote_one() {
        let old = "https://test.example/media/alice/a.png";
        let new = "https://test.example/media/alice/b.png";
        assert_eq!(
            replaced_url(None, None),
            None,
            "nothing stored, nothing gone"
        );
        assert_eq!(
            replaced_url(Some(old), Some(old)),
            None,
            "the same URL still uses its file"
        );
        let replaced = replaced_url(Some(old), Some(new));
        assert_eq!(
            replaced.as_deref(),
            Some(old),
            "a replacement names the old"
        );
        let cleared = replaced_url(Some(old), None);
        assert_eq!(
            cleared.as_deref(),
            Some(old),
            "a clear names the stored one"
        );
    }
}
