//! The wire half of the object fetcher: the budgeted, size-capped, signed
//! dereference and the containment checks on the document it returns.

use serde_json::Value;
use url::Url;

use super::{FetchBudget, FetchError};
use crate::{
    config::Config,
    federation::fetch_actor::{
        Signer, ensure_not_redirected, ensure_same_host, fetch_document, fetch_signer,
        names_served_url,
    },
    http_client::{HttpClient, user_agent},
    state::AppState,
};

/// Cap on one fetched object document: 2 MiB. Notes carry content, tags
/// and attachment lists; anything larger is an attack or a mistake.
pub(super) const MAX_OBJECT_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Redirects one object `GET` follows. Tighter than the shared client's
/// budget on purpose: an object id that needs more hops to resolve is not
/// resolving (see the module docs).
const MAX_OBJECT_REDIRECTS: usize = 3;

/// Dereferences a remote object over HTTP with a fresh fetch budget.
///
/// The `GET` asks for both `ActivityPub` JSON media types, follows at most
/// `MAX_OBJECT_REDIRECTS` redirects, caps the body at 2 MiB and refuses
/// an answer whose content type does not say `json`; with `SIGN_FETCHES`
/// on it carries the instance actor's HTTP Signature, like every other
/// federation fetch (see `fetch_signer`).
///
/// The returned document is contained as the [module docs](super) describe:
/// its `id` names the fetched URL — or, on the same origin, a document
/// fetched once at its own `id` that names itself.
///
/// # Errors
///
/// [`FetchError::Gone`] for a 410, [`FetchError::NotFound`] for a 404,
/// [`FetchError::Http`] for any other transport or status failure
/// (including a body over the cap), and [`FetchError::Invalid`] for a
/// non-JSON content type, a body that is not parseable JSON with a string
/// `id`, a document that fails containment, or a fetch past the budget.
pub async fn fetch_object_json(state: &AppState, url: &str) -> Result<Value, FetchError> {
    let budget = FetchBudget::new(state.config.fetch.limits.max_fetches_per_activity);
    fetch_contained(state, url, &budget).await
}

/// Dereferences `url` under `budget` and returns the contained document
/// (see the [module docs](super)): the fetch may not be redirected to
/// another host, and the `id` must name the URL that served it, or a URL
/// on the same origin that is dereferenced once more, without a redirect,
/// and names itself.
pub(super) async fn fetch_contained(
    state: &AppState,
    url: &str,
    budget: &FetchBudget,
) -> Result<Value, FetchError> {
    let http = object_client(&state.config);
    let signer = fetch_signer(state).await?;
    let (document, served) = fetch_bounded(&http, url, signer.as_ref(), budget).await?;
    ensure_same_host(url, &served)?;
    let claimed = declared_id(&document)?;
    if names_served_url(url, &served, claimed) {
        return Ok(document);
    }
    if !same_authority(url, claimed) {
        return Err(FetchError::Invalid(format!(
            "the object fetched from `{url}` names `{claimed}`, a different origin"
        )));
    }
    // Same origin but a different id than the URL that served it: follow
    // the claim exactly once, without a redirect, and accept only a
    // document that names itself.
    let (refetched, refetched_from) =
        fetch_bounded(&http, claimed, signer.as_ref(), budget).await?;
    ensure_not_redirected(claimed, &refetched_from)?;
    let refetched_id = declared_id(&refetched)?;
    if refetched_id != claimed {
        return Err(FetchError::Invalid(format!(
            "the object `{claimed}` identifies itself as `{refetched_id}`"
        )));
    }
    Ok(refetched)
}

/// One budgeted document `GET`: spends a dereference and runs the shared
/// signed federation fetch against the object body cap.
async fn fetch_bounded(
    http: &HttpClient,
    url: &str,
    signer: Option<&Signer>,
    budget: &FetchBudget,
) -> Result<(Value, Url), FetchError> {
    if !budget.take() {
        return Err(FetchError::Invalid(format!(
            "the per-activity object fetch budget is spent (`{url}` not fetched)"
        )));
    }
    fetch_document(http, url, signer, MAX_OBJECT_BODY_BYTES)
        .await
        .map_err(FetchError::from)
}

/// The client the object fetches run on: the instance's outbound identity
/// and SSRF posture exactly as the application's shared client takes from
/// [`Config`], with the tighter redirect budget the module docs state.
fn object_client(config: &Config) -> HttpClient {
    HttpClient::with_redirect_limit(
        &user_agent(&config.base_url, config.contact_email.as_deref()),
        config.fetch.http_allow_private_hosts,
        MAX_OBJECT_REDIRECTS,
    )
}

/// The document's self-declared `id`: every containment rule compares it
/// against the URL the document was fetched from.
fn declared_id(document: &Value) -> Result<&str, FetchError> {
    document
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| FetchError::Invalid("the object document carries no string `id`".to_owned()))
}

/// Whether two absolute URLs name the same origin: the scheme-less
/// authority (`host:port`) compared on the parsed URL, the same measure
/// [`is_local_ap_id`] keeps. An unparsable side matches nothing.
fn same_authority(left: &str, right: &str) -> bool {
    let Ok(parsed_left) = Url::parse(left) else {
        return false;
    };
    Url::parse(right).is_ok_and(|parsed_right| parsed_left.authority() == parsed_right.authority())
}
