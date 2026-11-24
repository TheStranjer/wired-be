//! The remote side of the install routes: the request shapes of
//! `POST /packs/download` and `/packs/download_zip`, the guarded fetches
//! of another instance's pack listing and archive, and the relay route
//! `GET /packs/remote`.

use std::collections::BTreeMap;

use axum::{Json, extract::State};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;
use url::form_urlencoded;

use super::{
    AREA_EMOJI, CLASS_WRITE, COULD_NOT_FETCH, INVALID_URL, MAX_FETCH_STATUS, MAX_REMOTE_LIST_BYTES,
    MIN_FETCH_STATUS, NAME_EMPTY, REMOTE_PACK_PATH, REMOTE_PACKS_PATH, params::PackParams,
};
use crate::{
    auth::admin::AdminUser,
    error::{AppError, AppResult},
    http_client::{HttpError, HttpResponse},
    services::emoji_packs,
    state::AppState,
    util::url::parse_http_url,
};

/// The `url`, `name` and optional `as` body of `POST /packs/download`:
/// the remote instance to install from, the pack on it, and the local
/// name to install it under (Pleroma's `as`, defaulted to `name`).
#[derive(Debug, Deserialize)]
pub(super) struct DownloadParams {
    /// The remote instance's base URL.
    pub(super) url: String,
    /// The pack's name on the remote.
    pub(super) name: String,
    /// The local pack name, absent for the remote one.
    #[serde(default, rename = "as")]
    pub(super) as_name: Option<String>,
}

/// One validated install request, ready to build remote URLs from.
#[derive(Debug)]
pub(super) struct DownloadRequest<'a> {
    /// The remote instance's base URL, trimmed.
    pub(super) base: &'a str,
    /// The pack's name on the remote, trimmed.
    pub(super) remote_name: &'a str,
    /// The local pack name: `as` when non-blank, else the remote name.
    pub(super) local_name: &'a str,
}

impl<'a> DownloadRequest<'a> {
    /// Validates `params` the way the module's other writes validate a
    /// name: a blank remote name is the shared empty-name 400, and the
    /// local name must be one this instance may create a directory for.
    /// The base URL is checked where it becomes a fetch target
    /// ([`instance_api_url`]).
    pub(super) fn from_params(params: &'a DownloadParams) -> AppResult<Self> {
        let remote_name = params.name.trim();
        if remote_name.is_empty() {
            return Err(AppError::BadRequest(NAME_EMPTY.to_owned()));
        }
        let local_name = params.as_name.as_deref().unwrap_or_default().trim();
        let local_name = if local_name.is_empty() {
            remote_name
        } else {
            local_name
        };
        if !emoji_packs::pack_name_valid(local_name) {
            return Err(AppError::BadRequest(format!(
                "invalid pack name: {local_name}"
            )));
        }
        Ok(Self {
            base: params.url.trim(),
            remote_name,
            local_name,
        })
    }
}

/// The `name` parameter of `POST /packs/download_zip` (query or body,
/// [`create_file`](super::file_writes::create_file) reads the pair alike);
/// the archive itself rides in the multipart `file` part.
#[derive(Debug, Deserialize)]
pub(super) struct DownloadZipParams {
    /// Raw pack `name` value, if present.
    #[serde(default)]
    pub(super) name: Option<String>,
}

/// One pack as a remote instance's `/api/v1/pleroma/emoji/pack` answers
/// it: the same triple [`PackView`](super::reads::PackView) serializes,
/// read back from a peer.
/// Unknown keys (Pleroma's `files_extra`) are ignored.
#[derive(Debug, Deserialize)]
pub(super) struct RemotePackListing {
    /// The manifest metadata block: its `share-files` flag and
    /// `fallback-src` decide where the archive comes from.
    #[serde(default)]
    pub(super) pack: emoji_packs::PackMeta,
    /// Shortcode to file name, as the remote paginated them.
    #[serde(default)]
    pub(super) files: BTreeMap<String, String>,
    /// Total files on the remote, before pagination.
    #[serde(default)]
    pub(super) files_count: usize,
}

/// The 400 for a `url` this instance's client could never call.
fn bad_url() -> AppError {
    AppError::BadRequest(INVALID_URL.to_owned())
}

/// The URL of the instance `base` answers `path` at, with `query`
/// percent-encoded onto it — Pleroma's `URI.merge` of the instance URL
/// with the fixed API path. A `base` that parses to no http(s) address
/// is [`bad_url`]'s 400; whether the SSRF guard lets the host through is
/// the fetch's answer, not this function's.
pub(super) fn instance_api_url(
    base: &str,
    path: &str,
    query: &[(&str, &str)],
) -> AppResult<String> {
    let parsed = parse_http_url(base.trim()).ok_or_else(bad_url)?;
    let mut target = parsed.join(path).map_err(|_err| bad_url())?;
    let query_text = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(query.iter().copied())
        .finish();
    target.set_query(Some(&query_text));
    Ok(target.into())
}

/// The 500 for a failed remote fetch: the fixed client-safe `message`
/// on the wire, the cause in the log — Pleroma answers its remote
/// actions' fetch failures with a 500 whose body names what failed
/// (`emoji_pack_controller.ex`), which [`AppError::Internal`]'s generic
/// message could not.
fn remote_failure(message: &str, cause: &str) -> AppError {
    debug!(cause, "emoji pack remote fetch failed");
    AppError::InternalMessage(message.to_owned())
}

/// Why [`fetch_checked`] gave no usable response; each caller words it
/// for its own route.
#[derive(Debug)]
pub(super) enum FetchFailure {
    /// The guarded client refused or failed the fetch: the SSRF guard,
    /// transport, or the size cap.
    Transport(HttpError),
    /// The peer answered with this status outside 2xx.
    Status(u16),
}

impl FetchFailure {
    /// The cause as the log words it.
    fn cause(&self) -> String {
        match self {
            Self::Transport(err) => err.to_string(),
            Self::Status(status) => format!("the peer answered {status}"),
        }
    }
}

/// Fetches `url` with the state's guarded client, reading at most
/// `max_bytes`, and insists on a 2xx answer.
pub(super) async fn fetch_checked(
    state: &AppState,
    url: &str,
    max_bytes: usize,
) -> Result<HttpResponse, FetchFailure> {
    let response = state
        .http_client
        .get_limited(url, &[], max_bytes)
        .await
        .map_err(FetchFailure::Transport)?;
    if !(MIN_FETCH_STATUS..MAX_FETCH_STATUS).contains(&response.status) {
        return Err(FetchFailure::Status(response.status));
    }
    Ok(response)
}

/// Fetches `url` through [`fetch_checked`]; every failure — transport,
/// guard, size cap, status — arrives as the 500 that `message` words.
pub(super) async fn fetch_remote(
    state: &AppState,
    url: &str,
    max_bytes: usize,
    message: &str,
) -> AppResult<HttpResponse> {
    fetch_checked(state, url, max_bytes)
        .await
        .map_err(|failure| remote_failure(message, &failure.cause()))
}

/// Fetches `url` under the listing cap ([`MAX_REMOTE_LIST_BYTES`]) and
/// requires a JSON body; bytes that parse to no JSON are as good as no
/// answer.
async fn fetch_remote_json(state: &AppState, url: &str) -> AppResult<Value> {
    let response = fetch_remote(state, url, MAX_REMOTE_LIST_BYTES, COULD_NOT_FETCH).await?;
    serde_json::from_slice(&response.body)
        .map_err(|err| remote_failure(COULD_NOT_FETCH, &err.to_string()))
}

/// Fetches the listing of the pack `request` names on its instance,
/// asking for `page_size` files; Pleroma's `download` reads
/// `files_count` from a first page and refetches with it as the size,
/// and this keeps that shape.
pub(super) async fn fetch_pack_listing(
    state: &AppState,
    request: &DownloadRequest<'_>,
    page_size: &str,
) -> AppResult<RemotePackListing> {
    let url = instance_api_url(
        request.base,
        REMOTE_PACK_PATH,
        &[
            ("name", request.remote_name),
            ("page", "1"),
            ("page_size", page_size),
        ],
    )?;
    let body = fetch_remote_json(state, &url).await?;
    serde_json::from_value(body).map_err(|err| remote_failure(COULD_NOT_FETCH, &err.to_string()))
}

/// `GET /api/v1/pleroma/emoji/packs/remote?url=…` — Pleroma's `remote`:
/// another instance's pack listing, its `{"count","packs"}` object
/// fetched from `{url}/api/v1/pleroma/emoji/packs` (paginated by the
/// `page` and `page_size` of the request, this module's defaults and
/// cap included) through the SSRF-guarded client at the 5 MiB cap, and
/// relayed as it answered. A `url` that is no http(s) address is the
/// 400; a remote that is down, answers a non-2xx or talks non-JSON is
/// the 500 `{"error":"Could not fetch the pack list"}`. An admin with
/// `admin:write` (as Pleroma demands; `admin:read` is refused).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] for a `url` no
/// client may call ([`INVALID_URL`]); [`AppError::InternalMessage`] for
/// every remote failure; other read failures arrive unchanged.
pub(super) async fn remote(
    State(state): State<AppState>,
    admin: AdminUser,
    params: PackParams,
) -> AppResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_EMOJI)?;
    let page = params.page.to_string();
    let page_size = params.page_size.to_string();
    let url = instance_api_url(
        params.url.as_deref().unwrap_or_default(),
        REMOTE_PACKS_PATH,
        &[("page", &page), ("page_size", &page_size)],
    )?;
    let body = fetch_remote_json(&state, &url).await?;
    debug!("emoji pack remote listing relayed");
    Ok(Json(body))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::emoji_packs::{REMOTE_ARCHIVE_PATH, test_support::download_body};

    #[test]
    fn the_remote_urls_are_built_on_the_instances_api_paths() {
        let packs = instance_api_url(
            " https://remote.example/ ",
            REMOTE_PACKS_PATH,
            &[("page", "2"), ("page_size", "7")],
        )
        .unwrap_or_else(|err| panic!("a good base parses: {err}"));
        assert_eq!(
            packs, "https://remote.example/api/v1/pleroma/emoji/packs?page=2&page_size=7",
            "the base is trimmed and the API path and query appended"
        );
        let named = instance_api_url(
            "https://remote.example",
            REMOTE_PACK_PATH,
            &[("name", "two words"), ("page", "1"), ("page_size", "3")],
        )
        .unwrap();
        assert_eq!(
            named,
            "https://remote.example/api/v1/pleroma/emoji/pack?name=two+words&page=1&page_size=3",
            "the pack name rides percent-encoded like every query value"
        );
        let hosted = instance_api_url(
            "https://remote.example/pleroma",
            REMOTE_ARCHIVE_PATH,
            &[("name", "cats")],
        )
        .unwrap();
        assert_eq!(
            hosted, "https://remote.example/api/v1/pleroma/emoji/packs/archive?name=cats",
            "an absolute API path replaces the base path, Pleroma's URI.merge"
        );
    }

    #[test]
    fn a_url_no_client_could_ever_call_is_the_400() {
        for base in ["", "   ", "not a url", "ftp://remote.example", "https://"] {
            let err = instance_api_url(base, REMOTE_PACKS_PATH, &[]).unwrap_err();
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "{base:?} is a 400, not {err}"
            );
            assert_eq!(err.to_string(), INVALID_URL, "the message is pinned");
        }
    }

    #[test]
    fn the_download_body_takes_url_name_and_an_as_override() {
        let full: DownloadParams = serde_json::from_value(json!({
            "url": "https://remote.example",
            "name": "cats",
            "as": "local-cats",
        }))
        .unwrap();
        let request = DownloadRequest::from_params(&full).unwrap();
        assert_eq!(
            (request.base, request.remote_name, request.local_name),
            ("https://remote.example", "cats", "local-cats"),
            "the dashed body key renames into `as_name`"
        );

        let trimmed_body = download_body(" https://remote.example ", "  cats  ", Some("   "));
        let trimmed = DownloadRequest::from_params(&trimmed_body).unwrap();
        assert_eq!(
            (trimmed.base, trimmed.remote_name, trimmed.local_name),
            ("https://remote.example", "cats", "cats"),
            "everything is trimmed and a blank `as` stands aside for the remote name"
        );

        let blank = download_body("https://x.test", "  ", None);
        let empty_name_err = DownloadRequest::from_params(&blank).unwrap_err();
        assert_eq!(
            empty_name_err.to_string(),
            NAME_EMPTY,
            "a blank pack name is the 400"
        );

        let escaping = download_body("https://x.test", "cats", Some("../evil"));
        let escape_err = DownloadRequest::from_params(&escaping).unwrap_err();
        assert_eq!(
            escape_err.to_string(),
            "invalid pack name: ../evil",
            "no local name can name an escaping directory"
        );
    }

    #[test]
    fn the_remote_listing_reads_pleromas_pack_triple() {
        let listing: RemotePackListing = serde_json::from_value(json!({
            "files": {"cat": "cat.png"},
            "files_count": 1,
            "files_extra": {"cat": {"tags": ["pack:cats"]}},
            "pack": {
                "license": "MIT",
                "share-files": true,
                "can-download": true,
                "download-sha256": "beef",
            },
        }))
        .unwrap();
        assert!(listing.pack.share_files, "the share flag parsed");
        assert_eq!(
            listing.files.get("cat").map(String::as_str),
            Some("cat.png"),
            "the shortcode map parsed"
        );
        assert_eq!(listing.files_count, 1, "and the total count");

        let empty: RemotePackListing = serde_json::from_value(json!({})).unwrap();
        assert!(
            !empty.pack.share_files && empty.files.is_empty() && empty.files_count == 0,
            "every key is optional on read"
        );
        let err = serde_json::from_value::<RemotePackListing>(json!("just a string"))
            .expect_err("a non-object body is no listing");
        assert!(!err.to_string().is_empty(), "and says why: {err}");
    }
}
