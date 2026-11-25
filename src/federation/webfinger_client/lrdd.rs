//! The retry behind a failed first `WebFinger`: which failures justify it,
//! the host-meta document that advertises an LRDD template, and the URL that
//! template expands to for a resource.

use crate::http_client::{HttpClient, HttpError, HttpResponse};

use super::{
    JRD_JSON, LRDD_REL, URI_MACRO, URI_MACRO_ENCODED, WebfingerError, XRD_XML,
    document::FingerLink, parse::parse_xrd, resolve::fetch,
};

/// The second path of a failed first `WebFinger`: ask host-meta for an LRDD
/// template, fetch through it, and keep the original error when host-meta
/// has nothing usable.
pub(super) async fn fall_back_to_lrdd(
    http: &HttpClient,
    first_error: WebfingerError,
    base: &str,
    resource: &str,
) -> Result<HttpResponse, WebfingerError> {
    let Some(template_url) = lrdd_url_from_host_meta(http, base, resource).await else {
        return Err(first_error);
    };
    fetch(http, &template_url, JRD_JSON).await
}

/// The first-attempt failures that justify consulting host-meta (the ticket
/// list: 404, 410, and network trouble). Every other failure — other
/// statuses, a too-large body, a blocked or unparsable target — is the
/// answer already.
pub(super) const fn should_fall_back(error: &WebfingerError) -> bool {
    matches!(
        error,
        WebfingerError::Http(
            HttpError::Status(404 | 410) | HttpError::Connect(_) | HttpError::Timeout
        )
    )
}

/// The URL the `{base}/.well-known/host-meta` LRDD template expands to for
/// `resource`, [`None`] when host-meta is unreachable, unparseable, or
/// advertises no usable `rel="lrdd"` template.
async fn lrdd_url_from_host_meta(http: &HttpClient, base: &str, resource: &str) -> Option<String> {
    let url = format!("{base}/.well-known/host-meta");
    let response = fetch(http, &url, XRD_XML).await.ok()?;
    let body = String::from_utf8(response.body.to_vec()).ok()?;
    let document = parse_xrd(&body).ok()?;
    lrdd_url(&document.links, resource)
}

/// The URL an `lrdd` link's `template` names for `resource`, expanding
/// either `{uri}` spelling. A template without a macro cannot carry the
/// resource, so it is not used.
fn lrdd_url(links: &[FingerLink], resource: &str) -> Option<String> {
    links.iter().find_map(|link| {
        if link.rel.as_deref() != Some(LRDD_REL) {
            return None;
        }
        let template = link.template.as_deref()?;
        let expanded = if template.contains(URI_MACRO) {
            template.replace(URI_MACRO, resource)
        } else {
            template.replace(URI_MACRO_ENCODED, resource)
        };
        if expanded == template {
            return None;
        }
        Some(expanded)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::http::StatusCode;
    use serde_json::json;
    use tokio::net::TcpSocket;

    use super::*;
    use crate::federation::webfinger_client::{
        finger_with_base, resolve_acct_with_base, test_support::*,
    };

    #[test]
    fn only_404_410_and_network_failures_fall_back() {
        for (error, fallback) in [
            (HttpError::Status(404), true),
            (HttpError::Status(410), true),
            (HttpError::Status(500), false),
            (HttpError::Status(422), false),
            (HttpError::Connect("refused".to_owned()), true),
            (HttpError::Timeout, true),
            (HttpError::TooLarge, false),
            (HttpError::Blocked("loopback".to_owned()), false),
            (HttpError::InvalidUrl, false),
        ] {
            let error = WebfingerError::Http(error);
            assert_eq!(should_fall_back(&error), fallback, "{error}");
        }
        assert!(
            !should_fall_back(&WebfingerError::Malformed),
            "only HTTP failures fall back"
        );
    }

    #[test]
    fn lrdd_needs_its_macro_and_its_rel() {
        let with_template = |rel: Option<&str>, template: Option<&str>| FingerLink {
            rel: rel.map(str::to_owned),
            template: template.map(str::to_owned),
            ..FingerLink::default()
        };
        assert_eq!(
            lrdd_url(
                &[with_template(
                    Some(LRDD_REL),
                    Some("https://x.example/wf?r={uri}")
                )],
                "acct:a@x.example",
            )
            .unwrap(),
            "https://x.example/wf?r=acct:a@x.example",
            "the macro expands"
        );
        for links in [
            vec![with_template(
                Some("other"),
                Some("https://x.example/wf?r={uri}"),
            )],
            vec![with_template(Some(LRDD_REL), Some("https://x.example/wf"))],
            vec![with_template(Some(LRDD_REL), None)],
            vec![],
        ] {
            assert_eq!(
                lrdd_url(&links, "acct:a@x.example"),
                None,
                "no usable lrdd link"
            );
        }
    }

    #[tokio::test]
    async fn falls_back_to_the_host_meta_lrdd_template() {
        let app = status_route("/.well-known/webfinger", StatusCode::NOT_FOUND)
            .merge(host_meta_route(false))
            .merge(static_route("/lrdd", Some(JRD_JSON), JRD_ALICE));
        assert_actor(resolve_against(app, ACCT).await, ACTOR);
    }

    #[tokio::test]
    async fn the_percent_encoded_uri_macro_is_expanded_too() {
        let app = status_route("/.well-known/webfinger", StatusCode::GONE)
            .merge(host_meta_route(true))
            .merge(static_route("/lrdd", Some(JRD_JSON), JRD_ALICE));
        assert_actor(resolve_against(app, ACCT).await, ACTOR);
    }

    #[tokio::test]
    async fn finger_falls_back_to_the_host_meta_template_too() {
        let app = status_route("/.well-known/webfinger", StatusCode::NOT_FOUND)
            .merge(host_meta_route(false))
            .merge(static_route("/lrdd", Some(JRD_JSON), JRD_ALICE));
        let (server, base) = serve(app).await;
        let document = finger_with_base(&client(), ACCT, Some(&base))
            .await
            .expect("the template answer is a full document");
        server.abort();
        assert_eq!(document["subject"], json!("acct:alice@remote.example"));
    }

    #[tokio::test]
    async fn a_successful_first_attempt_never_consults_host_meta() {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = static_route("/.well-known/webfinger", Some(JRD_JSON), JRD_ALICE)
            .merge(counting_failure_route("/.well-known/host-meta", &hits));
        assert_actor(resolve_against(app, ACCT).await, ACTOR);
        assert_eq!(hits.load(Ordering::SeqCst), 0, "host-meta was never asked");
    }

    #[tokio::test]
    async fn a_non_fallback_status_propagates_without_consulting_host_meta() {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = status_route("/.well-known/webfinger", StatusCode::INTERNAL_SERVER_ERROR)
            .merge(counting_failure_route("/.well-known/host-meta", &hits));
        let error = resolve_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::Http(HttpError::Status(500))),
            "a 500 is the answer, not a prompt for host-meta: {error:?}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0, "host-meta was never asked");
    }

    #[tokio::test]
    async fn network_failure_reports_the_first_attempt_error_when_host_meta_fails() {
        // A port bound but never listened on: both the WebFinger attempt and
        // the host-meta fallback are refused, and the first error survives.
        // The socket stays bound for the whole test so no concurrently
        // running test's server can take the port (binding a listener and
        // dropping it let another test's WebFinger mock answer here).
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind(([127, 0, 0, 1], 0).into()).unwrap();
        let dead_base = format!("http://{}", socket.local_addr().unwrap());
        let error = resolve_acct_with_base(&client(), ACCT, Some(&dead_base))
            .await
            .unwrap_err();
        drop(socket);
        assert!(
            matches!(error, WebfingerError::Http(HttpError::Connect(_))),
            "a refused connection is reported: {error:?}"
        );
    }

    #[tokio::test]
    async fn an_unusable_host_meta_keeps_the_original_error() {
        let app =
            status_route("/.well-known/webfinger", StatusCode::NOT_FOUND).merge(static_route(
                "/.well-known/host-meta",
                Some(XRD_XML),
                format!(r#"<XRD xmlns="{XRD_NAMESPACE}"><Link rel="self" href="{ACTOR}"/></XRD>"#),
            ));
        let error = resolve_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::Http(HttpError::Status(404))),
            "host-meta without an lrdd template: the 404 stands: {error:?}"
        );
    }

    #[tokio::test]
    async fn the_templated_attempt_reports_its_own_failures() {
        // The templated fetch answers 410: that status, not the original
        // 404, is what the caller sees — proof the template URL was fetched.
        let app = status_route("/.well-known/webfinger", StatusCode::NOT_FOUND)
            .merge(host_meta_route(false))
            .merge(status_route("/lrdd", StatusCode::GONE));
        let error = resolve_against(app, ACCT).await.unwrap_err();
        assert!(
            matches!(error, WebfingerError::Http(HttpError::Status(410))),
            "the template fetch failed: {error:?}"
        );
    }
}
