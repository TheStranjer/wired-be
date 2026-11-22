//! The report routes' request parameters and the checks run on them: the
//! comment cap, the category set and the cited status and rule ids.

use serde::Deserialize;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::{instance_rule, report::Category, status, status_visibility, user::User},
    state::AppState,
    util::id::parse_id,
};

/// 422 body for a cited status that is not an id, not live, not the target's,
/// or not visible to the reporter (the ticket's wording, where Pleroma's
/// `visibility_error` answers a 404 "Record not found").
const INVALID_STATUS_IDS: &str = "Invalid status_ids";
/// 422 body for a cited rule that names no rule of this instance.
const INVALID_RULE_IDS: &str = "Invalid rule_ids";
/// 422 body for a `category` outside the four the column allows (Mastodon's
/// changeset wording, the precedent [`crate::api::filters`] keeps).
const CATEGORY_INVALID: &str = "Validation failed: Category is not included in the list";

/// The request parameters of `POST /api/v1/reports`.
///
/// Everything but [`account_id`](Self::account_id) is `Option` at the serde
/// layer so the endpoint can apply the documented default or answer the
/// ticket's wording; the id arrays are [`IdValues`], which takes the JSON
/// array, the form's `status_ids[]=…` keys and a lone value alike.
#[derive(Debug, Deserialize)]
pub(super) struct ReportParams {
    /// The account to report; required, and a missing one is the extractor's
    /// 422.
    pub(super) account_id: String,
    /// The statuses to cite for context.
    #[serde(default)]
    pub(super) status_ids: Option<IdValues>,
    /// The reporter's own words; empty when it says none.
    #[serde(default)]
    pub(super) comment: Option<String>,
    /// Whether to forward the report to the target's instance; `false` when
    /// absent (Pleroma's `Map.get(data, :forward, false)`).
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    pub(super) forward: Option<bool>,
    /// Why it is reporting; `other` when absent.
    #[serde(default)]
    pub(super) category: Option<String>,
    /// The instance rules it is citing.
    #[serde(default)]
    pub(super) rule_ids: Option<IdValues>,
}

/// The request parameters of `POST /api/v1/accounts/{id}/flag`: everything
/// [`ReportParams`] carries except the target, which rides in the path.
///
/// [`forward`](Self::forward) is read here but defaulted the other way
/// round — the flag route assumes forwarding is why it was called — and
/// every other default matches the collection-shaped filing.
#[derive(Debug, Deserialize)]
pub(super) struct FlagParams {
    /// The statuses to cite for context.
    #[serde(default)]
    pub(super) status_ids: Option<IdValues>,
    /// The reporter's own words; empty when it says none.
    #[serde(default)]
    pub(super) comment: Option<String>,
    /// Whether to forward the flag to the target's instance; `true` when
    /// absent, the opposite of the `POST /api/v1/reports` default.
    #[serde(default, deserialize_with = "crate::extract::flex::opt_bool")]
    pub(super) forward: Option<bool>,
    /// Why it is reporting; `other` when absent.
    #[serde(default)]
    pub(super) category: Option<String>,
    /// The instance rules it is citing.
    #[serde(default)]
    pub(super) rule_ids: Option<IdValues>,
}

/// One of the id-array parameters as clients send it: a list (a JSON array or
/// repeated `key[]=…` form keys) or a lone value, which Pleroma's
/// `List.wrap` accepts too.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum IdValues {
    /// A lone id.
    Single(String),
    /// A list of ids.
    Multiple(Vec<String>),
}

impl IdValues {
    /// The values as a list; a lone string becomes the one-element list.
    fn into_list(self) -> Vec<String> {
        match self {
            Self::Single(text) => vec![text],
            Self::Multiple(values) => values,
        }
    }
}

/// The `comment`, checked: absent is empty, and more than
/// `max_report_comment` characters is 422 with Mastodon's limit wording. A
/// kept comment is stored verbatim.
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
pub(super) fn validated_comment(
    max_report_comment: usize,
    comment: Option<&str>,
) -> AppResult<String> {
    let text = comment.unwrap_or_default();
    if text.chars().count() > max_report_comment {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Comment is too long (maximum is {max_report_comment} characters)"
        )));
    }
    Ok(text.to_owned())
}

/// The `category`, checked: absent is [`Category::Other`] (the column's
/// default), anything the enum does not name is 422 [`CATEGORY_INVALID`].
///
/// # Errors
///
/// [`AppError::Unprocessable`] as described above.
pub(super) fn validated_category(category: Option<&str>) -> AppResult<Category> {
    category.map_or_else(
        || Ok(Category::Other),
        |text| {
            text.parse()
                .map_err(|_unknown| AppError::Unprocessable(CATEGORY_INVALID.to_owned()))
        },
    )
}

/// The values of one id-array parameter as ids, in the order they were sent
/// and without repeats: an entry that is not id text is 422 `refusal`, which
/// is how the endpoint answers a malformed id the same way it answers one
/// that names nothing.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `refusal` as its message, for an entry
/// that is not id text.
fn id_list(values: Option<IdValues>, refusal: &str) -> AppResult<Vec<Uuid>> {
    let mut ids: Vec<Uuid> = Vec::new();
    for value in values.map_or_else(Vec::new, IdValues::into_list) {
        let Some(id) = parse_id(&value) else {
            return Err(AppError::Unprocessable(refusal.to_owned()));
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// The statuses a report may cite, checked: every one of them a live status
/// [`status`] gives `reporter` leave to see and whose author is `target`.
///
/// The authorship check is Pleroma's `get_report_statuses` (the cited posts
/// must belong to the reported account) and the visibility check its
/// `check_statuses_visibility`; the two are one answer here, because telling
/// a reporter that somebody's private post exists is not something either
/// endpoint does. An empty or absent array is the empty list and nothing to
/// check.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_STATUS_IDS`] for an entry that
/// is not id text or fails either check; any database failure arrives through
/// `?` as [`AppError::Database`].
pub(super) async fn cited_status_ids(
    state: &AppState,
    reporter: &User,
    target: &User,
    values: Option<IdValues>,
) -> AppResult<Vec<Uuid>> {
    let requested = id_list(values, INVALID_STATUS_IDS)?;
    if requested.is_empty() {
        return Ok(requested);
    }
    let authored: Vec<status::Status> = status::get_many(&state.db, &requested)
        .await?
        .into_iter()
        .filter(|row| row.user_id == target.id)
        .collect();
    let visible = status_visibility::filter_visible(&state.db, authored, Some(reporter)).await?;
    if visible.len() != requested.len() {
        return Err(AppError::Unprocessable(INVALID_STATUS_IDS.to_owned()));
    }
    Ok(requested)
}

/// The rules a report may cite, checked: every one of them an id this
/// instance's [`instance_rule::list`] holds. An empty or absent array is the
/// empty list and nothing to check.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`INVALID_RULE_IDS`] for an entry that is
/// not id text or names no rule; any database failure arrives through `?` as
/// [`AppError::Database`].
pub(super) async fn cited_rule_ids(
    state: &AppState,
    values: Option<IdValues>,
) -> AppResult<Vec<Uuid>> {
    let requested = id_list(values, INVALID_RULE_IDS)?;
    if requested.is_empty() {
        return Ok(requested);
    }
    let rules = instance_rule::list(&state.db).await?;
    let known: Vec<Uuid> = rules.iter().map(|rule| rule.id).collect();
    if requested.iter().all(|id| known.contains(id)) {
        Ok(requested)
    } else {
        Err(AppError::Unprocessable(INVALID_RULE_IDS.to_owned()))
    }
}

#[cfg(test)]
mod tests;
