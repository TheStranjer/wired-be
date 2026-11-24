//! The terms of service view: `GET /api/v1/instance/terms_of_service`.

use axum::{Json, extract::State};
use serde::Serialize;

use crate::{
    api::admin::apps_docs::{TERMS_OF_SERVICE, read_document_stamped},
    error::AppResult,
    state::AppState,
    util::time::mastodon_time,
};

/// The document `GET /api/v1/instance/terms_of_service` answers, the
/// shape of Mastodon's `Api::V1::Instances::TermsOfServicesController`
/// reduced to the two keys the stored document can fill.
#[derive(Debug, Serialize)]
pub(super) struct TermsOfService {
    /// When the stored document was last written, in Mastodon's time
    /// format.
    updated_at: String,
    /// The stored `terms-of-service` document, an HTML string.
    content: String,
}

/// `GET /api/v1/instance/terms_of_service` — the terms of service an
/// admin uploaded as the `terms-of-service` instance document, as JSON.
/// No authentication.
///
/// # Errors
///
/// [`AppError::RouteNotFound`](crate::error::AppError) when no terms of
/// service were uploaded (or the name is not configured);
/// [`AppError::Internal`](crate::error::AppError) for a store that
/// refuses to be read.
pub(super) async fn show_terms_of_service(
    State(state): State<AppState>,
) -> AppResult<Json<TermsOfService>> {
    let (bytes, written) = read_document_stamped(&state.config, TERMS_OF_SERVICE).await?;
    Ok(Json(TermsOfService {
        updated_at: mastodon_time(&written),
        content: String::from_utf8_lossy(&bytes).into_owned(),
    }))
}
