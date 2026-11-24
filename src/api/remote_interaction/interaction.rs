//! The interaction endpoints: `POST /api/v1/pleroma/remote_interaction`
//! and `GET /authorize_interaction`. The subscribe lookup, its
//! `WebFinger` template reading and macro expansion, and the frontend
//! redirect for whatever a `uri` resolves to.

use axum::{
    Json,
    extract::{Query, State},
    response::Response,
};
use serde_json::Value;
use tracing::debug;
use url::form_urlencoded;

use crate::{
    api::pleroma_account_settings::looks_like_ap_id,
    error::{AppError, AppResult},
    extract::ApiBody,
    federation::{
        fetch_actor::{get_or_fetch_by_acct, get_or_fetch_by_ap_id},
        fetch_object::resolve_status,
        webfinger_client::{URI_MACRO, URI_MACRO_ENCODED, finger_with_base, strip_finger_prefixes},
    },
    http_client::HttpClient,
    state::AppState,
};

use super::{
    COULD_NOT_FIND_USER, InteractionParams, InteractionQuery, InteractionReply, MISSING_AP_ID,
    MISSING_PROFILE, SUBSCRIBE_REL, pages::required, subscribe::frontend_redirect,
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
/// template.
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
    let url = subscribe_url_for(&state.http_client, ap_id, profile, webfinger_base)
        .await
        .ok_or_else(|| AppError::BadRequest(COULD_NOT_FIND_USER.to_owned()))?;
    Ok(Json(InteractionReply { url }))
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

/// `GET /authorize_interaction` (Pleroma's `authorize_interaction`,
/// which bounces to its own subscribe form; this instance keeps the page
/// flow on the frontend instead): the 302 to the frontend page for
/// whatever `uri` resolves to.
///
/// # Errors
///
/// [`AppError::NotFound`] — the 404 JSON — for an absent or blank `uri`
/// and for one that resolves to no actor and no status.
pub(super) async fn authorize_interaction(
    State(state): State<AppState>,
    Query(query): Query<InteractionQuery>,
) -> AppResult<Response> {
    let Some(uri) = required(query.uri.as_deref()) else {
        return Err(AppError::NotFound);
    };
    interaction_redirect(&state, uri).await
}

/// The `GET /authorize_interaction` resolution rule: an `http(s)` URL is
/// tried as an actor first and as a status second (Pleroma's own check
/// distinguishes the same two shapes); anything else is a handle through
/// [`get_or_fetch_by_acct`] with its `@`/`acct:` prefixes folded. A local
/// row answers from the database in either direction; only an unseen
/// remote address touches the network.
async fn interaction_redirect(state: &AppState, uri: &str) -> AppResult<Response> {
    if looks_like_ap_id(uri) {
        if let Ok(user) = get_or_fetch_by_ap_id(state, uri).await {
            return frontend_redirect(&state.config, &format!("users/{}", user.id));
        }
        if let Ok(Some(status)) = resolve_status(state, uri).await {
            return frontend_redirect(&state.config, &format!("notice/{}", status.id));
        }
        debug!(uri, "authorize_interaction resolved no actor and no status");
        return Err(AppError::NotFound);
    }
    match get_or_fetch_by_acct(state, strip_finger_prefixes(uri)).await {
        Ok(user) => frontend_redirect(&state.config, &format!("users/{}", user.id)),
        Err(err) => {
            debug!(%err, uri, "authorize_interaction could not resolve the handle");
            Err(AppError::NotFound)
        }
    }
}

#[cfg(test)]
mod tests {
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
        federation::webfinger_client::{JRD_JSON, XRD_XML},
    };

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
