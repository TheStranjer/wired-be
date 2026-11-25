//! The interaction endpoint: `POST /api/v1/pleroma/remote_interaction`.
//! The subscribe lookup, its `WebFinger` template reading and macro
//! expansion, and the parameter trimming the request is read through.

use axum::{Json, extract::State};
use serde_json::Value;
use tracing::debug;
use url::form_urlencoded;

use crate::{
    error::{AppError, AppResult},
    extract::ApiBody,
    federation::webfinger_client::{URI_MACRO, URI_MACRO_ENCODED, finger_with_base, split_acct},
    http_client::HttpClient,
    state::AppState,
};

use super::{
    ALLOWED_PORTS, COULD_NOT_FIND_USER, InteractionParams, InteractionReply, MISSING_AP_ID,
    MISSING_PROFILE, SUBSCRIBE_REL,
};

/// `POST /api/v1/pleroma/remote_interaction` (Pleroma's
/// `RemoteInteractionController.remote_interaction`): the JSON answer of
/// the subscribe-template lookup.
///
/// # Errors
///
/// [`AppError::BadRequest`] with [`MISSING_AP_ID`] or [`MISSING_PROFILE`]
/// for an absent or blank parameter, and with [`COULD_NOT_FIND_USER`]
/// when `profile` resolves through `WebFinger` to no usable subscribe
/// template, and likewise when `profile` names a port other than 80 or
/// 443 (see [`port_allowed`]).
pub(super) async fn remote_interaction(
    State(state): State<AppState>,
    ApiBody(params): ApiBody<InteractionParams>,
) -> AppResult<Json<InteractionReply>> {
    interaction_answer(&state, params, None).await
}

/// The `POST /api/v1/pleroma/remote_interaction` decision, extracted so
/// the loopback-mock tests can point `webfinger_base` at their `WebFinger`
/// answer (the seam of [`crate::api::search`]' resolver and
/// [`crate::federation::webfinger_client`]'s reader). Production reaches
/// it only through [`remote_interaction`], which passes no override.
pub(super) async fn interaction_answer(
    state: &AppState,
    params: InteractionParams,
    webfinger_base: Option<&str>,
) -> AppResult<Json<InteractionReply>> {
    let Some(ap_id) = required(params.ap_id.as_deref()) else {
        return Err(AppError::BadRequest(MISSING_AP_ID.to_owned()));
    };
    let Some(profile) = required(params.profile.as_deref()) else {
        return Err(AppError::BadRequest(MISSING_PROFILE.to_owned()));
    };
    if !port_allowed(profile, state.config.fetch.http_allow_private_hosts) {
        debug!(profile, "remote interaction refused a handle naming a port");
        return Err(AppError::BadRequest(COULD_NOT_FIND_USER.to_owned()));
    }
    let url = subscribe_url_for(&state.http_client, ap_id, profile, webfinger_base)
        .await
        .ok_or_else(|| AppError::BadRequest(COULD_NOT_FIND_USER.to_owned()))?;
    Ok(Json(InteractionReply { url }))
}

/// Whether a user-triggered lookup may finger `profile`: a handle naming no
/// port, or the standard `80` or `443`, always; any other port only with
/// `allow_ported` (`HTTP_ALLOW_PRIVATE_HOSTS`, the development and test
/// switch), so an anonymous caller cannot aim this server at arbitrary
/// `host:port` pairs. An unusable handle passes: the lookup refuses it
/// before any request.
fn port_allowed(profile: &str, allow_ported: bool) -> bool {
    if allow_ported {
        return true;
    }
    let Some((_nickname, domain)) = split_acct(profile) else {
        return true;
    };
    domain
        .rsplit_once(':')
        .is_none_or(|(_host, port)| ALLOWED_PORTS.contains(&port))
}

/// The whole subscribe lookup of the JSON endpoint: `WebFinger`s `profile` and expands `target` into
/// its subscribe template. `webfinger_base` replaces the spec-fixed
/// `https://{domain}` origin of the lookup — [`None`] in production, a
/// loopback mock's origin in the tests (the override
/// [`crate::federation::webfinger_client`] gives its resolver). Failures
/// are logged, never rendered.
///
/// [`None`] for an unresolvable profile, a document with no
/// `rel=http://ostatus.org/schema/1.0/subscribe` link, and one whose
/// template carries no macro to expand.
pub(super) async fn subscribe_url_for(
    http: &HttpClient,
    target: &str,
    profile: &str,
    webfinger_base: Option<&str>,
) -> Option<String> {
    match finger_with_base(http, profile, webfinger_base).await {
        Ok(document) => expand_subscribe_url(&document, target),
        Err(err) => {
            debug!(%err, profile, "webfinger found no answer for the subscribe profile");
            None
        }
    }
}

/// The subscribe link's template of a `WebFinger` document: the
/// `template` of the first `links` entry whose `rel` (trimmed) names
/// [`SUBSCRIBE_REL`] and that carries a non-blank template. A `rel` or
/// `template` that is not a string is as absent as a blank one.
fn subscribe_template(document: &Value) -> Option<&str> {
    document
        .get("links")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|link| {
            let rel = link.get("rel").and_then(Value::as_str)?.trim();
            if rel != SUBSCRIBE_REL {
                return None;
            }
            let template = link.get("template").and_then(Value::as_str)?.trim();
            (!template.is_empty()).then_some(template)
        })
}

/// The subscribe URL: the document's template with `target`
/// percent-encoded into its macro. Both spellings the `WebFinger` client
/// accepts for the LRDD macro are expanded (`{uri}` and `%7Buri%7D`); a
/// template with neither cannot carry the target, and is no URL. The
/// encoding is [`url::form_urlencoded`]'s serializer: everything outside
/// `[A-Za-z0-9*-._]` becomes its `%XX` UTF-8 bytes, so a full `ap_id`
/// URL survives being quoted into another URL's query value intact.
pub(super) fn expand_subscribe_url(document: &Value, target: &str) -> Option<String> {
    let template = subscribe_template(document)?;
    let encoded: String = form_urlencoded::byte_serialize(target.as_bytes()).collect();
    if template.contains(URI_MACRO) {
        return Some(template.replace(URI_MACRO, &encoded));
    }
    if template.contains(URI_MACRO_ENCODED) {
        return Some(template.replace(URI_MACRO_ENCODED, &encoded));
    }
    None
}

/// `value` trimmed, or the empty string when the request carried none.
fn trimmed(value: Option<&str>) -> &str {
    value.map_or("", str::trim)
}

/// The parameter `value` when the request carried it non-blank after
/// trimming — the presence rule of every required parameter here.
fn required(value: Option<&str>) -> Option<&str> {
    let text = trimmed(value);
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        http::{StatusCode, header},
        response::IntoResponse as _,
    };
    use serde_json::json;

    use super::*;
    use crate::{
        api::remote_interaction::test_support::{
            AP_ID, AP_ID_ENCODED, JRD_ENCODED_MACRO, JRD_NO_MACRO, JRD_NO_SUBSCRIBE,
            JRD_NO_TEMPLATE, JRD_SUBSCRIBE, PROFILE, WANT_URL, XRD_SUBSCRIBE, body_of, client,
            client_state, serve, status_route, url_against, webfinger_route,
        },
        config::Config,
        federation::webfinger_client::{JRD_JSON, XRD_XML},
    };

    #[test]
    fn required_parameters_are_present_and_non_blank() {
        assert_eq!(required(None), None, "an absent parameter");
        assert_eq!(required(Some("")), None, "an empty one");
        assert_eq!(required(Some("   ")), None, "a blank one");
        assert_eq!(
            required(Some(" https://x.example/a ")),
            Some("https://x.example/a"),
            "a value is trimmed"
        );
        assert_eq!(trimmed(None), "", "and an absent value reads as empty");
        assert_eq!(
            trimmed(Some(" https://x.example/a ")),
            "https://x.example/a"
        );
    }

    #[tokio::test]
    async fn the_json_endpoint_answers_the_interaction_url() {
        let (server, base) = serve(webfinger_route(JRD_JSON, JRD_SUBSCRIBE)).await;
        let state = client_state();
        let params = InteractionParams {
            ap_id: Some(AP_ID.to_owned()),
            profile: Some(PROFILE.to_owned()),
        };
        let reply = interaction_answer(&state, params, Some(&base))
            .await
            .expect("the mock answers a subscribe template");
        server.abort();
        let response = reply.into_response();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the interaction answer is a 200"
        );
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json",
            "and it is JSON"
        );
        assert_eq!(
            body_of(response).await,
            format!(r#"{{"url":"{WANT_URL}"}}"#),
            "the exact body the ticket fixes"
        );
    }

    #[tokio::test]
    async fn the_json_endpoint_refuses_without_the_network() {
        let state = client_state();
        for (params, why, want) in [
            (
                InteractionParams {
                    ap_id: None,
                    profile: Some(PROFILE.to_owned()),
                },
                "an absent ap_id",
                MISSING_AP_ID,
            ),
            (
                InteractionParams {
                    ap_id: Some("   ".to_owned()),
                    profile: Some(PROFILE.to_owned()),
                },
                "a blank ap_id",
                MISSING_AP_ID,
            ),
            (
                InteractionParams {
                    ap_id: Some(AP_ID.to_owned()),
                    profile: None,
                },
                "an absent profile",
                MISSING_PROFILE,
            ),
            (
                InteractionParams {
                    ap_id: Some(AP_ID.to_owned()),
                    profile: Some("bob@invalid".to_owned()),
                },
                "an unusable profile (no request ever leaves)",
                COULD_NOT_FIND_USER,
            ),
        ] {
            let err = interaction_answer(&state, params, None)
                .await
                .expect_err(&format!("{why} must be refused"));
            assert!(matches!(err, AppError::BadRequest(_)), "{why} is a 400");
            assert_eq!(err.to_string(), want, "{why}: the exact message");
        }
    }

    /// A state whose SSRF guard is on, as production runs: ported
    /// handles are refused. No request leaves it.
    fn guarded_state() -> AppState {
        let mut config = Config::for_tests();
        config.fetch.http_allow_private_hosts = false;
        AppState {
            config: Arc::new(config),
            ..client_state()
        }
    }

    #[test]
    fn only_standard_ports_pass_without_the_development_switch() {
        for (profile, allowed, why) in [
            ("bob@remote.example", true, "no port"),
            ("bob@remote.example:443", true, "the https port"),
            ("@bob@remote.example:80", true, "the http port"),
            ("acct:bob@Remote.Example:443", true, "a prefixed handle"),
            ("bob@remote.example:8080", false, "an alternate web port"),
            ("bob@remote.example:22", false, "a port that is no web port"),
            ("bob@203.0.113.9:6379", false, "an IP with a service port"),
            ("bob@remote.example:0443", false, "a padded port"),
            (
                "bob@remote.example:4430",
                false,
                "a port that only starts 443",
            ),
            ("bob@invalid", true, "an unusable handle fails later"),
        ] {
            assert_eq!(port_allowed(profile, false), allowed, "{why}");
        }
        assert!(
            port_allowed("bob@remote.example:8080", true),
            "the development switch allows any port"
        );
    }

    #[tokio::test]
    async fn the_json_endpoint_refuses_a_non_standard_port_by_default() {
        let state = guarded_state();
        for profile in ["bob@remote.example:8080", "bob@203.0.113.9:25"] {
            let params = InteractionParams {
                ap_id: Some(AP_ID.to_owned()),
                profile: Some(profile.to_owned()),
            };
            let err = interaction_answer(&state, params, None)
                .await
                .expect_err("a ported handle is refused");
            assert!(matches!(err, AppError::BadRequest(_)), "{profile}: a 400");
            assert_eq!(
                err.to_string(),
                COULD_NOT_FIND_USER,
                "{profile}: the same refusal as an unknown profile"
            );
        }
    }

    #[tokio::test]
    async fn the_json_endpoint_still_fingers_the_https_port() {
        let (server, base) = serve(webfinger_route(JRD_JSON, JRD_SUBSCRIBE)).await;
        let params = InteractionParams {
            ap_id: Some(AP_ID.to_owned()),
            profile: Some("bob@remote.example:443".to_owned()),
        };
        let reply = interaction_answer(&guarded_state(), params, Some(&base))
            .await
            .expect("port 443 is a standard port");
        server.abort();
        assert_eq!(reply.0.url, WANT_URL, "the lookup went through");
    }

    #[tokio::test]
    async fn the_json_endpoint_refuses_a_profile_without_a_subscribe_link() {
        let (server, base) = serve(webfinger_route(JRD_JSON, JRD_NO_SUBSCRIBE)).await;
        let state = client_state();
        let params = InteractionParams {
            ap_id: Some(AP_ID.to_owned()),
            profile: Some(PROFILE.to_owned()),
        };
        let err = interaction_answer(&state, params, Some(&base))
            .await
            .expect_err("no template is no interaction");
        server.abort();
        assert_eq!(
            err.to_string(),
            COULD_NOT_FIND_USER,
            "Pleroma's refusal wording for an answer with no subscribe link"
        );
    }

    #[test]
    fn the_subscribe_url_expands_the_macro_with_the_encoded_target() {
        let document = json!({"links": [
            {"rel": SUBSCRIBE_REL, "template": "https://r.example/f?acct={uri}"}
        ]});
        assert_eq!(
            expand_subscribe_url(&document, AP_ID).as_deref(),
            Some("https://r.example/f?acct=https%3A%2F%2Ftest.example%2Fusers%2Falice"),
            "the {{uri}} macro takes the percent-encoded ap_id"
        );
    }

    #[test]
    fn the_percent_encoded_macro_is_expanded_too() {
        let document = json!({"links": [
            {"rel": SUBSCRIBE_REL, "template": "https://r.example/f?acct=%7Buri%7D"}
        ]});
        assert_eq!(
            expand_subscribe_url(&document, AP_ID).as_deref(),
            Some(format!("https://r.example/f?acct={AP_ID_ENCODED}").as_str()),
            "both macro spellings carry the target"
        );
    }

    #[test]
    fn the_encoded_target_cannot_break_out_of_the_query_value() {
        let document = json!({"links": [
            {"rel": SUBSCRIBE_REL, "template": "https://r.example/f?u={uri}"}
        ]});
        let url = expand_subscribe_url(&document, "https://x.example/a b&c=1").expect("a url");
        assert_eq!(
            url, "https://r.example/f?u=https%3A%2F%2Fx.example%2Fa+b%26c%3D1",
            "space, & and = are all encoded into the value"
        );
    }

    #[test]
    fn only_the_subscribe_rel_with_a_template_counts() {
        for (document, why) in [
            (json!({"links": []}), "no links at all"),
            (
                json!({"links": [{"rel": "self", "template": "https://x/?u={uri}"}]}),
                "the wrong rel",
            ),
            (
                json!({"links": [{"rel": "http://webfinger.net/rel/profile-page"}]}),
                "no subscribe link",
            ),
            (
                json!({"links": [{"rel": SUBSCRIBE_REL, "template": "  "}]}),
                "a blank template",
            ),
            (
                json!({"links": [{"rel": SUBSCRIBE_REL, "template": "https://x/f"}]}),
                "a template with no macro",
            ),
            (json!({"links": "not-an-array"}), "links that are no array"),
            (json!({}), "no links member"),
        ] {
            assert_eq!(
                expand_subscribe_url(&document, AP_ID),
                None,
                "{why} is no interaction url"
            );
        }
    }

    #[test]
    fn the_first_usable_subscribe_link_wins() {
        let document = json!({"links": [
            {"rel": SUBSCRIBE_REL, "template": "https://first.example/f?u={uri}"},
            {"rel": SUBSCRIBE_REL, "template": "https://second.example/f?u={uri}"}
        ]});
        assert_eq!(
            expand_subscribe_url(&document, AP_ID).as_deref(),
            Some("https://first.example/f?u=https%3A%2F%2Ftest.example%2Fusers%2Falice"),
            "document order decides"
        );
    }

    #[test]
    fn the_subscribe_rel_is_trimmed_before_comparing() {
        let document = json!({"links": [
            {"rel": format!(" {SUBSCRIBE_REL} "), "template": "https://r.example/f?u={uri}"}
        ]});
        assert!(
            expand_subscribe_url(&document, AP_ID).is_some(),
            "surrounding whitespace is not part of the rel"
        );
    }

    #[tokio::test]
    async fn a_jrd_answer_builds_the_interaction_url() {
        let app = webfinger_route(JRD_JSON, JRD_SUBSCRIBE);
        assert_eq!(
            url_against(app, AP_ID).await.as_deref(),
            Some(WANT_URL),
            "the template with the encoded ap_id in place of the macro"
        );
    }

    #[tokio::test]
    async fn an_xrd_answer_builds_the_same_url() {
        let app = webfinger_route(XRD_XML, XRD_SUBSCRIBE);
        assert_eq!(
            url_against(app, AP_ID).await.as_deref(),
            Some(WANT_URL),
            "the normalized document reads the same"
        );
    }

    #[tokio::test]
    async fn the_encoded_macro_answer_builds_the_url_too() {
        let app = webfinger_route(JRD_JSON, JRD_ENCODED_MACRO);
        assert_eq!(
            url_against(app, AP_ID).await.as_deref(),
            Some(format!("https://remote.example/follow?uri={AP_ID_ENCODED}").as_str()),
            "the %7Buri%7D spelling expands"
        );
    }

    #[tokio::test]
    async fn a_useless_subscribe_answer_is_no_url() {
        for (body, why) in [
            (JRD_NO_TEMPLATE, "the link carries no template"),
            (JRD_NO_MACRO, "the template carries no macro"),
            (JRD_NO_SUBSCRIBE, "there is no subscribe link"),
            ("{oops", "the body is no document"),
        ] {
            let app = webfinger_route(JRD_JSON, body);
            assert_eq!(
                url_against(app, AP_ID).await,
                None,
                "{why} is no interaction url"
            );
        }
    }

    #[tokio::test]
    async fn an_unreachable_profile_is_no_url() {
        let app = status_route(StatusCode::NOT_FOUND);
        assert_eq!(
            url_against(app, AP_ID).await,
            None,
            "a 404 WebFinger with no host-meta fallback resolves no profile"
        );
    }

    #[tokio::test]
    async fn an_unusable_profile_never_reaches_the_network() {
        // No server at all: `bob@invalid` fails the handle grammar before
        // any request, so this proves the early refusal.
        let missing = subscribe_url_for(&client(), AP_ID, "bob@invalid", None).await;
        assert_eq!(missing, None, "a single-label domain resolves nothing");
    }
}
