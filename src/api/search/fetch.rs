//! The remote reads one resolving search spends its lookup on: the
//! handle's round trip through [`lookup_handle`] behind [`fetch_handle`],
//! the URL arm's actor and status reads ([`fetch_actor_url`],
//! [`fetch_status_url`]), their readers ([`web_url`],
//! [`local_permalink_id`]) and the [`RESOLVE_BUDGET`] gate
//! ([`within_budget`]) that runs each [`Lookup`] inside it.

use std::{future::Future, pin::Pin};

use tracing::debug;
use url::Url;
use uuid::Uuid;

use crate::{
    config::Config,
    federation::{
        fetch_actor::{FetchError, get_or_fetch_by_acct, get_or_fetch_by_ap_id, is_local_ap_id},
        fetch_object::resolve_status,
        webfinger_client::resolve_acct_with_base,
    },
    models::{
        status::{self, Status as StatusRow},
        user::User,
    },
    state::AppState,
    util::{id::parse_id, url::parse_http_url},
};

use super::{RESOLVE_BUDGET, STATUSES_SEGMENT, USERS_SEGMENT};
/// One resolution's remote lookup, pinned to a pointer: [`within_budget`]
/// waits on it, and a boxed future keeps the waiting search's own state
/// machine small however much state the federation dereference inside it
/// carries (the shape [`crate::jobs`] hands its job handlers, for the same
/// reason).
type Lookup<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The handle lookup of a resolving search: [`get_or_fetch_by_acct`] —
/// `WebFinger`, then the actor dereference, then the vouch check — under
/// [`RESOLVE_BUDGET`], every failure (and every clock that runs out) answering
/// `None` with a `debug` line, because the ticket's unresolved arm is an
/// empty list, not an error.
pub(super) async fn fetch_handle(
    state: &AppState,
    webfinger_base: Option<&str>,
    acct: &str,
) -> Option<User> {
    within_budget(Box::pin(async move {
        match lookup_handle(state, webfinger_base, acct).await {
            Ok(found) => Some(found),
            Err(err) => {
                debug!(%err, %acct, "a search could not resolve the handle");
                None
            }
        }
    }))
    .await
}

/// The one handle lookup behind [`fetch_handle`]. The production arm is
/// [`get_or_fetch_by_acct`] entire; the `Some(base)` arm runs the same
/// `WebFinger`-then-actor round trip against a mock peer at `base` and
/// skips only the vouch check `get_or_fetch_by_acct` closes it with — a mock
/// answering on loopback can never name the dotted domain of a handle
/// `WebFinger` accepts, so the check would reject every mock success. The
/// same seam [`crate::services::mentions::resolve_mentions_with_base`]
/// keeps, for the same reason.
async fn lookup_handle(
    state: &AppState,
    webfinger_base: Option<&str>,
    acct: &str,
) -> Result<User, FetchError> {
    match webfinger_base {
        None => get_or_fetch_by_acct(state, acct).await,
        Some(base) => {
            let ap_id = resolve_acct_with_base(&state.http_client, acct, Some(base))
                .await?
                .actor;
            get_or_fetch_by_ap_id(state, &ap_id).await
        }
    }
}

/// The actor reading of a URL query: [`get_or_fetch_by_ap_id`] under
/// [`RESOLVE_BUDGET`]. It answers a stored actor or a local one from the
/// database — an unknown id on this instance's own origin is refused without
/// any network at all — and mirrors a remote one it has never seen.
pub(super) async fn fetch_actor_url(state: &AppState, url: &Url) -> Option<User> {
    within_budget(Box::pin(async move {
        match get_or_fetch_by_ap_id(state, url.as_str()).await {
            Ok(found) => Some(found),
            Err(err) => {
                debug!(%err, "a search read a URL as an actor id and found no account");
                None
            }
        }
    }))
    .await
}

/// The status reading of a URL query: this instance's own perma-link is a
/// database read, and [`resolve_status`] does the rest — it answers a stored
/// row (its `/objects/{uuid}` shape and its `/notice/{uuid}` alias alike)
/// and nothing at all for an unknown local id, so the only local form left
/// out is the perma-link the resolver stores no uri under.
pub(super) async fn fetch_status_url(state: &AppState, url: &Url) -> Option<StatusRow> {
    if let Some(id) = local_permalink_id(&state.config, url) {
        let found = status::get_by_id(&state.db, id).await.ok().flatten();
        debug!(
            %id,
            stored = found.is_some(),
            "a local status perma-link resolved from the database"
        );
        return found;
    }
    within_budget(Box::pin(async move {
        match resolve_status(state, url.as_str()).await {
            Ok(found) => found,
            Err(err) => {
                debug!(%err, "a search read a URL as a status and found no post");
                None
            }
        }
    }))
    .await
}

/// The query as a URL federation can act on: `http` or `https` only —
/// anything else (a word, a handle, an `ftp:` or `mailto:` address) names no
/// remote document, and is [`None`].
pub(super) fn web_url(query: &str) -> Option<Url> {
    parse_http_url(query)
}

/// The id a local status perma-link names: `{BASE_URL}/users/{nickname}/
/// statuses/{id}` on this instance's own origin, with `id` a stored-shaped
/// id ([`parse_id`]). The nickname segment is the client's slug and decides
/// nothing — the id is the row. Every other URL is [`None`], including a
/// local URL of another shape, which the resolver's own local arms settle.
fn local_permalink_id(config: &Config, url: &Url) -> Option<Uuid> {
    if !is_local_ap_id(config, url.as_str()) {
        return None;
    }
    let mut segments = url.path().trim_start_matches('/').split('/');
    if segments.next() != Some(USERS_SEGMENT) {
        return None;
    }
    if segments.next().is_none_or(str::is_empty) {
        return None;
    }
    if segments.next() != Some(STATUSES_SEGMENT) {
        return None;
    }
    let id = segments.next().filter(|id| !id.is_empty())?;
    if segments.next().is_some() {
        return None;
    }
    parse_id(id)
}

/// Runs one resolution `future` under [`RESOLVE_BUDGET`], answering whatever
/// it answers or [`None`] when the budget runs out first. A lookup cut off
/// mid-flight is simply dropped: the ticket's silent failure, and the same
/// posture a remote mention lookup takes (see [`crate::services::ingest`]).
///
/// The future arrives boxed for the same reason [`crate::jobs`] boxes its
/// handler futures: a federation dereference is kilobytes of state machine,
/// and the search's own future holds a pointer to it instead.
async fn within_budget<T>(future: Lookup<'_, Option<T>>) -> Option<T> {
    tokio::time::timeout(RESOLVE_BUDGET, future)
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TEST_BASE_URL;

    #[test]
    fn only_an_http_url_is_read_as_an_address() {
        for query in [
            "https://remote.example/users/zoe",
            "http://127.0.0.1:8080/a",
        ] {
            let url = web_url(query).expect("the address parses as a web URL");
            assert!(matches!(url.scheme(), "http" | "https"), "{query} is web");
        }
        for query in [
            "alice@remote.example",
            "@alice",
            "ftp://remote.example/pub",
            "mailto:alice@remote.example",
            "//remote.example/users/zoe",
            "",
        ] {
            assert!(web_url(query).is_none(), "`{query}` names no document");
        }
    }

    #[test]
    fn a_local_permalink_is_read_by_id_and_nothing_else() {
        let config = Config::for_tests();
        let id = Uuid::now_v7();
        for (path, want) in [
            (format!("/users/bob/statuses/{id}"), Some(id)),
            (format!("/users/bob/statuses/{}", id.simple()), Some(id)),
            (format!("/users/bob/statuses/{id}/replies"), None),
            ("/users/bob/statuses/nope".to_owned(), None),
            (format!("/users//statuses/{id}"), None),
            ("/users/bob/statuses".to_owned(), None),
            ("/users/bob".to_owned(), None),
            (format!("/notice/{id}"), None),
            (format!("/objects/{id}"), None),
        ] {
            let url = Url::parse(&format!("{TEST_BASE_URL}{path}")).expect("the fixture URL");
            assert_eq!(
                local_permalink_id(&config, &url),
                want,
                "`{path}` is no perma-link of ours"
            );
        }
        let elsewhere = Url::parse(&format!("http://127.0.0.1:9999/users/bob/statuses/{id}"))
            .expect("the foreign URL");
        assert_eq!(
            local_permalink_id(&config, &elsewhere),
            None,
            "another origin's perma-link is not ours to read from the database"
        );
    }
}
