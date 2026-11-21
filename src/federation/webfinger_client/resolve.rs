//! The lookup: the `WebFinger` request a handle turns into and the actor URL
//! its answer names.
//!
//! The domain-containment rule the answer has to pass is checked here too;
//! the host-meta retry that stands behind a failed first attempt lives in
//! `lrdd`.

use reqwest::header::ACCEPT;
use serde_json::Value;
use url::Url;

use crate::http_client::{HttpClient, HttpResponse};

use super::{
    ACCT_PREFIX, JRD_JSON, MAX_BODY_BYTES, WebfingerError,
    acct::{split_acct, strip_prefix_ignore_case},
    lrdd::{fall_back_to_lrdd, should_fall_back},
    parse::{full_jrd, parse_finger_response},
};

/// Resolves a remote handle to its `ActivityPub` actor URL.
///
/// `acct` names the account as `nick@domain`, optionally prefixed with `@`
/// and/or `acct:` (both are stripped before validation). The document rules
/// are the ones in the [`WebFinger`](crate::federation::webfinger_client)
/// module's docs; the returned URL is the self link's `href` verbatim.
///
/// The answer must belong to the requested domain: either its `subject` (if
/// present) names that domain, or the actor URL's host equals it. Mastodon
/// enforces this harder with a full round trip: it re-resolves the returned
/// `acct:` subject and requires the second answer to lead back to the same
/// actor, because a `WebFinger` host could otherwise claim an account on any
/// other host and get the requester to talk to (and trust) it. This client
/// enforces the containment the ticket fixes — subject domain OR actor host
/// equals the requested domain, mirroring the check Pleroma's
/// `WebFinger.validate_webfinger/2` puts first — and a later fetch ticket
/// that dereferences the actor URL with its own origin checks completes the
/// round trip. Anything outside that rule fails as
/// [`WebfingerError::DomainMismatch`].
///
/// # Errors
///
/// [`WebfingerError::InvalidAcct`] for an unusable handle (no request is
/// made), [`WebfingerError::Http`] for transport, status and body-size
/// failures of the `WebFinger` and template fetches,
/// [`WebfingerError::Malformed`] for a body that is neither valid JRD nor
/// valid XRD, [`WebfingerError::NoActorLink`] when it names no actor, and
/// [`WebfingerError::DomainMismatch`] when it does not belong to the
/// requested domain.
pub async fn resolve_acct(http: &HttpClient, acct: &str) -> Result<String, WebfingerError> {
    resolve_acct_with_base(http, acct, None).await
}

/// [`resolve_acct`] with `scheme_and_authority_override` replacing the
/// spec-fixed `https://{domain}` base of both the `WebFinger` and the
/// host-meta URL. `WebFinger` is HTTPS-only by RFC 7033, but the tests
/// serve their mocks over plain HTTP on loopback, so they go through here;
/// production resolves with [`None`] and speaks the spec. Crate-visible
/// for the same reason: [`crate::federation::fetch_actor`]'s account tests
/// drive the full resolve-then-fetch round trip against loopback mocks.
pub(crate) async fn resolve_acct_with_base(
    http: &HttpClient,
    acct: &str,
    scheme_and_authority_override: Option<&str>,
) -> Result<String, WebfingerError> {
    let (response, domain) = fetch_answer(http, acct, scheme_and_authority_override).await?;
    let document = parse_finger_response(&response)?;
    let actor = document.actor_href().ok_or(WebfingerError::NoActorLink)?;
    verify_round_trip(document.subject.as_deref(), actor, &domain)?;
    Ok(actor.to_owned())
}

/// Resolves a remote handle through `WebFinger` and answers the whole
/// document as a JRD JSON object.
///
/// The lookup is [`resolve_acct`]'s — same URLs, same host-meta fallback,
/// same format sniffing — but instead of extracting the actor URL it
/// returns the document itself: a JSON answer verbatim (unknown members
/// like `aliases` and `properties` included), an XRD answer normalized
/// into `{subject?, links[]}` with every `<Link>` element's `rel`, `type`,
/// `href` and `template` attributes. Callers that read anything beyond the
/// actor link — the subscribe-template lookup of
/// [`crate::api::remote_interaction`], for instance — go through here.
/// No domain-containment check is applied: the document is returned as
/// answered, and a later ticket that consumes an actor URL from it reuses
/// [`resolve_acct`], which does check.
///
/// [`resolve_acct`]: resolve_acct
///
/// # Errors
///
/// The same failures as [`resolve_acct`]'s lookup steps:
/// [`WebfingerError::InvalidAcct`] for an unusable handle (no request is
/// made), [`WebfingerError::Http`] for transport, status and body-size
/// failures, [`WebfingerError::Malformed`] for a body that is neither
/// valid JRD (a JSON object) nor valid XRD.
pub(crate) async fn finger_with_base(
    http: &HttpClient,
    acct: &str,
    scheme_and_authority_override: Option<&str>,
) -> Result<Value, WebfingerError> {
    let (response, _domain) = fetch_answer(http, acct, scheme_and_authority_override).await?;
    full_jrd(&response)
}

/// The shared half of both reads: the `WebFinger` answer for `acct`, plus
/// the (lowercased) domain it was looked up on. Every failure — an
/// unusable handle, a transport or status failure of the direct attempt or
/// of the host-meta fallback — stops the lookup before any parsing.
async fn fetch_answer(
    http: &HttpClient,
    acct: &str,
    scheme_and_authority_override: Option<&str>,
) -> Result<(HttpResponse, String), WebfingerError> {
    let (nickname, domain) = split_acct(acct).ok_or(WebfingerError::InvalidAcct)?;
    let base = scheme_and_authority_override.map_or_else(|| default_base(&domain), str::to_owned);
    let resource = format!("acct:{nickname}@{domain}");
    let response = match fetch(http, &finger_url(&base, &resource), JRD_JSON).await {
        Ok(response) => response,
        Err(first_error) if should_fall_back(&first_error) => {
            fall_back_to_lrdd(http, first_error, &base, &resource).await?
        }
        Err(first_error) => return Err(first_error),
    };
    Ok((response, domain))
}

/// The base the spec fixes for a domain: `https://{domain}` — never a port,
/// which [`split_acct`] rules out of the domain it accepts.
fn default_base(domain: &str) -> String {
    format!("https://{domain}")
}

/// The `{base}/.well-known/webfinger?resource={resource}` URL; every
/// character of `resource` is from the alphabets [`split_acct`] enforces,
/// so none of it needs percent-encoding.
fn finger_url(base: &str, resource: &str) -> String {
    format!("{base}/.well-known/webfinger?resource={resource}")
}

/// `GET`s `url` asking for `accept`, capped at [`MAX_BODY_BYTES`], with any
/// 4xx/5xx converted to [`WebfingerError::Http`].
pub(super) async fn fetch(
    http: &HttpClient,
    url: &str,
    accept: &str,
) -> Result<HttpResponse, WebfingerError> {
    http.get_limited(url, &[(ACCEPT.as_str(), accept)], MAX_BODY_BYTES)
        .await?
        .error_for_status()
        .map_err(WebfingerError::from)
}

/// The actor URL rule of a `WebFinger` answer: its `subject` (an `acct:`
/// URI) names `domain`, or the actor URL's own host equals `domain` — the
/// OR the ticket fixes, with [`resolve_acct`]'s doc comment for why (the
/// Mastodon round trip).
fn verify_round_trip(
    subject: Option<&str>,
    actor: &str,
    domain: &str,
) -> Result<(), WebfingerError> {
    if subject_names_domain(subject, domain) || actor_names_domain(actor, domain) {
        Ok(())
    } else {
        Err(WebfingerError::DomainMismatch)
    }
}

/// Whether `subject` carries an `@domain` part equal to `domain`
/// (case-insensitively); an absent subject, or one without an `@`, names
/// nothing.
fn subject_names_domain(subject: Option<&str>, domain: &str) -> bool {
    subject.is_some_and(|subject| {
        let value = strip_prefix_ignore_case(subject, ACCT_PREFIX).unwrap_or(subject);
        value
            .rsplit_once('@')
            .is_some_and(|(_name, host)| host.eq_ignore_ascii_case(domain))
    })
}

/// Whether `actor` parses as a URL whose host (with its port, when it names
/// one) equals `domain` (case-insensitively; URL parsing already lowercases
/// hosts). A requested domain carrying a port only matches an actor URL on
/// the same host and port — the containment a `host:port` handle asks for.
fn actor_names_domain(actor: &str, domain: &str) -> bool {
    Url::parse(actor).is_ok_and(|url| {
        let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
        !authority.is_empty() && authority.eq_ignore_ascii_case(domain)
    })
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        http::{HeaderMap, StatusCode},
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use reqwest::header::CONTENT_TYPE;
    use serde_json::json;

    use super::*;
    use crate::{
        federation::webfinger_client::{XRD_XML, test_support::*},
        http_client::HttpError,
    };

    #[test]
    fn the_spec_url_is_https_with_the_acct_resource() {
        assert_eq!(
            default_base(DOMAIN),
            "https://remote.example",
            "the production base is the spec's https origin"
        );
        assert_eq!(
            finger_url(&default_base(DOMAIN), "acct:alice@remote.example"),
            "https://remote.example/.well-known/webfinger?resource=acct:alice@remote.example"
        );
    }

    /// Serve [`JRD_ALICE`] only when the request asked for exactly
    /// `application/jrd+json`, and 400 otherwise — so a success also proves
    /// the client's `Accept` header and the request path.
    async fn jrd_checking_accept(headers: HeaderMap) -> Response {
        let asked = headers.get(ACCEPT).and_then(|value| value.to_str().ok()) == Some(JRD_JSON);
        if !asked {
            return StatusCode::BAD_REQUEST.into_response();
        }
        ([(CONTENT_TYPE, JRD_JSON)], JRD_ALICE).into_response()
    }

    #[tokio::test]
    async fn resolves_a_jrd_answer() {
        let app = Router::new().route("/.well-known/webfinger", get(jrd_checking_accept));
        let result = resolve_against(app, "@acct:alice@remote.example").await;
        assert_actor(result, ACTOR);
    }

    #[tokio::test]
    async fn resolves_an_xrd_answer() {
        let app = static_route("/.well-known/webfinger", Some(XRD_XML), XRD_ALICE);
        assert_actor(resolve_against(app, ACCT).await, ACTOR);
    }

    #[tokio::test]
    async fn finger_fails_an_unusable_handle_before_any_request() {
        let error = finger_with_base(&client(), "alice", None)
            .await
            .unwrap_err();
        assert!(
            matches!(error, WebfingerError::InvalidAcct),
            "no handle, no request: {error:?}"
        );
    }

    #[tokio::test]
    async fn invalid_handles_fail_before_any_request() {
        for acct in [
            "alice",
            "@",
            "alice@localhost",
            "alice@remote.example:70000",
        ] {
            let error = resolve_acct_with_base(&client(), acct, None)
                .await
                .unwrap_err();
            assert!(
                matches!(error, WebfingerError::InvalidAcct),
                "{acct} is no handle, and no DNS is tried: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn domain_containment_accepts_either_subject_or_actor_host() {
        for (body, expect) in [
            (JRD_FOREIGN_SUBJECT_LOCAL_ACTOR, ACTOR),
            (
                JRD_LOCAL_SUBJECT_FOREIGN_ACTOR,
                "https://webfinger.example/users/alice",
            ),
        ] {
            let app = static_route("/.well-known/webfinger", Some(JRD_JSON), body);
            assert_actor(resolve_against(app, ACCT).await, expect);
        }
    }

    #[tokio::test]
    async fn domain_containment_rejects_answers_naming_nothing_local() {
        for body in [JRD_EVIL, JRD_NO_SUBJECT_FOREIGN_ACTOR] {
            let app = static_route("/.well-known/webfinger", Some(JRD_JSON), body);
            let error = resolve_against(app, ACCT).await.unwrap_err();
            assert!(
                matches!(error, WebfingerError::DomainMismatch),
                "neither subject nor actor host is {DOMAIN}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_port_and_an_ip_literal_are_resolvable_handles() {
        // A peer on `https://{host}:{port}` is addressable exactly like a
        // plain domain: the containment rule matches the actor URL's
        // authority against the requested domain, port included.
        let body = json!({
            "subject": "acct:alice@remote.example:8080",
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": "https://remote.example:8080/users/alice",
            }],
        })
        .to_string();
        let app = static_route("/.well-known/webfinger", Some(JRD_JSON), body);
        assert_actor(
            resolve_against(app, "alice@remote.example:8080").await,
            "https://remote.example:8080/users/alice",
        );
    }

    #[tokio::test]
    async fn a_foreign_port_actor_does_not_contain_a_portless_domain() {
        // Subject and actor both name `remote.example:8080`, but the request
        // is for plain `remote.example`: the authority differs, so this is
        // the mismatched-domain refusal, not a pass on the bare host.
        let body = json!({
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": "https://remote.example:8080/users/alice",
            }],
        })
        .to_string();
        let app = static_route("/.well-known/webfinger", Some(JRD_JSON), body);
        let error = resolve_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::DomainMismatch),
            "the port is part of the authority: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_too_large() {
        let huge = format!(
            r#"{{"subject": "acct:alice@remote.example", "links": [{{"rel": "self", "type": "application/activity+json", "href": "https://remote.example/users/alice", "note": "{}"}}]}}"#,
            "x".repeat(MAX_BODY_BYTES),
        );
        let app = static_route("/.well-known/webfinger", Some(JRD_JSON), huge);
        let error = resolve_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::Http(HttpError::TooLarge)),
            "the 256 KiB cap stops an endless body: {error:?}"
        );
    }
}
