//! The unit-test fixtures the external-login children share: the provider
//! the route-shaped tests configure, the state those tests build a router
//! over, the named header value of a response, and the decoded query
//! parameters of a URL. Test-only.

use axum::{http::HeaderName, response::Response};
use url::form_urlencoded;

use crate::{
    config::{Config, OauthProvider},
    state::AppState,
};

/// The provider the fixtures describe.
pub(super) fn provider(name: &str) -> OauthProvider {
    OauthProvider {
        name: name.to_owned(),
        client_id: format!("{name}-client-id"),
        client_secret: format!("{name}-client-secret"),
        authorize_url: format!("https://{name}.example/authorize"),
        token_url: format!("https://{name}.example/token"),
        userinfo_url: format!("https://{name}.example/userinfo"),
        scopes: "read:user user:email".to_owned(),
        uid_field: "id".to_owned(),
        nickname_field: "login".to_owned(),
        email_field: "email".to_owned(),
        name_field: "name".to_owned(),
    }
}

/// The state the route-shaped tests need: config with one provider per
/// name, and a lazy pool those gates never consult.
pub(super) fn test_state(names: &[&str]) -> AppState {
    let mut config = Config::for_tests();
    config.oauth_providers = names.iter().map(|name| provider(name)).collect();
    AppState::lazy_for_tests(config)
}

/// The named header value of a response, as text.
pub(super) fn header_of(response: &Response, name: HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)?
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// The decoded query parameters of a URL, in order.
pub(super) fn query_pairs(url: &str) -> Vec<(String, String)> {
    let (_, query) = url.split_once('?').expect("a query string");
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}
