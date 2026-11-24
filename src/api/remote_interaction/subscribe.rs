//! The form endpoints: the `GET/POST /main/ostatus` remote-subscribe
//! pair, and the frontend and external redirects they answer with.

use axum::{
    extract::{Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse as _, Response},
};
use tracing::debug;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::user::{User, get_by_acct},
    state::AppState,
};

use super::{
    COULD_NOT_FIND_USER_PAGE, RemoteSubscribeBody, RemoteSubscribeQuery, SOMETHING_WENT_WRONG,
    SubscribeFields,
    interaction::subscribe_url_for,
    pages::{remote_subscribe_page, trimmed},
};

/// `GET /main/ostatus` (Pleroma's `show_subscribe_form` in its form
/// half): the remote-subscribe form, prefilled from the flat `nickname`
/// and `profile` query pair and escaped.
pub(super) async fn show_remote_subscribe(Query(query): Query<RemoteSubscribeQuery>) -> Response {
    remote_subscribe_page(
        trimmed(query.nickname.as_deref()),
        trimmed(query.profile.as_deref()),
        None,
    )
}

/// `POST /main/ostatus` (Pleroma's `remote_subscribe` with the nested
/// `user` map): the local actor named by `user[nickname]`, the subscribe
/// template of `user[profile]`, and the 302 to the URL the JSON endpoint
/// would have answered for the pair. Every failure re-renders the form
/// with an error line and both submitted values kept.
pub(super) async fn do_remote_subscribe(
    State(state): State<AppState>,
    ApiBody(body): ApiBody<RemoteSubscribeBody>,
) -> Response {
    let empty = SubscribeFields::default();
    let fields = body.user.as_ref().unwrap_or(&empty);
    remote_subscribe_answer(
        &state,
        trimmed(fields.nickname.as_deref()),
        trimmed(fields.profile.as_deref()),
        None,
    )
    .await
}

/// The `POST /main/ostatus` decision, extracted so the loopback-mock
/// tests can point `webfinger_base` at their `WebFinger` answer (the same
/// seam as [`interaction_answer`]). Empty fields are the "no such user"
/// refusal; the local actor's `ap_id` expanded into the profile's
/// subscribe template is the redirect; anything that fails is the form
/// back with an error line.
///
/// [`interaction_answer`]: crate::api::remote_interaction::interaction::interaction_answer
async fn remote_subscribe_answer(
    state: &AppState,
    nickname: &str,
    profile: &str,
    webfinger_base: Option<&str>,
) -> Response {
    if nickname.is_empty() || profile.is_empty() {
        return remote_subscribe_page(nickname, profile, Some(COULD_NOT_FIND_USER_PAGE));
    }
    let Some(user) = local_subscribe_actor(state, nickname).await else {
        return remote_subscribe_page(nickname, profile, Some(COULD_NOT_FIND_USER_PAGE));
    };
    let Some(url) =
        subscribe_url_for(&state.http_client, &user.ap_id, profile, webfinger_base).await
    else {
        return remote_subscribe_page(nickname, profile, Some(SOMETHING_WENT_WRONG));
    };
    found_redirect(&url)
        .unwrap_or_else(|| remote_subscribe_page(nickname, profile, Some(SOMETHING_WENT_WRONG)))
}

/// The `user[nickname]` of `POST /main/ostatus`: the row
/// [`get_by_acct`] finds for the submitted handle — bare nickname or
/// spelled with this instance's own domain alike — when it is a live
/// *local* account. This page sends the local actor's `ap_id` out, so a
/// remote mirror is no actor to interact as.
async fn local_subscribe_actor(state: &AppState, nickname: &str) -> Option<User> {
    let found = match get_by_acct(&state.db, &state.config, nickname).await {
        Ok(found) => found,
        Err(err) => {
            debug!(%err, nickname, "the subscribe actor could not be read");
            return None;
        }
    };
    found.filter(User::is_local_active)
}

/// The 302 to `{FRONTEND_BASE_URL}/{path}` (the shape
/// [`crate::api::activitypub`]'s frontend redirects share): a location
/// whose text cannot ride a `Location` header names no page, and the
/// route answers its own 404.
pub(super) fn frontend_redirect(config: &Config, path: &str) -> AppResult<Response> {
    let location = format!("{}/{}", config.frontend_base_url, path);
    let value = HeaderValue::from_str(&location)
        .ok()
        .ok_or(AppError::NotFound)?;
    Ok((StatusCode::FOUND, [(header::LOCATION, value)]).into_response())
}

/// The external 302 of `POST /main/ostatus`: the `Location` is a remote
/// instance's URL, so one that is not header-safe is [`None`] and the
/// caller re-renders the form.
fn found_redirect(location: &str) -> Option<Response> {
    let value = HeaderValue::from_str(location).ok()?;
    Some((StatusCode::FOUND, [(header::LOCATION, value)]).into_response())
}

#[cfg(test)]
mod tests {
    use axum::http::{StatusCode, header};

    use super::*;
    use crate::{
        api::remote_interaction::test_support::{
            AP_ID, JRD_NO_TEMPLATE, JRD_SUBSCRIBE, PROFILE, WANT_URL, body_of, local_alice, serve,
            test_state, webfinger_route,
        },
        federation::webfinger_client::JRD_JSON,
        models::user::{NewUser, insert},
    };

    #[tokio::test]
    async fn the_remote_subscribe_post_redirects_to_the_interaction_url() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let alice = local_alice(&pool).await;
        assert_eq!(
            alice.ap_id, AP_ID,
            "the fixture actor is the expanded target"
        );
        let (server, base) = serve(webfinger_route(JRD_JSON, JRD_SUBSCRIBE)).await;

        let plain = remote_subscribe_answer(&state, "alice", PROFILE, Some(&base)).await;
        let spelled =
            remote_subscribe_answer(&state, "alice@test.example", PROFILE, Some(&base)).await;
        server.abort();

        for response in [plain, spelled] {
            assert_eq!(
                response.status(),
                StatusCode::FOUND,
                "the subscribe post redirects"
            );
            assert_eq!(
                response.headers()[header::LOCATION],
                WANT_URL,
                "to the same url the JSON endpoint answers"
            );
        }
    }

    #[tokio::test]
    async fn the_remote_subscribe_post_re_renders_when_the_profile_answers_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let _alice: User = local_alice(&pool).await;
        let (server, base) = serve(webfinger_route(JRD_JSON, JRD_NO_TEMPLATE)).await;

        let response = remote_subscribe_answer(&state, "alice", PROFILE, Some(&base)).await;
        server.abort();

        assert_eq!(response.status(), StatusCode::OK, "the form comes back");
        let body = body_of(response).await;
        assert!(
            body.contains("<p class=\"error\">Something went wrong.</p>"),
            "Pleroma's catch-all line: {body}"
        );
        assert!(body.contains("value=\"alice\""), "the pair is kept: {body}");
    }

    #[tokio::test]
    async fn the_remote_subscribe_post_refuses_a_nonlocal_actor_without_a_lookup() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let eve: User = insert(
            &pool,
            &NewUser {
                nickname: "eve@other.example".to_owned(),
                ap_id: "https://other.example/users/eve".to_owned(),
                domain: Some("other.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the fixture mirror inserts");
        assert!(!eve.local.get(), "the fixture is a remote mirror");

        for nickname in ["nobody", "eve@other.example"] {
            let response = remote_subscribe_answer(&state, nickname, "bob@invalid", None).await;
            assert_eq!(response.status(), StatusCode::OK, "the form comes back");
            let body = body_of(response).await;
            assert!(
                body.contains("<p class=\"error\">Could not find user</p>"),
                "`{nickname}` is no local actor to interact as: {body}"
            );
        }
    }

    #[test]
    fn the_frontend_redirect_names_the_configured_origin() {
        let config = Config::for_tests();
        let response = frontend_redirect(&config, "users/01934568-1111-7000-8000-abcdef012345")
            .expect("a uuid path is header-safe");
        assert_eq!(response.status(), StatusCode::FOUND, "the redirect status");
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://test.example/users/01934568-1111-7000-8000-abcdef012345",
            "the frontend origin carries the path"
        );
        let err = frontend_redirect(&config, "users/bad\nid")
            .expect_err("a path with a newline cannot ride a header");
        assert!(matches!(err, AppError::NotFound), "the 404 it answers with");
    }

    #[test]
    fn the_external_redirect_carries_the_remote_url_or_nothing() {
        let response = found_redirect("https://remote.example/ostatus_subscribe?acct=x")
            .expect("a plain url is header-safe");
        assert_eq!(response.status(), StatusCode::FOUND, "the redirect status");
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://remote.example/ostatus_subscribe?acct=x",
            "the Location is the expanded template verbatim"
        );
        assert!(
            found_redirect("https://r.example/a\nb").is_none(),
            "a url with a newline is no redirect"
        );
    }
}
