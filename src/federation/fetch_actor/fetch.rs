//! The wire half: fetching and containing an actor document.
//!
//! [`fetch_actor_json`] is the entry point; `fetch_document` is the `GET`
//! it runs, `is_activitypub_media_type` and `document_id` the checks an answer must
//! pass, and `fetch_and_store` the run of a fetched document through the
//! filters into the mirror row.

use reqwest::header::{CONTENT_TYPE, HeaderMap};
use serde_json::Value;
use url::Url;

use crate::{
    federation::{
        mrf,
        remote_actor::{parse_actor, upsert_remote_user},
        vocab::{ACTIVITY_JSON, LD_JSON},
    },
    http_client::HttpClient,
    models::user::{User, get_by_ap_id},
    state::AppState,
    util::url::same_origin,
};

use super::{
    ACTIVITYSTREAMS_PROFILE, ACTOR_ACCEPT, FetchError, MAX_BODY_BYTES, STATUS_GONE,
    STATUS_NOT_FOUND,
    containment::{ensure_not_redirected, ensure_same_host, names_served_url},
    error::server_error,
    handle::{Naming, confirm_nickname, keep_stored_nickname},
    signer::{Signer, fetch_signer, sign_get},
};

/// Dereferences an actor document over HTTP.
///
/// The `GET` asks for both `ActivityPub` JSON media types, follows
/// redirects under the shared client's policy, caps the body at 1 MiB and
/// refuses an answer whose content type is not an `ActivityPub` one
/// (`application/activity+json`, or `application/ld+json` with the
/// `ActivityStreams` profile). With `signer`
/// the request is additionally HTTP-signed (see
/// [`sign_request`](crate::federation::signature::sign_request)), the shape of an authorized fetch.
///
/// The returned document is contained as the [module
/// docs](super) describe: its `id` either names the URL that served it or
/// names a document fetched once, without a redirect, at its own `id` that
/// names itself. A redirect to another host is rejected.
///
/// # Errors
///
/// [`FetchError::Gone`] for a 410, [`FetchError::NotFound`] for a 404,
/// [`FetchError::Http`] for any other transport or status failure
/// (including a body over the cap), and [`FetchError::Invalid`] for a
/// non-`ActivityPub` content type, a body that is not a JSON object with a string
/// `id`, a document that moves its `id` on while being refetched, or a
/// `signer` that cannot sign this URL.
pub async fn fetch_actor_json(
    http: &HttpClient,
    url: &str,
    signer: Option<&Signer>,
) -> Result<Value, FetchError> {
    let rules = ContainmentRules {
        max_bytes: MAX_BODY_BYTES,
        cross_host: CrossHost::Allow,
    };
    fetch_contained_document(http, url, signer, rules, |_url| Ok(())).await
}

/// What distinguishes one fetcher's containment from the other's: the body
/// cap and the cross-host rule. Bundled to keep
/// [`fetch_contained_document`] within the argument limit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ContainmentRules {
    /// Cap on one fetched document body, in bytes.
    pub(crate) max_bytes: usize,
    /// Whether a claimed id may name another authority.
    pub(crate) cross_host: CrossHost,
}

/// Whether a document may claim an `id` on another authority
/// (`host:port`) than the URL it was fetched from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CrossHost {
    /// The claimed id is followed wherever it points (actor documents).
    Allow,
    /// A claimed id on another authority is refused without a request
    /// (object documents).
    Deny,
}

/// Fetches `url` and returns the document contained as the [module
/// docs](super) describe: the fetch may not be redirected to another host,
/// and the document's `id` must name the URL that served it, or name a
/// document fetched once more at that `id`, without a redirect, that names
/// itself. This is the only implementation of that refetch-once rule; the
/// actor and object fetchers differ only in `rules` (the body cap, and with
/// [`CrossHost::Deny`] a claimed id on another authority is
/// [`FetchError::Invalid`] before any second request) and in `before_fetch`,
/// which runs with the URL ahead of every `GET` and may refuse it (the
/// object fetcher spends its fetch budget there).
///
/// # Errors
///
/// Those of [`fetch_actor_json`], plus whatever `before_fetch` returns and
/// a claimed id refused under [`CrossHost::Deny`].
pub(crate) async fn fetch_contained_document(
    http: &HttpClient,
    url: &str,
    signer: Option<&Signer>,
    rules: ContainmentRules,
    mut before_fetch: impl FnMut(&str) -> Result<(), FetchError>,
) -> Result<Value, FetchError> {
    let ContainmentRules {
        max_bytes,
        cross_host,
    } = rules;
    before_fetch(url)?;
    let (document, served) = fetch_document(http, url, signer, max_bytes).await?;
    ensure_same_host(url, &served)?;
    let claimed = document_id(&document)?;
    if names_served_url(url, &served, claimed) {
        return Ok(document);
    }
    if cross_host == CrossHost::Deny && !same_authority(url, claimed) {
        return Err(FetchError::Invalid(format!(
            "the document fetched from `{url}` names `{claimed}`, a different origin"
        )));
    }
    // The document names a different id than the URL that served it:
    // follow the claim exactly once, without a redirect, and accept only a
    // document that names itself.
    before_fetch(claimed)?;
    let (refetched, refetched_from) = fetch_document(http, claimed, signer, max_bytes).await?;
    ensure_not_redirected(claimed, &refetched_from)?;
    let refetched_id = document_id(&refetched)?;
    if refetched_id != claimed {
        return Err(FetchError::Invalid(format!(
            "the document `{claimed}` identifies itself as `{refetched_id}`"
        )));
    }
    Ok(refetched)
}

/// Whether two absolute URLs name the same origin: the scheme-less
/// authority (`host:port`) compared on the parsed URL. An unparsable side
/// matches nothing.
fn same_authority(left: &str, right: &str) -> bool {
    let (Ok(parsed_left), Ok(parsed_right)) = (Url::parse(left), Url::parse(right)) else {
        return false;
    };
    same_origin(&parsed_left, &parsed_right)
}

/// `GET`s `url` once and reads the document it answers, applying the
/// status, size, content-type and JSON rules of [`fetch_actor_json`] with
/// the body cap at `max_bytes` (the object fetcher, see
/// [`crate::federation::fetch_object`], caps at its own 2 MiB through
/// [`fetch_contained_document`]). Returns the document with the final URL
/// that served it, so callers can contain redirects.
async fn fetch_document(
    http: &HttpClient,
    url: &str,
    signer: Option<&Signer>,
    max_bytes: usize,
) -> Result<(Value, Url), FetchError> {
    let signed_request = signer.map(|key| sign_get(url, key)).transpose()?;
    let mut headers = vec![("accept", ACTOR_ACCEPT)];
    if let Some(signed) = &signed_request {
        // Host is sent explicitly so the peer sees exactly the host that
        // was signed.
        headers.extend(signed.header_pairs());
    }
    let response = http.get_limited(url, &headers, max_bytes).await?;
    if response.status == STATUS_GONE {
        return Err(FetchError::Gone);
    }
    if response.status == STATUS_NOT_FOUND {
        return Err(FetchError::NotFound);
    }
    let response = response.error_for_status()?;
    if !is_activitypub_media_type(&response.headers) {
        return Err(FetchError::Invalid(format!(
            "the response from `{url}` does not declare an ActivityPub content type"
        )));
    }
    let document: Value = response.json().map_err(|err| {
        FetchError::Invalid(format!("the body of `{url}` is not parseable JSON: {err}"))
    })?;
    Ok((document, response.url))
}

/// Whether a response declares an `ActivityPub` body, as Mastodon and
/// Pleroma require: `application/activity+json`, or `application/ld+json`
/// whose `profile` parameter lists the `ActivityStreams` namespace. The
/// media type and parameter name compare case-insensitively and other
/// parameters (`charset`) are ignored. Plain `application/json`, any other
/// type, and an absent or unreadable header fail.
fn is_activitypub_media_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let mut parts = value.split(';');
    let media_type = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    match media_type.as_str() {
        ACTIVITY_JSON => true,
        LD_JSON => parts.any(declares_activitystreams_profile),
        _ => false,
    }
}

/// Whether one `;`-separated media-type parameter is a `profile` whose
/// (optionally quoted) space-separated list names `ActivityStreams`.
fn declares_activitystreams_profile(parameter: &str) -> bool {
    let Some((name, value)) = parameter.split_once('=') else {
        return false;
    };
    name.trim().eq_ignore_ascii_case("profile")
        && value
            .trim()
            .trim_matches('"')
            .split_whitespace()
            .any(|profile| profile == ACTIVITYSTREAMS_PROFILE)
}

/// The document's own `id`: every containment rule compares it against the
/// URL the document was fetched from.
fn document_id(document: &Value) -> Result<&str, FetchError> {
    document
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| FetchError::Invalid("the document carries no string `id`".to_owned()))
}

/// Dereferences `ap_id`, runs the actor document through the instance's
/// [filter chain](mrf::filter_actor), parses it and stores the mirror row
/// (see [`crate::federation::remote_actor`]).
///
/// With `SIGN_FETCHES` on (the default) every `GET` carries the instance
/// actor's HTTP Signature, so peers in authorized-fetch mode answer; with
/// it off the dereference goes out unsigned, like Pleroma's default.
///
/// A host the `simple` MRF policy rejects (or its allowlist omits) is
/// refused before anything is sent: [`FetchError::Invalid`].
///
/// The nickname the parser derives (`preferredUsername@<id host>`) is
/// settled before the write as `naming` says: with [`Naming::Confirm`] it
/// runs through [`confirm_nickname`], so a split-domain account keeps the
/// handle its `WebFinger` answer gave it; with [`Naming::Caller`] the
/// caller already holds a `WebFinger` answer for this actor, so no second
/// lookup goes out and [`keep_stored_nickname`] only keeps a stored
/// split-domain handle.
pub(super) async fn fetch_and_store(
    state: &AppState,
    ap_id: &str,
    naming: Naming,
) -> Result<User, FetchError> {
    mrf::refuse_rejected(state, ap_id)
        .await
        .map_err(FetchError::Invalid)?;
    let signer = fetch_signer(state).await?;
    let document = fetch_actor_json(&state.http_client, ap_id, signer.as_ref()).await?;
    let document = mrf::filter_actor(state, document).await;
    let mut new_user = parse_actor(&document).map_err(FetchError::from)?;
    let stored = get_by_ap_id(&state.db, &new_user.ap_id)
        .await
        .map_err(|err| server_error(&err))?;
    match naming {
        Naming::Confirm => confirm_nickname(state, stored.as_ref(), &mut new_user).await,
        Naming::Caller => keep_stored_nickname(stored.as_ref(), &mut new_user),
    }
    let user = upsert_remote_user(&state.db, new_user)
        .await
        .map_err(FetchError::from)?;
    Ok(user)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Router,
        extract::State,
        http::StatusCode,
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use serde_json::json;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::{
        federation::fetch_actor::{get_or_fetch_by_ap_id, test_support::*},
        http_client::HttpError,
        services::runtime_config,
    };

    /// A peer on a fresh loopback port answering every `/actor` request
    /// with `status`; returns the error the fetch reported.
    async fn fetch_failing(status: StatusCode) -> FetchError {
        let listener = bind().await;
        let url = format!("{}/actor", origin(&listener));
        let server = spawn(
            listener,
            Router::new()
                .route("/actor", get(status_only))
                .with_state(status),
        );
        let error = fetch_actor_json(&client(), &url, None).await.unwrap_err();
        server.abort();
        error
    }

    /// The `Invalid` detail of `error`, panicking on any other variant.
    fn invalid_detail(error: FetchError) -> String {
        match error {
            FetchError::Invalid(detail) => detail,
            FetchError::Http(_)
            | FetchError::Gone
            | FetchError::NotFound
            | FetchError::AcctMismatch
            | FetchError::Webfinger(_) => {
                panic!("an actor fetch must be rejected as Invalid, got {error:?}")
            }
        }
    }

    /// The actor stage of the chain runs over a fetched document before it
    /// is mirrored: what the domain lists strip never reaches the row, and
    /// what they leave alone does.
    #[tokio::test]
    async fn a_fetched_document_is_stripped_by_the_mrf_actor_lists() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listener = bind().await;
        let base = origin(&listener);
        let ap_id = format!("{base}/users/alice");
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(profiled_actor))
                .with_state(peer_at(&base)),
        );
        runtime_config::write(
            &pool,
            ":pleroma",
            ":mrf",
            &json!({
                "policies": ["simple"],
                "simple": {"avatar_removal": ["127.0.0.1"]},
            }),
        )
        .await
        .expect("the mrf entry writes");
        let state = test_state(pool);

        let mirrored = get_or_fetch_by_ap_id(&state, &ap_id).await.unwrap();
        server.abort();

        assert_eq!(mirrored.name, "First", "the document was mirrored");
        assert!(
            mirrored.avatar_url.is_none(),
            "the avatar the lists strip never lands"
        );
        assert!(
            mirrored.banner_url.is_some(),
            "the banner no list names is stored"
        );
    }

    /// Dereferences the loopback mock's actor under an optional MRF config.
    ///
    /// The `simple` policy is configured by `lists` (no MRF entry at all
    /// for `None`) and the fetch goes through [`get_or_fetch_by_ap_id`];
    /// returns the answer and how many requests the mock saw.
    async fn fetch_loopback_actor(lists: Option<&Value>) -> (Result<User, FetchError>, usize) {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        if let Some(lists) = lists {
            runtime_config::write(
                &pool,
                ":pleroma",
                ":mrf",
                &json!({"policies": ["simple"], "simple": lists}),
            )
            .await
            .expect("the mrf entry writes");
        }
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(self_actor))
                .with_state(peer),
        );
        let state = test_state(pool);

        let answer = get_or_fetch_by_ap_id(&state, &format!("{base}/users/alice")).await;
        server.abort();
        (answer, hits.load(Ordering::SeqCst))
    }

    /// A host the `simple` policy rejects — by its `reject` list or by an
    /// allowlist that omits it — is refused before a request is sent, and
    /// nothing is mirrored.
    #[tokio::test]
    async fn an_actor_of_a_rejected_host_is_not_fetched() {
        for lists in [
            json!({"reject": ["127.0.0.1"]}),
            json!({"accept": ["friend.example"]}),
        ] {
            let (answer, hits) = fetch_loopback_actor(Some(&lists)).await;

            let detail = invalid_detail(answer.expect_err("the policy refuses the host"));
            assert!(
                detail.contains("127.0.0.1"),
                "the refusal names the host ({lists}): {detail}"
            );
            assert_eq!(hits, 0, "nothing was sent to the refused host ({lists})");
        }
    }

    /// An allowlist that names the host, and no MRF configuration at all,
    /// leave the fetch alone.
    #[tokio::test]
    async fn an_actor_of_an_allowed_host_is_fetched() {
        for lists in [Some(json!({"accept": ["127.0.0.1"]})), None] {
            let (answer, hits) = fetch_loopback_actor(lists.as_ref()).await;

            let mirrored = answer.expect("the host is not refused");
            assert_eq!(mirrored.name, "First", "the document was mirrored");
            assert!(hits > 0, "the mock was asked ({lists:?})");
        }
    }

    #[tokio::test]
    async fn fetches_a_self_identifying_document_asking_with_the_ap_accept() {
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(self_actor_checking_accept))
                .with_state(peer),
        );

        let document = fetch_actor_json(&client(), &format!("{base}/users/alice"), None)
            .await
            .unwrap();

        assert_eq!(
            document["id"],
            format!("{base}/users/alice"),
            "the document whose id names the requested URL"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one request answered");
        server.abort();
    }

    #[tokio::test]
    async fn a_410_answer_is_gone() {
        let error = fetch_failing(StatusCode::GONE).await;
        assert!(
            matches!(error, FetchError::Gone),
            "a 410 states the actor is gone: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_404_answer_is_not_found() {
        let error = fetch_failing(StatusCode::NOT_FOUND).await;
        assert!(
            matches!(error, FetchError::NotFound),
            "a 404 states the actor is missing: {error:?}"
        );
    }

    #[tokio::test]
    async fn other_error_statuses_arrive_as_http_errors() {
        let error = fetch_failing(StatusCode::INTERNAL_SERVER_ERROR).await;
        assert!(
            matches!(error, FetchError::Http(HttpError::Status(500))),
            "a plain 5xx keeps its status: {error:?}"
        );
    }

    #[tokio::test]
    async fn bodies_that_are_not_actor_json_are_invalid() {
        /// A real actor document under an HTML content type.
        async fn html_document() -> Response {
            (
                [(CONTENT_TYPE, "text/html; charset=utf-8")],
                r#"{"id":"https://test.example/users/alice"}"#,
            )
                .into_response()
        }
        /// A JSON content type over bytes that do not parse.
        async fn broken_json() -> Response {
            ([(CONTENT_TYPE, ACTOR_CONTENT_TYPE)], "{oops").into_response()
        }
        /// Parseable JSON with no string `id`.
        async fn no_id() -> Response {
            ([(CONTENT_TYPE, ACTOR_CONTENT_TYPE)], r#"{"type":"Person"}"#).into_response()
        }

        for (what, app) in [
            (
                "an HTML content type",
                Router::new().route("/actor", get(html_document)),
            ),
            (
                "a body that is not JSON",
                Router::new().route("/actor", get(broken_json)),
            ),
            (
                "an object without an id",
                Router::new().route("/actor", get(no_id)),
            ),
        ] {
            let listener = bind().await;
            let url = format!("{}/actor", origin(&listener));
            let server = spawn(listener, app);
            let error = fetch_actor_json(&client(), &url, None).await.unwrap_err();
            let detail = invalid_detail(error);
            assert!(
                !detail.is_empty(),
                "{what} reports an Invalid carrying its cause"
            );
            server.abort();
        }
    }

    /// Headers carrying `content_type`, or none at all.
    fn typed(content_type: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = content_type {
            let _previous =
                headers.insert(CONTENT_TYPE, value.parse().expect("a valid header value"));
        }
        headers
    }

    #[test]
    fn activitypub_media_types_are_accepted() {
        for value in [
            "application/activity+json",
            "application/activity+json; charset=utf-8",
            " Application/Activity+JSON ",
            r#"application/ld+json; profile="https://www.w3.org/ns/activitystreams""#,
            "application/ld+json; profile=https://www.w3.org/ns/activitystreams",
            r#"application/ld+json; charset=utf-8; profile="https://example.test/x https://www.w3.org/ns/activitystreams""#,
            r#"APPLICATION/LD+JSON; Profile="https://www.w3.org/ns/activitystreams""#,
        ] {
            assert!(
                is_activitypub_media_type(&typed(Some(value))),
                "{value} is accepted"
            );
        }
    }

    #[test]
    fn other_media_types_are_refused() {
        for value in [
            "application/ld+json",
            "application/ld+json; charset=utf-8",
            r#"application/ld+json; profile="https://example.test/other""#,
            r#"application/ld+json; profile="https://www.w3.org/ns/activitystreams-extra""#,
            "application/ld+json; profile",
            "application/json",
            "text/json",
            "application/x-json-whatever",
            "application/jrd+json",
            "text/html",
            "",
        ] {
            assert!(
                !is_activitypub_media_type(&typed(Some(value))),
                "{value:?} is refused"
            );
        }
        assert!(
            !is_activitypub_media_type(&typed(None)),
            "a missing header is refused"
        );
    }

    #[tokio::test]
    async fn an_application_json_actor_answer_is_invalid() {
        /// A well-formed, self-identifying actor under plain `application/json`.
        async fn json_actor(State(peer): State<Peer>) -> Response {
            (
                [(CONTENT_TYPE, "application/json")],
                json!({"id": format!("{}/users/alice", peer.base), "type": "Person"}).to_string(),
            )
                .into_response()
        }

        let listener = bind().await;
        let base = origin(&listener);
        let server = spawn(
            listener,
            Router::new()
                .route("/users/alice", get(json_actor))
                .with_state(peer_at(&base)),
        );

        let error = fetch_actor_json(&client(), &format!("{base}/users/alice"), None)
            .await
            .unwrap_err();

        let detail = invalid_detail(error);
        assert!(
            detail.contains("does not declare an ActivityPub content type"),
            "the plain JSON answer is refused for its type: {detail}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_body_over_the_one_mebibyte_cap_is_too_large() {
        /// A JSON document padded past [`MAX_BODY_BYTES`].
        async fn huge() -> Response {
            let padding = "x".repeat(MAX_BODY_BYTES);
            let body = format!(r#"{{"id":"https://test.example/users/alice","p":"{padding}"}}"#);
            ([(CONTENT_TYPE, ACTOR_CONTENT_TYPE)], body).into_response()
        }

        let listener = bind().await;
        let url = format!("{}/actor", origin(&listener));
        let server = spawn(listener, Router::new().route("/actor", get(huge)));

        let error = fetch_actor_json(&client(), &url, None).await.unwrap_err();

        assert!(
            matches!(error, FetchError::Http(HttpError::TooLarge)),
            "the 1 MiB cap stops an endless document: {error:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_same_host_redirect_whose_body_names_the_final_url_is_accepted() {
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let app = Router::new()
            .route("/actor", get(hop))
            .route("/users/alice", get(self_actor))
            .with_state(peer);
        let server = spawn(listener, app);

        let document = fetch_actor_json(&client(), &format!("{base}/actor"), None)
            .await
            .unwrap();

        assert_eq!(
            document["id"],
            format!("{base}/users/alice"),
            "the document names the URL that served it"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "no refetch was needed");
        server.abort();
    }

    #[tokio::test]
    async fn a_redirected_body_naming_the_original_url_is_rejected() {
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let app = Router::new()
            .route("/actor", get(hop))
            .route("/users/alice", get(redirect_target_actor))
            .with_state(peer);
        let server = spawn(listener, app);

        let error = fetch_actor_json(&client(), &format!("{base}/actor"), None)
            .await
            .unwrap_err();

        let detail = invalid_detail(error);
        assert!(
            detail.contains("redirects"),
            "the refetch of the original URL may not redirect: {detail}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "the original URL and its refetch each reached the target once, then it was refused"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_redirect_to_another_authority_is_a_cross_domain_failure() {
        let victim = bind().await;
        let attacker = bind().await;
        let victim_base = origin(&victim);
        let attacker_base = origin(&attacker);
        let forged = peer_at(&attacker_base);
        let victim_app = Router::new().route(
            "/actor",
            get(redirect_to).with_state(format!("{attacker_base}/actor")),
        );
        let attacker_app = Router::new()
            .route("/actor", get(redirect_target_actor))
            .with_state(forged);
        let victim_server = spawn(victim, victim_app);
        let attacker_server = spawn(attacker, attacker_app);

        let error = fetch_actor_json(&client(), &format!("{victim_base}/actor"), None)
            .await
            .unwrap_err();

        let detail = invalid_detail(error);
        assert!(
            detail.contains("cross-domain"),
            "a body from another host is refused: {detail}"
        );
        victim_server.abort();
        attacker_server.abort();
    }

    #[tokio::test]
    async fn a_claimed_id_that_redirects_is_rejected() {
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let app = Router::new()
            .route("/users/alice", get(moved_actor))
            .route("/moved/home", get(moved_home_actor))
            .with_state(peer)
            .route(
                "/moved/alice",
                get(redirect_to).with_state(format!("{base}/moved/home")),
            );
        let server = spawn(listener, app);

        let error = fetch_actor_json(&client(), &format!("{base}/users/alice"), None)
            .await
            .unwrap_err();

        let detail = invalid_detail(error);
        assert!(
            detail.contains("redirects"),
            "the refetch of the claimed id may not redirect: {detail}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "the moved document and the redirected target were read, then refused"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_moved_document_is_refetched_once_and_accepted() {
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let app = Router::new()
            .route("/users/alice", get(moved_actor))
            .with_state(peer.clone())
            .merge(
                Router::new()
                    .route("/moved/alice", get(moved_home_actor))
                    .with_state(peer),
            );
        let server = spawn(listener, app);

        let document = fetch_actor_json(&client(), &format!("{base}/users/alice"), None)
            .await
            .unwrap();

        assert_eq!(
            document["id"],
            format!("{base}/moved/alice"),
            "the document is re-anchored to the id it claims"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "the original URL and the claim each answered once"
        );
        server.abort();
    }

    /// The actor body cap under `cross_host`.
    const fn rules(cross_host: CrossHost) -> ContainmentRules {
        ContainmentRules {
            max_bytes: MAX_BODY_BYTES,
            cross_host,
        }
    }

    /// Two peers on separate loopback ports: `home` serves `/users/alice`
    /// claiming `/moved/alice` on `other`, which serves that document
    /// naming itself. Returns the servers, `home`'s URL for the document
    /// and the hit counters of `home` and `other`.
    async fn cross_authority_claim() -> (
        [JoinHandle<()>; 2],
        String,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let home = bind().await;
        let other = bind().await;
        let home_base = origin(&home);
        let other_base = origin(&other);
        let home_peer = peer_at(&other_base);
        let other_peer = peer_at(&other_base);
        let home_hits = Arc::clone(&home_peer.hits);
        let other_hits = Arc::clone(&other_peer.hits);
        let home_server = spawn(
            home,
            Router::new()
                .route("/users/alice", get(moved_actor))
                .with_state(home_peer),
        );
        let other_server = spawn(
            other,
            Router::new()
                .route("/moved/alice", get(moved_home_actor))
                .with_state(other_peer),
        );
        (
            [home_server, other_server],
            format!("{home_base}/users/alice"),
            home_hits,
            other_hits,
        )
    }

    #[tokio::test]
    async fn deny_rejects_a_cross_authority_claim_without_a_second_request() {
        let (servers, url, home_hits, other_hits) = cross_authority_claim().await;
        let mut asked = Vec::new();

        let error =
            fetch_contained_document(&client(), &url, None, rules(CrossHost::Deny), |target| {
                asked.push(target.to_owned());
                Ok(())
            })
            .await
            .unwrap_err();

        let detail = invalid_detail(error);
        assert!(
            detail.contains("different origin"),
            "the foreign claim is refused on the spot: {detail}"
        );
        assert_eq!(home_hits.load(Ordering::SeqCst), 1, "one request to home");
        assert_eq!(
            other_hits.load(Ordering::SeqCst),
            0,
            "the claim is not asked"
        );
        assert_eq!(asked, [url], "only the first GET reached before_fetch");
        for server in servers {
            server.abort();
        }
    }

    #[tokio::test]
    async fn allow_follows_a_cross_authority_claim_once() {
        let (servers, url, home_hits, other_hits) = cross_authority_claim().await;
        let mut asked = Vec::new();

        let document =
            fetch_contained_document(&client(), &url, None, rules(CrossHost::Allow), |target| {
                asked.push(target.to_owned());
                Ok(())
            })
            .await
            .unwrap();

        assert!(
            document["id"]
                .as_str()
                .is_some_and(|id| id.ends_with("/moved/alice")),
            "the document is re-anchored to the claimed id"
        );
        assert_eq!(home_hits.load(Ordering::SeqCst), 1, "one request to home");
        assert_eq!(other_hits.load(Ordering::SeqCst), 1, "the claim asked once");
        assert_eq!(asked.len(), 2, "before_fetch ran ahead of both GETs");
        for server in servers {
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_refusing_before_fetch_stops_the_refetch() {
        let (servers, url, home_hits, other_hits) = cross_authority_claim().await;
        let mut calls = 0_usize;

        let error =
            fetch_contained_document(&client(), &url, None, rules(CrossHost::Allow), |target| {
                calls += 1;
                (calls == 1)
                    .then_some(())
                    .ok_or_else(|| FetchError::Invalid(format!("budget spent at `{target}`")))
            })
            .await
            .unwrap_err();

        let detail = invalid_detail(error);
        assert!(
            detail.contains("budget spent"),
            "the refusal comes back as returned: {detail}"
        );
        assert_eq!(home_hits.load(Ordering::SeqCst), 1, "the first GET ran");
        assert_eq!(other_hits.load(Ordering::SeqCst), 0, "the second did not");
        for server in servers {
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_document_that_keeps_moving_is_rejected() {
        let listener = bind().await;
        let base = origin(&listener);
        let peer = peer_at(&base);
        let hits = Arc::clone(&peer.hits);
        let app = Router::new()
            .route("/users/alice", get(moved_actor))
            .with_state(peer.clone())
            .merge(
                Router::new()
                    .route("/moved/alice", get(moving_on_actor))
                    .with_state(peer),
            );
        let server = spawn(listener, app);

        let error = fetch_actor_json(&client(), &format!("{base}/users/alice"), None)
            .await
            .unwrap_err();

        match error {
            FetchError::Invalid(detail) => assert!(
                detail.contains("elsewhere"),
                "the rejection names where the id moved on to: {detail}"
            ),
            FetchError::Http(_)
            | FetchError::Gone
            | FetchError::NotFound
            | FetchError::AcctMismatch
            | FetchError::Webfinger(_) => {
                panic!("a moving id must be rejected as Invalid, got {error:?}")
            }
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "exactly one refetch: the third id is never asked for"
        );
        server.abort();
    }
}
