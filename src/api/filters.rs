//! Filters over HTTP: one account's phrase filters, the Mastodon v1 CRUD.
//!
//! The reference is Pleroma's `FilterController`
//! (`lib/pleroma/web/mastodon_api/controllers/filter_controller.ex`) and
//! `Pleroma.Filter` (`lib/pleroma/filter.ex`). Pleroma's per-user integer
//! `filter_id`, its `hide`-column renaming and its Oban purge job all stay
//! behind the model ([`crate::models::filter`]); this module is the five
//! routes and the parameter rules in front of them.
//!
//! - `GET /api/v1/filters` answers the caller's filters that have not
//!   expired ([`list_active`](crate::models::filter::list_active)), newest
//!   first, each as the `Filter` entity
//!   ([`crate::entities::filter`]).
//! - `GET /api/v1/filters/{id}` answers one filter by id; an expired filter
//!   is still a row its owner can read.
//! - `POST /api/v1/filters` creates, `PUT /api/v1/filters/{id}` replaces
//!   every mutable column of one filter, and `DELETE /api/v1/filters/{id}`
//!   removes it and answers `{}`.
//!
//! Both writes take the same parameters (decision 10: through
//! [`ApiBody`], so `context[]` array keys work in forms): `phrase`
//! (required, at most 200 characters), `context` (required, non-empty, each
//! value one of `home`, `notifications`, `public`, `thread`, `account`),
//! `irreversible` and `whole_word` (booleans; `whole_word` defaults to
//! `true`) and `expires_in` (seconds; `null` or absent means the filter
//! never expires, a positive value stamps `expires_at` that far ahead).
//! A blank or absent phrase is the 422 `{"error":"Validation failed:
//! Phrase can't be blank"}`, an absent, empty or unknown context
//! `{"error":"Validation failed: Context is invalid"}` — Mastodon's
//! changeset wording — and Pleroma's `CastAndValidate` plug precedent puts
//! both ahead of any lookup. An id that is not `UUID` text, names no
//! filter, or names one the caller does not own is the same 404 "Record not
//! found": foreign rows are indistinguishable from missing ones.
//!
//! All five routes require a user token ([`AuthUser`]) and, per the
//! ticket's scope pair, `read:filters` to look and `write:filters` to
//! create, replace and delete. Each successful write then publishes
//! Mastodon's `filters_changed` on the caller's own `user:` stream, so a
//! client watching its timelines re-reads its filters
//! ([`filters_changed`](crate::services::streaming::filters_changed)).

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::debug;

use crate::{
    auth::AuthUser,
    entities::filter::Filter,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::filter,
    oauth::scopes::{READ_FILTERS, WRITE_FILTERS},
    services::streaming,
    state::AppState,
    util::id::parse_id,
};

/// The longest phrase the endpoint accepts (Mastodon's filter limit).
const MAX_PHRASE_CHARS: usize = 200;
/// The contexts a filter may name (Mastodon's valid filter contexts).
const VALID_CONTEXTS: [&str; 5] = ["home", "notifications", "public", "thread", "account"];
/// 422 body for an absent or blank phrase (Mastodon's changeset wording).
const PHRASE_BLANK: &str = "Validation failed: Phrase can't be blank";
/// 422 body for an absent, empty or unknown context (likewise).
const CONTEXT_INVALID: &str = "Validation failed: Context is invalid";

/// The `/api/v1/filters` router, merged into [`crate::api::v1`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/filters", get(list_filters).post(create_filter))
        .route(
            "/filters/{id}",
            get(show_filter).put(update_filter).delete(delete_filter),
        )
}

/// `GET /api/v1/filters` — the caller's filters that have not expired,
/// newest first.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:filters` check. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn list_filters(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<Filter>>> {
    auth.require(READ_FILTERS)?;
    let active = filter::list_active(&state.db, auth.0.id).await?;
    debug!(
        count = active.len(),
        user = %auth.0.id,
        "filters listed"
    );
    Ok(Json(active.iter().map(Filter::from_row).collect()))
}

/// `GET /api/v1/filters/{id}` — one of the caller's filters; an expired one
/// is still served, only the list hides it.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `read:filters` check; [`AppError::NotFound`] for an id that is
/// malformed, names no filter or names one of another account. Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn show_filter(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Filter>> {
    auth.require(READ_FILTERS)?;
    let Some(filter_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let Some(row) = filter::get_owned(&state.db, filter_id, auth.0.id).await? else {
        return Err(AppError::NotFound);
    };
    Ok(Json(Filter::from_row(&row)))
}

/// `POST /api/v1/filters` — create a filter for the caller and answer it.
/// A successful create publishes Mastodon's `filters_changed` to the
/// caller's own user stream, as the other two writes do.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:filters` check; [`AppError::Unprocessable`] when `phrase` or
/// `context` fails validation (see the [module docs]). Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn create_filter(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<FilterParams>,
) -> AppResult<Json<Filter>> {
    auth.require(WRITE_FILTERS)?;
    let new = params.into_new_filter()?;
    let row = filter::create(&state.db, auth.0.id, &new).await?;
    debug!(
        filter = %row.id,
        user = %auth.0.id,
        "filter created"
    );
    streaming::filters_changed(&state, auth.0.id);
    Ok(Json(Filter::from_row(&row)))
}

/// `PUT /api/v1/filters/{id}` — replace every mutable column of one owned
/// filter and answer it, publishing Mastodon's `filters_changed` to the
/// caller's own user stream. Parameters are those of `POST`, with the same
/// defaults: an absent `expires_in` clears the old expiry, an absent flag
/// takes its default rather than keeping the stored value.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:filters` check; [`AppError::Unprocessable`] for invalid
/// parameters — ahead of the lookup, as Pleroma's validation plug does;
/// [`AppError::NotFound`] when the id is malformed, unknown or foreign. Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn update_filter(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<FilterParams>,
) -> AppResult<Json<Filter>> {
    auth.require(WRITE_FILTERS)?;
    // Validation first, lookup second: Pleroma's `CastAndValidate` plug
    // rejects a bad body before the action ever reaches `Filter.get`.
    let new = params.into_new_filter()?;
    let Some(filter_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    let Some(row) = filter::update(&state.db, filter_id, auth.0.id, &new).await? else {
        return Err(AppError::NotFound);
    };
    streaming::filters_changed(&state, auth.0.id);
    Ok(Json(Filter::from_row(&row)))
}

/// `DELETE /api/v1/filters/{id}` — remove one owned filter, publish Mastodon's
/// `filters_changed` to the caller's own user stream, and answer `{}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:filters` check; [`AppError::NotFound`] when the id is
/// malformed, unknown or foreign. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn delete_filter(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_FILTERS)?;
    let Some(filter_id) = parse_id(&id) else {
        return Err(AppError::NotFound);
    };
    if !filter::delete(&state.db, filter_id, auth.0.id).await? {
        return Err(AppError::NotFound);
    }
    streaming::filters_changed(&state, auth.0.id);
    Ok(Json(Value::Object(Map::new())))
}

/// The request parameters of `POST` and `PUT /api/v1/filters`.
///
/// Every field is `Option` at the serde layer so the endpoint can answer
/// the ticket's `{"error":"Validation failed: …"}` wording for what is
/// missing; the defaults are applied in
/// [`into_new_filter`](FilterParams::into_new_filter).
#[derive(Debug, Deserialize)]
struct FilterParams {
    /// The phrase; required and non-blank, at most
    /// [`MAX_PHRASE_CHARS`] characters.
    #[serde(default)]
    phrase: Option<String>,
    /// The contexts; required, non-empty, every value known.
    #[serde(default)]
    context: Option<ContextValues>,
    /// Hide rather than warn; `false` when absent.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    irreversible: Option<bool>,
    /// Whole-word matching; `true` when absent, like the column.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    whole_word: Option<bool>,
    /// Seconds until expiry; `null` or absent (or not a positive,
    /// representable number of seconds) means the filter never expires.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_i64")]
    expires_in: Option<i64>,
}

/// `context` as clients send it: one string or a list (a JSON array, or
/// form `context[]=…` keys).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ContextValues {
    /// A lone context name.
    Single(String),
    /// A list of context names.
    Multiple(Vec<String>),
}

impl ContextValues {
    /// The names as a list; a lone string becomes the one-element list.
    fn into_list(self) -> Vec<String> {
        match self {
            Self::Single(text) => vec![text],
            Self::Multiple(values) => values,
        }
    }
}

impl FilterParams {
    /// Validates the parameters and builds the row content they describe,
    /// applying the defaults for the optional flags and the expiry.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] with Mastodon's changeset wording for a
    /// blank or too-long phrase or an invalid context list.
    fn into_new_filter(self) -> AppResult<filter::NewFilter> {
        Ok(filter::NewFilter {
            phrase: validated_phrase(self.phrase)?,
            context: validated_context(self.context)?,
            whole_word: self.whole_word.unwrap_or(true),
            irreversible: self.irreversible.unwrap_or_default(),
            expires_at: expires_at_from(self.expires_in),
        })
    }
}

/// The phrase, checked: absent or blank is 422 [`PHRASE_BLANK`], longer
/// than [`MAX_PHRASE_CHARS`] characters is 422 with Mastodon's own limit
/// wording; a kept phrase is stored verbatim.
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn validated_phrase(phrase: Option<String>) -> AppResult<String> {
    let text = phrase.unwrap_or_default();
    if text.trim().is_empty() {
        return Err(AppError::Unprocessable(PHRASE_BLANK.to_owned()));
    }
    if text.chars().count() > MAX_PHRASE_CHARS {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Phrase is too long (maximum is {MAX_PHRASE_CHARS} characters)"
        )));
    }
    Ok(text)
}

/// The context list, checked: absent, empty or carrying any name outside
/// [`VALID_CONTEXTS`] is 422 [`CONTEXT_INVALID`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
fn validated_context(context: Option<ContextValues>) -> AppResult<Vec<String>> {
    let values = context.map_or_else(Vec::new, ContextValues::into_list);
    let known = values
        .iter()
        .all(|value| VALID_CONTEXTS.contains(&value.as_str()));
    if values.is_empty() || !known {
        return Err(AppError::Unprocessable(CONTEXT_INVALID.to_owned()));
    }
    Ok(values)
}

/// The expiry an `expires_in` of `seconds` describes: `None` when absent or
/// `null`, when the value is not positive, or when it names a span too
/// large to represent — anything unrepresentable is "never", not an error.
fn expires_at_from(seconds: Option<i64>) -> Option<DateTime<Utc>> {
    seconds
        .filter(|value| *value > 0)
        .and_then(chrono::Duration::try_seconds)
        .and_then(|span| Utc::now().checked_add_signed(span))
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::json;

    use super::*;

    /// Parses `value` as the endpoint's parameters and validates them, as
    /// the handler would.
    fn new_filter(value: Value) -> AppResult<filter::NewFilter> {
        let params: FilterParams = crate::extract::into_params(value)?;
        params.into_new_filter()
    }

    /// The minimal valid parameter object.
    fn minimal() -> Value {
        json!({"phrase": "foo", "context": ["home"]})
    }

    /// The minimal body with parameter `key` set to `value`.
    fn body_with(key: &str, value: Value) -> Value {
        let mut body = minimal();
        let _replaced: Option<Value> = body
            .as_object_mut()
            .and_then(|map| map.insert(key.to_owned(), value));
        body
    }

    /// The 422 message `value` is rejected with.
    fn rejection(value: Value) -> String {
        new_filter(value)
            .expect_err("the fixture is invalid")
            .to_string()
    }

    #[test]
    fn a_minimal_body_applies_every_default() {
        let new = new_filter(minimal()).expect("the minimal body validates");
        assert_eq!(new.phrase, "foo", "the phrase is kept verbatim");
        assert_eq!(new.context, ["home"], "the context list goes through");
        assert!(new.whole_word, "whole word defaults to true");
        assert!(!new.irreversible, "irreversible defaults to false");
        assert!(new.expires_at.is_none(), "and absent means never");
    }

    #[test]
    fn a_lone_context_string_is_the_one_element_list() {
        let new = new_filter(body_with("context", json!("account")))
            .expect("a string context is accepted");
        assert_eq!(new.context, ["account"], "as a one-element list");
    }

    #[test]
    fn flags_and_expiry_are_read_in_their_client_spellings() {
        let new = new_filter(json!({
            "phrase": "x",
            "context": ["public", "thread"],
            "irreversible": "true",
            "whole_word": "0",
            "expires_in": "3600",
        }))
        .expect("form spellings deserialize");
        assert!(new.irreversible, "the string `true`");
        assert!(!new.whole_word, "and the string `0`");
        assert_eq!(
            new.context,
            ["public", "thread"],
            "the list keeps its order"
        );
        let after = new.expires_at.expect("a positive expires_in expires");
        let delta = after - Utc::now();
        assert!(
            delta > Duration::seconds(3595) && delta < Duration::seconds(3605),
            "roughly an hour ahead, got {delta}"
        );
    }

    #[test]
    fn a_null_or_nonpositive_expiry_means_never() {
        for value in [json!(null), json!(0), json!(-5), json!(i64::MAX)] {
            let new = new_filter(body_with("expires_in", value.clone()))
                .unwrap_or_else(|err| panic!("`{value}` is accepted as never: {err}"));
            assert!(new.expires_at.is_none(), "`{value}` never expires");
        }
    }

    #[test]
    fn a_blank_or_absent_phrase_is_rejected_with_mastodon_wording() {
        for value in [json!(null), json!(""), json!("   ")] {
            assert_eq!(
                rejection(body_with("phrase", value.clone())),
                PHRASE_BLANK,
                "`{value}` is a blank phrase"
            );
        }
        let mut bare = Map::new();
        let _key: Option<Value> = bare.insert("context".to_owned(), json!(["home"]));
        assert_eq!(
            rejection(Value::Object(bare)),
            PHRASE_BLANK,
            "absent is as blank as empty"
        );
    }

    #[test]
    fn a_phrase_of_exactly_the_maximum_length_passes() {
        let edge = "x".repeat(MAX_PHRASE_CHARS);
        let new = new_filter(body_with("phrase", Value::String(edge.clone())))
            .expect("200 characters fit");
        assert_eq!(new.phrase, edge, "verbatim, at the edge");

        let over = "x".repeat(MAX_PHRASE_CHARS + 1);
        assert_eq!(
            rejection(body_with("phrase", Value::String(over))),
            "Validation failed: Phrase is too long (maximum is 200 characters)",
            "201 do not"
        );
    }

    #[test]
    fn a_non_ascii_phrase_is_counted_in_characters() {
        let dots = "…".repeat(MAX_PHRASE_CHARS);
        assert!(
            new_filter(body_with("phrase", Value::String(dots))).is_ok(),
            "200 three-byte characters fit"
        );
    }

    #[test]
    fn a_bad_context_is_rejected_with_mastodon_wording() {
        for value in [
            json!(null),
            json!([]),
            json!(""),
            json!("bogus"),
            json!(["home", "bogus"]),
            json!(["Home"]),
            json!([" home"]),
        ] {
            assert_eq!(
                rejection(body_with("context", value.clone())),
                CONTEXT_INVALID,
                "`{value}` is not a valid context"
            );
        }
        let mut bare = Map::new();
        let _key: Option<Value> = bare.insert("phrase".to_owned(), json!("x"));
        assert_eq!(
            rejection(Value::Object(bare)),
            CONTEXT_INVALID,
            "absent is invalid too"
        );
    }

    #[test]
    fn every_known_context_name_passes() {
        for name in VALID_CONTEXTS {
            let new = new_filter(body_with("context", json!([name])))
                .unwrap_or_else(|err| panic!("`{name}` is valid: {err}"));
            assert_eq!(new.context, [name.to_owned()], "`{name}` goes through");
        }
    }

    #[test]
    fn a_flag_that_is_not_a_boolean_is_a_parameter_error() {
        let err = crate::extract::into_params::<FilterParams>(json!({
            "phrase": "x",
            "context": ["home"],
            "irreversible": "maybe",
        }))
        .expect_err("maybe is not a boolean");
        assert!(
            err.to_string().contains("expected a boolean"),
            "and the flex message says so: {err}"
        );
    }

    #[test]
    fn expiry_overflow_cannot_panic_and_reads_as_never() {
        // `i64::MAX` seconds does not fit a `chrono` span; the endpoint
        // treats that as "never", not as a crash or an error.
        assert!(
            expires_at_from(Some(i64::MAX)).is_none(),
            "never, not a panic"
        );
        assert!(expires_at_from(Some(1)).is_some(), "one second is fine");
    }
}
