//! The account listings and the single-account read.

use axum::{
    Json,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, Uri},
};
use tracing::debug;

use super::{
    AREA_ACCOUNTS, CLASS_READ, DEFAULT_LIMIT, MAX_LIMIT, ORIGIN_LOCAL, ORIGIN_REMOTE,
    PERMISSIONS_STAFF, STATUS_ACTIVE, STATUS_DISABLED, STATUS_PENDING,
};
use crate::{
    api::helpers::{QueryPairs, link_query},
    auth::admin::AdminUser,
    entities::mastodon_admin::AdminAccount,
    error::{AppError, AppResult},
    models::user::{
        self, User,
        admin::{self, Criterion, MastodonListParams},
    },
    pagination::{PageParams, keyset_link, set_link_header},
    state::AppState,
    util::id::parse_id,
};

/// `GET /api/v1/admin/accounts` — Pleroma's `index`: the page of accounts the
/// request's boolean and text criteria select.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::BadRequest`] for a malformed
/// pagination cursor; [`AppError::Database`] from the listing read; and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`AppError::Internal`].
pub(super) async fn list_accounts(
    State(state): State<AppState>,
    admin: AdminUser,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<AdminAccount>>)> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    page_response(&state, &uri, v1_params(uri.query())).await
}

/// `GET /api/v2/admin/accounts` — Pleroma's `index2`: the same entities on the
/// same page, its criteria spelled as `origin`, `status` and `permissions`
/// words.
///
/// # Errors
///
/// Exactly those of [`list_accounts`].
pub(super) async fn list_accounts_v2(
    State(state): State<AppState>,
    admin: AdminUser,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<AdminAccount>>)> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    page_response(&state, &uri, v2_params(uri.query())).await
}

/// `GET /api/v1/admin/accounts/{id}` — Pleroma's `show`: the one entity the id
/// names, a deactivated account and a mirror included.
///
/// `{id}` is a row id and nothing else (Pleroma's `assign_account_by_id` reads
/// it the same way): text that is no id names no account, so a malformed id
/// and an unknown one are the same 404 `{"error":"Record not found"}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::NotFound`] as described;
/// [`AppError::Database`] from the lookup.
pub(super) async fn show_account(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    debug!(
        account = %account.id,
        "admin account shown"
    );
    Ok(Json(AdminAccount::from_user(&account, &state.config)))
}

/// The account one `/admin/accounts/{id}` route names, by row id and nothing
/// else — [`show_account`]'s lookup, which all five writes share.
///
/// No visibility rule on it: an admin moderates a deactivated account and
/// the instance actor exactly as an active one's, which is what
/// [`helpers::account_or_404`](crate::api::helpers::account_or_404) refuses
/// to do for the client API.
///
/// # Errors
///
/// [`AppError::NotFound`] for text that is no id and for an id that names no
/// row; [`AppError::Database`] from the read.
pub(super) async fn named_account(state: &AppState, raw: &str) -> AppResult<User> {
    let Some(id) = parse_id(raw) else {
        return Err(AppError::NotFound);
    };
    user::get_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)
}

/// The shared answer of the two listings: one fetched page of
/// [`AdminAccount`] entities under the `Link` header its cursors belong to.
///
/// # Errors
///
/// [`AppError::BadRequest`] for a malformed cursor, [`AppError::Database`]
/// from the read and [`AppError::Internal`] from a `Link` value that is not
/// header-safe.
async fn page_response(
    state: &AppState,
    uri: &Uri,
    params: MastodonListParams,
) -> AppResult<(HeaderMap, Json<Vec<AdminAccount>>)> {
    let page = PageParams::from_query_with_default(uri.query(), DEFAULT_LIMIT, MAX_LIMIT)?;
    let rows = admin::mastodon_list(&state.db, &params, page).await?;
    debug!(listed = rows.len(), "admin accounts listed");
    let link = keyset_link(
        &state.config.base_url,
        uri.path(),
        &link_query(uri.query()),
        &rows,
        |row: &User| row.id,
    );
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let accounts = rows
        .iter()
        .map(|row| AdminAccount::from_user(row, &state.config))
        .collect();
    Ok((headers, Json(accounts)))
}

/// v1's criteria: the six booleans, each read by [`Criterion::parse`] so that
/// a `false` asks for the opposite set, and the four text criteria.
fn v1_params(query: Option<&str>) -> MastodonListParams {
    let pairs = QueryPairs::parse(query);
    MastodonListParams {
        local: Criterion::parse(pairs.value("local").as_deref()),
        remote: Criterion::parse(pairs.value("remote").as_deref()),
        active: Criterion::parse(pairs.value("active").as_deref()),
        pending: Criterion::parse(pairs.value("pending").as_deref()),
        disabled: Criterion::parse(pairs.value("disabled").as_deref()),
        staff: Criterion::parse(pairs.value("staff").as_deref()),
        ..text_params(&pairs)
    }
}

/// v2's criteria: the same six facts spelled as three word parameters —
/// `origin`, `status` and `permissions` — and the four text criteria. The two
/// `status` words with no column behind them (`silenced` and `suspended`) fall
/// through [`word_criterion`] without matching and so filter nothing, which is
/// how the ticket has them treated.
fn v2_params(query: Option<&str>) -> MastodonListParams {
    let pairs = QueryPairs::parse(query);
    let origin = pairs.value("origin");
    let status = pairs.value("status");
    let permissions = pairs.value("permissions");
    MastodonListParams {
        local: word_criterion(origin.as_deref(), ORIGIN_LOCAL),
        remote: word_criterion(origin.as_deref(), ORIGIN_REMOTE),
        active: word_criterion(status.as_deref(), STATUS_ACTIVE),
        pending: word_criterion(status.as_deref(), STATUS_PENDING),
        disabled: word_criterion(status.as_deref(), STATUS_DISABLED),
        staff: word_criterion(permissions.as_deref(), PERMISSIONS_STAFF),
        ..text_params(&pairs)
    }
}

/// The four text criteria both versions state identically, and no boolean
/// criterion at all: the caller fills those six in.
fn text_params(pairs: &QueryPairs) -> MastodonListParams {
    MastodonListParams {
        username: pairs.value("username"),
        display_name: pairs.value("display_name"),
        domain: pairs.value("by_domain"),
        email: pairs.value("email"),
        ..MastodonListParams::default()
    }
}

/// The criterion one of v2's word-valued parameters states:
/// [`Only`](Criterion::Only) when the request's value is exactly `wanted` —
/// Pleroma's `%{status: "active"}` clauses match their literals, so the words
/// are case-sensitive and `origin=Local` asks for nothing — and
/// [`Any`](Criterion::Any) for anything else: absent, empty, or a word this
/// listing does not weigh.
fn word_criterion(text: Option<&str>, wanted: &str) -> Criterion {
    if text == Some(wanted) {
        Criterion::Only
    } else {
        Criterion::Any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The criteria a request states through no parameter at all: nothing is
    /// filtered, on either axis, in either version.
    #[test]
    fn an_absent_query_states_no_criterion() {
        for raw in [None, Some("")] {
            for params in [v1_params(raw), v2_params(raw)] {
                assert_eq!(params.local, Criterion::Any, "no origin filter");
                assert_eq!(params.remote, Criterion::Any, "and its opposite");
                assert_eq!(params.active, Criterion::Any, "no state filter");
                assert_eq!(params.pending, Criterion::Any, "no approval filter");
                assert_eq!(params.disabled, Criterion::Any, "no activation filter");
                assert_eq!(params.staff, Criterion::Any, "no role filter");
                assert!(
                    params.username.is_none()
                        && params.display_name.is_none()
                        && params.domain.is_none()
                        && params.email.is_none(),
                    "and no text criterion"
                );
            }
        }
    }

    /// v1's booleans, read in both directions: `true` weighs its own columns
    /// and `false` weighs their negation — Pleroma's
    /// `maybe_filter_local(…, %{local: false})` arm and its `active: false`
    /// clause — while an unknown spelling flips nothing.
    #[test]
    fn v1_reads_its_booleans_in_both_directions() {
        let only = v1_params(Some(
            "local=true&active=true&pending=true&disabled=true&staff=true",
        ));
        assert_eq!(only.local, Criterion::Only, "local=true is the locals");
        assert_eq!(
            only.active,
            Criterion::Only,
            "active=true is the fully green set"
        );
        assert_eq!(only.pending, Criterion::Only, "pending=true is the queue");
        assert_eq!(
            only.disabled,
            Criterion::Only,
            "disabled=true is the inactive set"
        );
        assert_eq!(only.staff, Criterion::Only, "staff=true is the two roles");

        let except = v1_params(Some(
            "local=false&remote=false&active=false&pending=false&disabled=false&staff=false",
        ));
        assert_eq!(
            except.local,
            Criterion::Except,
            "local=false is Pleroma's external criterion"
        );
        assert_eq!(
            except.remote,
            Criterion::Except,
            "and remote=false the locals"
        );
        assert_eq!(
            except.active,
            Criterion::Except,
            "active=false is anything not fully green"
        );
        assert_eq!(
            except.pending,
            Criterion::Except,
            "pending=false is the approved set"
        );
        assert_eq!(
            except.disabled,
            Criterion::Except,
            "disabled=false is active"
        );
        assert_eq!(
            except.staff,
            Criterion::Except,
            "staff=false is nobody's role"
        );

        let mixed = v1_params(Some("active=maybe&staff&pending=TRUE"));
        assert_eq!(
            mixed.pending,
            Criterion::Only,
            "case does not matter to a known spelling"
        );
        assert_eq!(
            mixed.active,
            Criterion::Any,
            "`maybe` is no boolean this listing weighs"
        );
        assert_eq!(
            v1_params(Some("local=yes&remote=no")).local,
            Criterion::Only,
            "`yes` is a true spelling"
        );
        assert_eq!(mixed.staff, Criterion::Any, "and a bare key is no value");
    }

    /// v2's words select their own criterion and leave every other one alone,
    /// so `status=pending` is `pending=true` rather than a state machine
    /// shared with `active`, and the two statuses with no column behind them
    /// state nothing at all.
    #[test]
    fn v2_reads_its_words_one_at_a_time() {
        let origin = v2_params(Some("origin=local"));
        assert_eq!(origin.local, Criterion::Only, "origin=local is the locals");
        assert_eq!(origin.remote, Criterion::Any, "and nothing else");

        let remote = v2_params(Some("origin=remote"));
        assert_eq!(
            remote.remote,
            Criterion::Only,
            "origin=remote is the mirrors"
        );
        assert_eq!(remote.local, Criterion::Any, "which is not `local` unset");

        let active = v2_params(Some("status=active"));
        assert_eq!(active.active, Criterion::Only, "status=active");
        let pending = v2_params(Some("status=pending"));
        assert_eq!(pending.pending, Criterion::Only, "status=pending");
        let disabled = v2_params(Some("status=disabled"));
        assert_eq!(disabled.disabled, Criterion::Only, "status=disabled");
        for status in ["active", "pending", "disabled"] {
            let params = v2_params(Some(&format!("status={status}")));
            let stated = [params.active, params.pending, params.disabled]
                .into_iter()
                .filter(|criterion| *criterion == Criterion::Only)
                .count();
            assert_eq!(stated, 1, "`status={status}` states exactly one criterion");
        }

        let unsupported = v2_params(Some("status=silenced&suspended=1"));
        assert_eq!(
            unsupported.active,
            Criterion::Any,
            "a silenced account is no criterion here"
        );
        assert_eq!(
            unsupported.disabled,
            Criterion::Any,
            "and neither is a suspended one"
        );
        assert_eq!(
            v2_params(Some("status=Active")).active,
            Criterion::Any,
            "the words are case-sensitive, Pleroma's clause for clause"
        );
        assert_eq!(
            v2_params(Some("permissions=staff")).staff,
            Criterion::Only,
            "permissions=staff is the staff criterion"
        );
        assert_eq!(
            v2_params(Some("permissions=admin")).staff,
            Criterion::Any,
            "any other permission level filters nothing"
        );
    }

    /// The four text criteria are the same parameters in both versions, and an
    /// empty one is nothing to filter on.
    #[test]
    fn both_versions_read_the_same_text_criteria() {
        let stated = "username=Ali&display_name=Alice&by_domain=remote.example&email=a@b.c";
        for params in [v1_params(Some(stated)), v2_params(Some(stated))] {
            assert_eq!(params.username.as_deref(), Some("Ali"), "the handle");
            assert_eq!(params.display_name.as_deref(), Some("Alice"), "the name");
            assert_eq!(
                params.domain.as_deref(),
                Some("remote.example"),
                "the mirrored host"
            );
            assert_eq!(params.email.as_deref(), Some("a@b.c"), "the address");
        }
        let empty = v1_params(Some("username=&email=&by_domain=&display_name="));
        assert!(
            empty.username.is_none()
                && empty.email.is_none()
                && empty.domain.is_none()
                && empty.display_name.is_none(),
            "an empty parameter filters nothing, in either spelling"
        );
    }

    /// A repeated criterion keeps its last value, the way a query-parameter
    /// map folds duplicates.
    #[test]
    fn a_repeated_criterion_keeps_its_last_value() {
        let params = v1_params(Some("staff=true&staff=false"));
        assert_eq!(params.staff, Criterion::Except, "the last occurrence wins");
    }

    /// The page window is Mastodon's admin pair rather than Pleroma's global
    /// one: a hundred rows when the request states no size, two hundred at
    /// most when it asks for more, and an unusable `limit` back to the
    /// default.
    #[test]
    fn the_page_window_defaults_to_a_hundred_and_caps_at_two() {
        let absent = PageParams::from_query_with_default(None, DEFAULT_LIMIT, MAX_LIMIT)
            .expect("an absent query is a first page");
        assert_eq!(absent.limit, DEFAULT_LIMIT, "the default page");
        for (query, want) in [("limit=7", 7), ("limit=150", 150), ("limit=999", MAX_LIMIT)] {
            let params = PageParams::from_query_with_default(Some(query), DEFAULT_LIMIT, MAX_LIMIT)
                .expect("a limit is parsable");
            assert_eq!(params.limit, want, "`{query}` sizes the page");
        }
        let junk =
            PageParams::from_query_with_default(Some("limit=nonsense"), DEFAULT_LIMIT, MAX_LIMIT)
                .expect("an unusable limit is no limit");
        assert_eq!(junk.limit, DEFAULT_LIMIT, "and falls back to the default");
    }

    /// The cursor half of the request still refuses a malformed id, the way
    /// every other keyset listing here does: an ignored *filter* is lenient,
    /// an unreadable *cursor* is a bad request.
    #[test]
    fn a_malformed_cursor_is_still_a_bad_request() {
        let err =
            PageParams::from_query_with_default(Some("max_id=nope"), DEFAULT_LIMIT, MAX_LIMIT)
                .expect_err("a broken cursor cannot page");
        assert_eq!(err.to_string(), "Invalid id", "the shared wording");
    }
}
