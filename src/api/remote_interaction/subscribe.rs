//! The frontend redirect `GET /authorize_interaction` answers with.

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse as _, Response},
};

use crate::{
    config::Config,
    error::{AppError, AppResult},
};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
