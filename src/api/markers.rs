//! Markers over HTTP: where an account left off reading each timeline,
//! `GET` and `POST /api/v1/markers`.
//!
//! The reference is Pleroma's `MarkerController`
//! (`lib/pleroma/web/mastodon_api/controllers/marker_controller.ex`) and
//! `Pleroma.Marker` (`lib/pleroma/marker.ex`). What a marker *is* — one row per
//! account and timeline, its position written in place with the version counter
//! bumped — stays behind [`crate::models::marker`]; this module is the two
//! routes, the parameter shapes and the `{timeline: marker}` object in front of
//! them.
//!
//! - `GET /api/v1/markers?timeline[]=home&timeline[]=notifications` answers the
//!   caller's markers for the timelines named by `timeline`, which accepts a
//!   list (`timeline[]=…`, or the repeated plain key `timeline=home&timeline=…`)
//!   or a single name (`timeline=home`). Only timelines that are asked for *and*
//!   have a marker appear, so an absent `timeline` — which asks for none —
//!   answers `{}`, as Pleroma's does. Unknown names are simply never had.
//! - `POST /api/v1/markers` takes one object per timeline,
//!   `{"home": {"last_read_id": "…"}, "notifications": {"last_read_id": "…"}}`,
//!   in JSON or in the form spelling `home[last_read_id]=…`; each timeline it
//!   names is upserted and answered in the same shape as `GET`, newest version
//!   included. Keys other than the two timelines are ignored, Pleroma's
//!   `Map.take` precedent, so an empty body posts nothing and answers `{}`. A
//!   posted timeline without a `last_read_id` marks the position empty,
//!   Pleroma's schema default.
//!
//! Each marker renders as the [`Marker`] entity — `last_read_id`, `version`,
//! `updated_at` in the Mastodon time format (decision 5) and Pleroma's
//! `pleroma.unread_count` — the counts coming from
//! [`unread_notifications_count`](crate::models::marker::unread_notifications_count)
//! for `notifications` and `0` for `home`. The object's keys are in timeline
//! name order, `home` before `notifications`.
//!
//! Both routes require a user token ([`AuthUser`]) and,
//! per Pleroma's scopes plug on these actions, `read:statuses` to look and
//! `write:statuses` to store.

use std::collections::BTreeMap;

use axum::{Json, Router, extract::State, routing::get};
use serde::Deserialize;
use tracing::debug;

use crate::{
    auth::AuthUser,
    entities::marker::Marker,
    error::AppResult,
    extract::{ApiBody, ApiParams},
    models::marker::{self, Marker as MarkerRow},
    oauth::scopes::{READ_STATUSES, WRITE_STATUSES},
    state::AppState,
};

/// The `{timeline name: marker}` object both endpoints answer with. A map, not
/// a `Vec`: markers are keyed by timeline, and the name order is the response's
/// key order.
type Markers = BTreeMap<String, Marker>;

/// The `/api/v1/markers` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new().route("/markers", get(read_markers).post(write_markers))
}

/// `GET /api/v1/markers` — the caller's markers for the timelines `timeline`
/// names.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from
/// [`AuthUser`](crate::auth::AuthUser) and the `read:statuses` check;
/// [`Unprocessable`](crate::error::AppError::Unprocessable) when `timeline` is
/// neither a name nor a list of names. Any database failure arrives through `?`
/// as [`Database`](crate::error::AppError::Database).
async fn read_markers(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiParams(params): ApiParams<ReadParams>,
) -> AppResult<Json<Markers>> {
    auth.require(READ_STATUSES)?;
    let rows = marker::get_many(&state.db, auth.0.id, &params.timelines()).await?;
    debug!(
        count = rows.len(),
        user = %auth.0.id,
        "markers read"
    );
    Ok(markers_response(rows))
}

/// `POST /api/v1/markers` — upsert every position the body posted and answer
/// the markers it wrote.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from
/// [`AuthUser`](crate::auth::AuthUser) and the `write:statuses` check;
/// [`Unprocessable`](crate::error::AppError::Unprocessable) when the body's
/// timeline values are not objects with a text position. Any database failure
/// arrives through `?` as [`Database`](crate::error::AppError::Database).
async fn write_markers(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<WriteParams>,
) -> AppResult<Json<Markers>> {
    auth.require(WRITE_STATUSES)?;
    let rows = marker::upsert_many(&state.db, auth.0.id, params.pairs()).await?;
    debug!(
        count = rows.len(),
        user = %auth.0.id,
        "markers upserted"
    );
    Ok(markers_response(rows))
}

/// Renders written and read rows alike as the response object: each row under
/// its own timeline name, with that timeline's unread count in the `pleroma`
/// half.
fn markers_response(rows: Vec<MarkerRow>) -> Json<Markers> {
    let mut markers = Markers::new();
    for row in rows {
        let _entry: Option<Marker> = markers.insert(
            row.timeline.clone(),
            Marker::from_row(&row, unread_count(&row)),
        );
    }
    Json(markers)
}

/// The `pleroma.unread_count` of `row`: notifications ask the model how far
/// past the marker they run; every other timeline reports `0`, because `home`
/// has no unread concept for Pleroma either.
fn unread_count(row: &MarkerRow) -> i64 {
    if row.timeline != marker::NOTIFICATIONS {
        return 0;
    }
    marker::unread_notifications_count(row.user_id, &row.last_read_id)
}

/// The query parameters of `GET /api/v1/markers`.
#[derive(Debug, Deserialize)]
struct ReadParams {
    /// Which timelines to read; absent asks for none.
    #[serde(default)]
    timeline: Option<TimelineNames>,
}

/// `timeline` as clients send it: one name or a list (a JSON array, or
/// `timeline[]=…` keys in a query string).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TimelineNames {
    /// A lone timeline name.
    Single(String),
    /// A list of timeline names.
    Multiple(Vec<String>),
}

impl ReadParams {
    /// The requested names, borrowed: one for `timeline=home`, all of them for
    /// `timeline[]=…`, none when the parameter is absent.
    fn timelines(&self) -> Vec<&str> {
        self.timeline
            .as_ref()
            .map_or_else(Vec::new, |names| match names {
                TimelineNames::Single(name) => vec![name.as_str()],
                TimelineNames::Multiple(values) => values.iter().map(String::as_str).collect(),
            })
    }
}

/// The body of `POST /api/v1/markers`: the timelines it posted a position for.
/// The two markers are fields, so any other key — Pleroma's `Map.take` drops it
/// the same way — is ignored rather than rejected.
#[derive(Debug, Deserialize)]
struct WriteParams {
    /// The `home` marker, when the body named it.
    #[serde(default)]
    home: Option<TimelineParams>,
    /// The `notifications` marker, when the body named it.
    #[serde(default)]
    notifications: Option<TimelineParams>,
}

/// One posted timeline: an object whose only key this endpoint reads.
#[derive(Debug, Deserialize)]
struct TimelineParams {
    /// The position to store; absent or `null` is the empty position, Pleroma's
    /// `last_read_id` default.
    #[serde(default)]
    last_read_id: Option<String>,
}

impl WriteParams {
    /// The `(timeline, position)` pairs to upsert, `home` first, naming only
    /// the timelines the body sent.
    fn pairs(&self) -> Vec<(&str, &str)> {
        let mut pairs = Vec::new();
        if let Some(home) = &self.home {
            pairs.push((marker::HOME, home.position()));
        }
        if let Some(notifications) = &self.notifications {
            pairs.push((marker::NOTIFICATIONS, notifications.position()));
        }
        pairs
    }
}

impl TimelineParams {
    /// The posted position, empty when the key was absent or `null`.
    fn position(&self) -> &str {
        self.last_read_id.as_deref().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// Deserializes `value` as the `GET` parameters, as the extractor would.
    fn read_params(value: Value) -> ReadParams {
        crate::extract::into_params(value).expect("the fixture deserializes")
    }

    /// The timeline names `value` asks `GET` to read.
    fn requested(value: Value) -> Vec<String> {
        read_params(value)
            .timelines()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// The `(timeline, position)` pairs `value` posts, each as
    /// `timeline=position` text for a readable assertion.
    fn posted(value: Value) -> Vec<String> {
        let params: WriteParams =
            crate::extract::into_params(value).expect("the fixture deserializes");
        params
            .pairs()
            .into_iter()
            .map(|(timeline, position)| format!("{timeline}={position}"))
            .collect()
    }

    #[test]
    fn an_absent_timeline_parameter_asks_for_nothing() {
        assert_eq!(requested(json!({})), Vec::<String>::new(), "no key");
        assert_eq!(
            requested(json!({"timeline": null})),
            Vec::<String>::new(),
            "the key with no value"
        );
    }

    #[test]
    fn a_single_timeline_name_is_the_one_element_list() {
        assert_eq!(requested(json!({"timeline": "home"})), ["home"]);
        assert_eq!(
            requested(json!({"timeline": "bogus"})),
            ["bogus"],
            "an unknown name is asked for as given, and answers nothing"
        );
    }

    #[test]
    fn a_timeline_list_is_kept_in_order() {
        assert_eq!(
            requested(json!({"timeline": ["notifications", "home"]})),
            ["notifications", "home"],
            "as `timeline[]=…` keys arrive"
        );
        assert_eq!(requested(json!({"timeline": []})), Vec::<String>::new());
    }

    #[test]
    fn a_timeline_that_is_neither_name_nor_list_is_a_parameter_error() {
        let err = crate::extract::into_params::<ReadParams>(json!({"timeline": {"a": "b"}}))
            .expect_err("an object is not a timeline name");
        assert!(
            err.to_string().starts_with("Invalid parameters"),
            "and the extractor's wording says so: {err}"
        );
    }

    #[test]
    fn a_posted_body_becomes_one_pair_per_timeline_it_names() {
        assert_eq!(
            posted(
                json!({"home": {"last_read_id": "10"}, "notifications": {"last_read_id": "20"}})
            ),
            ["home=10", "notifications=20"],
            "both timelines, home first"
        );
        assert_eq!(
            posted(json!({"notifications": {"last_read_id": "20"}})),
            ["notifications=20"],
            "and one when only one was sent"
        );
    }

    #[test]
    fn unknown_keys_and_nulls_are_nothing_to_write() {
        assert_eq!(
            posted(json!({})),
            Vec::<String>::new(),
            "an empty body posts nothing"
        );
        assert_eq!(
            posted(json!({"direct": {"last_read_id": "1"}, "chat": {}})),
            Vec::<String>::new(),
            "unknown timelines are ignored, not rejected"
        );
        assert_eq!(
            posted(json!({"home": null})),
            Vec::<String>::new(),
            "a null marker names no position"
        );
    }

    #[test]
    fn an_absent_or_null_position_is_the_empty_one() {
        assert_eq!(
            posted(json!({"home": {}})),
            ["home="],
            "the key with no position"
        );
        assert_eq!(
            posted(json!({"home": {"last_read_id": null}})),
            ["home="],
            "and the position present but null"
        );
        assert_eq!(
            posted(json!({"home": {"last_read_id": "x", "version": 4}})),
            ["home=x"],
            "other keys inside a timeline are ignored too"
        );
    }

    #[test]
    fn a_timeline_value_of_the_wrong_shape_is_a_parameter_error() {
        for body in [
            json!({"home": "10"}),
            json!({"home": 10}),
            json!({"home": true}),
            json!({"notifications": ["10", "20"]}),
        ] {
            let err = crate::extract::into_params::<WriteParams>(body.clone())
                .expect_err(&format!("`{body}` is not a marker object"));
            assert!(
                err.to_string().starts_with("Invalid parameters"),
                "the extractor's wording for `{body}`: {err}"
            );
        }
    }
}
