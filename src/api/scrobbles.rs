//! Scrobbles over HTTP (`/api/v1/pleroma`, WBE-121): posting what an account
//! listened to and listing an account's recent listens.
//!
//! The reference is Pleroma's `ScrobbleController`
//! (`lib/pleroma/web/pleroma_api/controllers/scrobble_controller.ex`) and its
//! `ScrobbleView` (`lib/pleroma/web/pleroma_api/views/scrobble_view.ex`).
//! Pleroma stores a listen as a `Listen` activity wrapping an `Audio` object;
//! here it is one row of `scrobbles` (see [`crate::models::scrobble`]), which
//! is what the two routes below read and write — no federation, no
//! notifications, no streaming.
//!
//! - `POST /api/v1/pleroma/scrobble` requires a user token with the
//!   `write:scrobbles` scope (of which a `write` token is the parent) and
//!   answers the created [`Scrobble`] with 200, as Pleroma's
//!   `render("show.json", …)` does. Its parameters (decision 10: through
//!   [`ApiBody`], so the form spellings work) are `title` (required, at most
//!   255 characters), `artist`, `album`, `length` in milliseconds,
//!   `externalLink` — whose `snake_case` spelling `external_link` Pleroma's
//!   controller folds it into, so both are accepted and the `snake_case` one
//!   wins — and
//!   `visibility`, `public` by default and otherwise `unlisted`, `private` or
//!   `direct`. A blank or absent title answers 422 `{"error":"title: can't be
//!   blank"}` and anything else that fails these rules its own 422; Pleroma
//!   answers a rejected listen with a 400 through its `{:error, message}` arm
//!   and its `CastAndValidate` plug with a 400 too, the ticket's 422 being
//!   authoritative here.
//! - `GET /api/v1/pleroma/accounts/{id}/scrobbles` lists that account's
//!   scrobbles newest first, keyset-paginated by [`PageParams`] (limit 20,
//!   [`MAX_LIMIT`](crate::pagination::MAX_LIMIT) at most) with the `Link`
//!   header Pleroma's `add_link_headers` writes. Authentication is optional —
//!   Pleroma's
//!   `OAuthScopesPlug … fallback: :proceed_unauthenticated` — and a token
//!   that grants `read:scrobbles` names the reader (one without the scope
//!   reads as no token): the account itself sees all of its
//!   scrobbles, anyone else only the `public` and `unlisted` ones, which is
//!   what Pleroma's address filter leaves of a `private` or `direct` `Listen`
//!   for a stranger. An id that is malformed, unknown or deactivated is the
//!   one 404 of [`account_or_404`], and a malformed pagination cursor the 400
//!   "Invalid id" that [`PageParams`] answers before the handler runs at all.
//!
//! Pleroma's `index` also caps the listing at the account's *recent* listens
//! (its `now playing` window); the ticket lists every scrobble newest first,
//! so nothing here filters by time — a client decides how far back to page.

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;
use tracing::debug;

use crate::{
    api::helpers::account_or_404,
    auth::{AuthUser, MaybeAuthUser, require_scope},
    entities::scrobble::Scrobble,
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    models::{
        scrobble::{self, NewScrobble},
        status::Visibility,
    },
    oauth::scopes::{READ_SCROBBLES, WRITE_SCROBBLES},
    pagination::{PageParams, link_header, set_link_header},
    state::AppState,
    util::url::is_http_url,
};

/// The longest title the endpoint accepts (the ticket's 255).
const MAX_TITLE_CHARS: usize = 255;
/// The longest artist or album the endpoint accepts.
const MAX_TEXT_CHARS: usize = 255;
/// The longest external link the endpoint accepts.
const MAX_LINK_CHARS: usize = 2048;
/// 422 body for an external link that is too long or not an `http(s)` URL.
const LINK_INVALID: &str = "externalLink: is invalid";
/// 422 body for an absent or blank title, in Pleroma's changeset wording
/// (`"title: can't be blank"`, the `validate_required` message its
/// `ApiSpec`-style errors are reported with).
const TITLE_BLANK: &str = "title: can't be blank";
/// The visibilities the endpoint accepts, `public` first because that is the
/// default a request without one gets.
const VISIBILITIES: [&str; 4] = ["public", "unlisted", "private", "direct"];
/// 422 body for a visibility outside [`VISIBILITIES`], in the same wording.
const VISIBILITY_INVALID: &str = "visibility: is not included in the list";
/// The `Link` header's query parameters carry nothing but the cursor: this
/// listing has no filter to repeat on either link.
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// The `/api/v1/pleroma` scrobble router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/pleroma/scrobble", post(create_scrobble))
        .route("/pleroma/accounts/{id}/scrobbles", get(list_scrobbles))
}

/// `POST /api/v1/pleroma/scrobble` — store one listen for the caller and
/// answer it as the [`Scrobble`] entity, rendered for its owner as reader
/// (Pleroma's `for: user`).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:scrobbles` check; [`AppError::Unprocessable`] for a blank, absent or
/// too-long title, an unknown `visibility` or a `length` that is not an
/// integer. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn create_scrobble(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<ScrobbleParams>,
) -> AppResult<Json<Scrobble>> {
    auth.require(WRITE_SCROBBLES)?;
    let new = params.into_new_scrobble()?;
    let row = scrobble::insert(&state.db, auth.0.id, &new).await?;
    debug!(
        scrobble = %row.id,
        user = %auth.0.id,
        visibility = row.visibility.as_str(),
        "scrobble created"
    );
    Ok(Json(Scrobble::from_row(
        &row,
        &auth.0,
        Some(&auth.0),
        &state.config,
    )))
}

/// `GET /api/v1/pleroma/accounts/{id}/scrobbles` — that account's scrobbles,
/// newest first, one [`PageParams`] page at a time.
///
/// The entity's `account` is the listed account, rendered for whoever asks
/// (Pleroma's `for: reading_user`), and `private`/`direct` rows appear only
/// for their owner (see [`page_for_user`]). An empty page answers the empty
/// array with no `Link` header, [`link_header`]'s rule.
///
/// The `Link` header's path is the request's own [`OriginalUri`], not the
/// [`Uri`] the router hands a nested handler: axum strips the `/api/v1`
/// prefix off the latter, and Pleroma links to `conn.request_path`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`MaybeAuthUser`] for an invalid token;
/// [`AppError::NotFound`] for a malformed, unknown or deactivated account id;
/// [`AppError::BadRequest`] from [`PageParams`] for a malformed cursor. Any
/// database failure arrives through `?` as [`AppError::Database`], and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`AppError::Internal`].
///
/// [`page_for_user`]: crate::models::scrobble::page_for_user
/// [`Uri`]: axum::http::Uri
async fn list_scrobbles(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Scrobble>>)> {
    // Pleroma's `fallback: :proceed_unauthenticated`: a token without
    // `read:scrobbles` reads as no token at all.
    let viewer = match (auth.0, auth.1) {
        (Some(user), Some(token)) if require_scope(&token, READ_SCROBBLES).is_ok() => Some(user),
        _ => None,
    };
    let owner = account_or_404(&state, &id).await?;
    let rows = scrobble::page_for_user(
        &state.db,
        owner.id,
        viewer.as_ref().map(|found| found.id),
        page,
    )
    .await?;
    let link = page_link(&state.config.base_url, uri.path(), &rows);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    debug!(
        count = rows.len(),
        account = %owner.id,
        "scrobbles listed"
    );
    Ok((
        headers,
        Json(
            rows.iter()
                .map(|row| Scrobble::from_row(row, &owner, viewer.as_ref(), &state.config))
                .collect(),
        ),
    ))
}

/// The `Link` header value for a fetched page: Pleroma's `add_link_headers`
/// over the first (newest) and last (oldest) row's id, `None` for an empty
/// page.
fn page_link(base_url: &str, path: &str, rows: &[scrobble::Scrobble]) -> Option<String> {
    let newest = rows.first()?.id.to_string();
    let oldest = rows.last()?.id.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}

/// The request parameters of `POST /api/v1/pleroma/scrobble`.
///
/// Every field is `Option` at the serde layer: the title is required in the
/// sense that its absence is the documented 422 rather than a deserialization
/// error, and the rest carry their column defaults
/// ([`into_new_scrobble`](ScrobbleParams::into_new_scrobble)).
#[derive(Debug, Deserialize)]
struct ScrobbleParams {
    /// The title of the media playing; required, at most
    /// [`MAX_TITLE_CHARS`] characters.
    #[serde(default)]
    title: Option<String>,
    /// The artist; `None` stores `NULL`.
    #[serde(default)]
    artist: Option<String>,
    /// The album; `None` stores `NULL`.
    #[serde(default)]
    album: Option<String>,
    /// Length of the media in milliseconds; `None` stores `NULL`. A [`flex`]
    /// integer, because a form body spells it `"180000"`.
    #[serde(default, deserialize_with = "flex::opt_i32")]
    length: Option<i32>,
    /// A URL referencing the media, under the `snake_case` spelling, which wins
    /// over [`external_link_camel`](Self::external_link_camel).
    #[serde(default)]
    external_link: Option<String>,
    /// The same URL under Pleroma's `externalLink` spelling.
    #[serde(default, rename = "externalLink")]
    external_link_camel: Option<String>,
    /// Who may see the scrobble; one of [`VISIBILITIES`], `public` when
    /// absent.
    #[serde(default)]
    visibility: Option<String>,
}

impl ScrobbleParams {
    /// Validates the parameters as one new scrobble, applying the column
    /// defaults to the optional fields.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] for a blank, absent or too-long title, or
    /// for a `visibility` outside [`VISIBILITIES`].
    fn into_new_scrobble(self) -> AppResult<NewScrobble> {
        Ok(NewScrobble {
            title: validate_title(self.title)?,
            artist: validate_optional_text("artist", self.artist, MAX_TEXT_CHARS)?,
            album: validate_optional_text("album", self.album, MAX_TEXT_CHARS)?,
            length: self.length,
            external_link: validate_link(self.external_link.or(self.external_link_camel))?,
            visibility: validate_visibility(self.visibility)?,
        })
    }
}

/// An optional free-text field, checked: blank or absent is `None`, longer
/// than `max` characters is 422 `"<field>: is too long (maximum is <max>
/// characters)"`; a kept value is stored verbatim.
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn validate_optional_text(
    field: &str,
    value: Option<String>,
    max: usize,
) -> AppResult<Option<String>> {
    let Some(text) = value.filter(|text| !text.trim().is_empty()) else {
        return Ok(None);
    };
    if text.chars().count() > max {
        return Err(AppError::Unprocessable(format!(
            "{field}: is too long (maximum is {max} characters)"
        )));
    }
    Ok(Some(text))
}

/// The external link, checked: blank or absent is `None`; otherwise at most
/// [`MAX_LINK_CHARS`] characters and an absolute `http(s)` URL, else 422
/// [`LINK_INVALID`]. A kept link is stored verbatim.
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn validate_link(link: Option<String>) -> AppResult<Option<String>> {
    let Some(text) = link.filter(|text| !text.trim().is_empty()) else {
        return Ok(None);
    };
    if text.chars().count() > MAX_LINK_CHARS || !is_http_url(&text) {
        return Err(AppError::Unprocessable(LINK_INVALID.to_owned()));
    }
    Ok(Some(text))
}

/// The title, checked: absent or blank is 422 [`TITLE_BLANK`], longer than
/// [`MAX_TITLE_CHARS`] characters is 422 with the same wording; a kept title
/// is stored verbatim, spaces included.
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn validate_title(title: Option<String>) -> AppResult<String> {
    let title = title.unwrap_or_default();
    if title.trim().is_empty() {
        return Err(AppError::Unprocessable(TITLE_BLANK.to_owned()));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(AppError::Unprocessable(format!(
            "title: is too long (maximum is {MAX_TITLE_CHARS} characters)"
        )));
    }
    Ok(title)
}

/// The visibility, checked: absent is the `public` default, one of
/// [`VISIBILITIES`] is that visibility, anything else — including a status'
/// `local` — is 422 [`VISIBILITY_INVALID`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn validate_visibility(visibility: Option<String>) -> AppResult<Visibility> {
    let Some(text) = visibility else {
        return Ok(Visibility::Public);
    };
    VISIBILITIES
        .into_iter()
        .find(|allowed| *allowed == text.as_str())
        .and_then(|allowed| allowed.parse::<Visibility>().ok())
        .ok_or_else(|| AppError::Unprocessable(VISIBILITY_INVALID.to_owned()))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::pagination::MAX_LIMIT;

    /// The oldest of the three ids [`rows`] builds, by text order.
    const OLDEST_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// The middle id of the three [`rows`] builds.
    const MIDDLE_ID: &str = "01934567-89ab-7cde-f012-3456789abcdf";
    /// The newest of the three ids [`rows`] builds, by text order.
    const NEWEST_ID: &str = "01934567-89ab-7cde-f012-3456789abce0";

    /// Parses `value` as the endpoint's parameters and validates them as a
    /// create would.
    fn new_scrobble(value: Value) -> AppResult<NewScrobble> {
        let params: ScrobbleParams = crate::extract::into_params(value)?;
        params.into_new_scrobble()
    }

    /// The 422 message a create of `value` is rejected with.
    fn rejection(value: Value) -> String {
        new_scrobble(value)
            .expect_err("the fixture is invalid")
            .to_string()
    }

    /// Three stored scrobbles of one account in the order [`page_link`]
    /// receives them: newest first, ids ascending by text.
    fn rows() -> Vec<scrobble::Scrobble> {
        [NEWEST_ID, MIDDLE_ID, OLDEST_ID]
            .into_iter()
            .map(|id| scrobble::Scrobble {
                id: Uuid::parse_str(id).expect("the fixture ids are ids"),
                user_id: Uuid::now_v7(),
                title: "x".to_owned(),
                artist: None,
                album: None,
                length: None,
                external_link: None,
                visibility: Visibility::Public,
                inserted_at: chrono::Utc::now(),
            })
            .collect()
    }

    /// The page parameters of `query`, the endpoint's own cap.
    fn page_of(query: &str) -> PageParams {
        PageParams::from_query(Some(query), MAX_LIMIT).expect("the fixture query paginates")
    }

    #[test]
    fn a_title_only_body_applies_every_default() {
        let new =
            new_scrobble(json!({"title": "Midnight City"})).expect("the minimal body validates");
        assert_eq!(new.title, "Midnight City", "the title is kept verbatim");
        assert!(new.artist.is_none(), "no artist means NULL");
        assert!(new.album.is_none(), "no album means NULL");
        assert!(new.length.is_none(), "no length means NULL");
        assert!(new.external_link.is_none(), "no link means NULL");
        assert_eq!(
            new.visibility,
            Visibility::Public,
            "and the visibility defaults to public"
        );
    }

    #[test]
    fn every_known_visibility_is_accepted() {
        for (text, want) in [
            ("public", Visibility::Public),
            ("unlisted", Visibility::Unlisted),
            ("private", Visibility::Private),
            ("direct", Visibility::Direct),
        ] {
            let new = new_scrobble(json!({"title": "x", "visibility": text}))
                .unwrap_or_else(|err| panic!("`{text}` is valid: {err}"));
            assert_eq!(new.visibility, want, "`{text}` goes through");
        }
        assert_eq!(VISIBILITIES.len(), 4, "the four the ticket lists");
    }

    #[test]
    fn an_unknown_visibility_is_rejected() {
        for value in [json!(""), json!("local"), json!("Public"), json!(" public")] {
            assert_eq!(
                rejection(json!({"title": "x", "visibility": value})),
                VISIBILITY_INVALID,
                "`{value}` is not a scrobble visibility"
            );
        }
    }

    #[test]
    fn a_blank_or_absent_title_is_rejected() {
        for value in [json!(null), json!(""), json!("   ")] {
            assert_eq!(
                rejection(json!({"title": value})),
                TITLE_BLANK,
                "`{value}` is a blank title"
            );
        }
        assert_eq!(
            rejection(json!({})),
            TITLE_BLANK,
            "absent is as blank as empty"
        );
        assert_eq!(
            rejection(json!({"artist": "M83"})),
            TITLE_BLANK,
            "and no other parameter stands in for it"
        );
    }

    #[test]
    fn a_title_of_exactly_the_maximum_length_passes() {
        let edge = "x".repeat(MAX_TITLE_CHARS);
        let new = new_scrobble(json!({"title": edge.as_str()})).expect("255 characters fit");
        assert_eq!(new.title, edge, "verbatim");

        assert_eq!(
            rejection(json!({"title": "x".repeat(MAX_TITLE_CHARS + 1)})),
            "title: is too long (maximum is 255 characters)",
            "256 do not"
        );
    }

    #[test]
    fn a_title_is_counted_in_characters_not_bytes() {
        let dots = "…".repeat(MAX_TITLE_CHARS);
        assert!(
            dots.len() > MAX_TITLE_CHARS,
            "and the fixture really is longer in bytes"
        );
        let new =
            new_scrobble(json!({"title": dots.as_str()})).expect("255 three-byte characters fit");
        assert_eq!(new.title, dots, "verbatim");
    }

    #[test]
    fn the_media_metadata_is_kept_verbatim() {
        let new = new_scrobble(json!({
            "title": "  A  B  ",
            "artist": "M83",
            "album": "HURRY UP",
            "length": 243_000,
        }))
        .expect("a full body validates");
        assert_eq!(new.title, "  A  B  ", "only checked, never trimmed");
        assert_eq!(new.artist.as_deref(), Some("M83"), "the artist");
        assert_eq!(new.album.as_deref(), Some("HURRY UP"), "the album");
        assert_eq!(new.length, Some(243_000), "the length as sent");
    }

    #[test]
    fn both_spellings_of_the_external_link_are_accepted() {
        let camel = new_scrobble(json!({
            "title": "x",
            "externalLink": "https://example.test/camel",
        }))
        .expect("Pleroma's camelCase spelling works");
        assert_eq!(
            camel.external_link.as_deref(),
            Some("https://example.test/camel"),
            "and lands in the same field"
        );

        let snake = new_scrobble(json!({
            "title": "x",
            "external_link": "https://example.test/snake",
        }))
        .expect("the snake_case spelling works too");
        assert_eq!(
            snake.external_link.as_deref(),
            Some("https://example.test/snake"),
            "as documented by Pleroma's own schema"
        );

        let both = new_scrobble(json!({
            "title": "x",
            "externalLink": "https://example.test/camel",
            "external_link": "https://example.test/snake",
        }))
        .expect("sending both is not an error");
        assert_eq!(
            both.external_link.as_deref(),
            Some("https://example.test/snake"),
            "and `external_link` wins, as in Pleroma's `put_new`"
        );
    }

    #[test]
    fn artist_and_album_are_capped_at_the_maximum_length() {
        for field in ["artist", "album"] {
            let edge = "x".repeat(MAX_TEXT_CHARS);
            let new = new_scrobble(json!({"title": "x", field: edge.as_str()}))
                .unwrap_or_else(|err| panic!("255 characters of {field} fit: {err}"));
            let kept = if field == "artist" {
                new.artist
            } else {
                new.album
            };
            assert_eq!(kept.as_deref(), Some(edge.as_str()), "{field} is verbatim");

            assert_eq!(
                rejection(json!({"title": "x", field: "x".repeat(MAX_TEXT_CHARS + 1)})),
                format!("{field}: is too long (maximum is 255 characters)"),
                "256 characters of {field} do not"
            );
        }
    }

    #[test]
    fn a_blank_artist_or_album_becomes_none() {
        let new = new_scrobble(json!({"title": "x", "artist": "  ", "album": ""}))
            .expect("blank metadata validates");
        assert!(new.artist.is_none(), "a blank artist is NULL");
        assert!(new.album.is_none(), "a blank album is NULL");
    }

    #[test]
    fn a_non_http_link_is_rejected() {
        for link in [
            "javascript:alert(1)",
            "data:text/html,x",
            "ftp://music.example/t/1",
            "music.example/t/1",
            "not a url",
        ] {
            for key in ["externalLink", "external_link"] {
                assert_eq!(
                    rejection(json!({"title": "x", key: link})),
                    LINK_INVALID,
                    "`{link}` under `{key}` is no http(s) link"
                );
            }
        }
    }

    #[test]
    fn an_http_link_passes_and_a_blank_one_becomes_none() {
        for link in ["https://music.example/t/1", "http://music.example/t/1"] {
            let new = new_scrobble(json!({"title": "x", "externalLink": link}))
                .unwrap_or_else(|err| panic!("`{link}` is valid: {err}"));
            assert_eq!(new.external_link.as_deref(), Some(link), "kept verbatim");
        }
        for blank in [json!(""), json!("   "), json!(null)] {
            let new = new_scrobble(json!({"title": "x", "externalLink": blank}))
                .expect("a blank link validates");
            assert!(new.external_link.is_none(), "`{blank}` is NULL");
        }
    }

    #[test]
    fn a_link_longer_than_the_maximum_is_rejected() {
        let prefix = "https://music.example/";
        let edge = format!("{prefix}{}", "a".repeat(MAX_LINK_CHARS - prefix.len()));
        assert_eq!(
            edge.chars().count(),
            MAX_LINK_CHARS,
            "the fixture is at the cap"
        );
        let new = new_scrobble(json!({"title": "x", "externalLink": edge.as_str()}))
            .expect("2048 characters fit");
        assert_eq!(
            new.external_link.as_deref(),
            Some(edge.as_str()),
            "verbatim"
        );

        assert_eq!(
            rejection(json!({"title": "x", "externalLink": format!("{edge}a")})),
            LINK_INVALID,
            "2049 do not"
        );
    }

    #[test]
    fn a_length_that_is_not_an_integer_is_a_parameter_error() {
        for value in [
            json!("abc"),
            json!(1.5),
            json!([1]),
            json!(3_000_000_000_u64),
        ] {
            let err = crate::extract::into_params::<ScrobbleParams>(json!({
                "title": "x",
                "length": value,
            }))
            .expect_err(&format!("`{value}` is no millisecond count"));
            assert!(
                err.to_string().contains("expected a number"),
                "and the flex message says so: {err}"
            );
        }
    }

    #[test]
    fn the_link_header_walks_from_both_ends_of_the_page() {
        let link = page_link("https://h", "/api/v1/pleroma/accounts/1/scrobbles", &rows())
            .expect("a full page links");
        assert_eq!(
            link,
            format!(
                "<https://h/api/v1/pleroma/accounts/1/scrobbles?max_id={OLDEST_ID}>; rel=\"next\", \
                 <https://h/api/v1/pleroma/accounts/1/scrobbles?min_id={NEWEST_ID}>; rel=\"prev\""
            ),
            "next walks to the oldest row, prev back to the newest"
        );
    }

    #[test]
    fn an_empty_page_links_nowhere() {
        assert_eq!(
            page_link("https://h", "/p", &[]),
            None,
            "no rows, no Link header"
        );
        let oldest_only = vec![rows().pop().expect("the fixture has rows")];
        let link = page_link("https://h", "/p", &oldest_only).expect("a one-row page links");
        assert!(
            link.contains(&format!("max_id={OLDEST_ID}"))
                && link.contains(&format!("min_id={OLDEST_ID}")),
            "and its single id is both cursors: {link}"
        );
    }

    #[tokio::test]
    async fn the_page_extractor_keeps_the_default_cap() {
        use axum::{body::Body, extract::FromRequestParts as _, http::Request};

        // The route takes the plain `PageParams` extractor, so [`MAX_LIMIT`]
        // is the cap and a bigger request falls to it.
        let mut parts = Request::get("/api/v1/pleroma/accounts/1/scrobbles?limit=41")
            .body(Body::empty())
            .expect("a valid test request")
            .into_parts()
            .0;
        let params = PageParams::from_request_parts(&mut parts, &())
            .await
            .expect("the fixture query paginates");
        assert_eq!(params.limit, MAX_LIMIT, "the cap is MAX_LIMIT");
        assert_eq!(page_of("limit=41").limit, MAX_LIMIT, "and so is the parse");
    }
}
