//! The wire half of the object fetcher: the budgeted, size-capped, signed
//! dereference and the containment checks on the document it returns.

use serde_json::Value;

use super::{FetchBudget, FetchError};
use crate::{
    config::Config,
    federation::{
        fetch_actor::{
            ContainmentRules, CrossHost, FetchError as ActorError, fetch_contained_document,
            fetch_signer,
        },
        mrf,
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
    refuse(state, url).await?;
    let http = object_client(&state.config);
    let signer = fetch_signer(state).await?;
    // The claimed id, when it differs from `url`, is on `url`'s authority
    // (`CrossHost::Deny`), so the host refused above covers it too.
    let rules = ContainmentRules {
        max_bytes: MAX_OBJECT_BODY_BYTES,
        cross_host: CrossHost::Deny,
    };
    fetch_contained_document(&http, url, signer.as_ref(), rules, |target| {
        take_budget(budget, target)
    })
    .await
    .map_err(FetchError::from)
}

/// Refuses to dereference `url` when the MRF policy rejects its host.
///
/// The refusal is a [`FetchError::Invalid`], a permanent failure to the
/// callers: nothing is sent to a domain the instance federates with no
/// one at.
async fn refuse(state: &AppState, url: &str) -> Result<(), FetchError> {
    mrf::refuse_rejected(state, url)
        .await
        .map_err(FetchError::Invalid)
}

/// Spends one dereference of `budget` for the `GET` of `url`. Fails in the
/// actor fetcher's vocabulary, which the shared containment speaks.
fn take_budget(budget: &FetchBudget, url: &str) -> Result<(), ActorError> {
    if budget.take() {
        return Ok(());
    }
    Err(ActorError::Invalid(format!(
        "the per-activity object fetch budget is spent (`{url}` not fetched)"
    )))
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
