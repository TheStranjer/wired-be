//! The account-listing endpoints: the profile directory, the staff
//! suggestions and their dismissal, the familiar-followers batch and the
//! birthday list (`docs/ARCHITECTURE.md` decision 1).
//!
//! Four listings that share one shape — a query over `users`, answered as
//! [`Account`] entities — and one that is a write, gathered into this module
//! because every one of them answers "which accounts?" to a question the
//! client asks in its query string:
//!
//! - `GET /api/v1/directory` is Pleroma's `DirectoryController.index`
//!   (`lib/pleroma/web/mastodon_api/controllers/directory_controller.ex`):
//!   **no auth required** ([`MaybeAuthUser`], so a presented token only
//!   decides *whose view* the accounts render as, and an invalid one is
//!   still the extractor's 401). It lists the active accounts that opted
//!   into discovery ([`discoverable`](crate::models::user::User::discoverable)),
//!   paged by `offset` (default `0`) and `limit` (default `LIST_LIMIT` of
//!   `40`, at most `LIST_CAP` of `80`), ordered by `order`: `active` (the
//!   default) sorts on newest `last_status_at` with the never-posted
//!   accounts last, `new` on newest account; any other spelling is the
//!   default, as Pleroma's exact
//!   match on `"new"` makes it. `local` is accepted and honoured the way
//!   Pleroma's `exclude_remote` is: a true spelling (`true`, `1`, `yes`, …)
//!   restricts the listing to
//!   accounts this instance owns, anything else — the default — leaves the
//!   remote mirrors in.
//! - `GET /api/v1/suggestions` and `GET /api/v2/suggestions` are Pleroma's
//!   `SuggestionController`'s `index`/`index2`
//!   (`lib/pleroma/web/mastodon_api/controllers/suggestion_controller.ex`):
//!   [`AuthUser`] plus the `read` scope, the same `limit`, and the pool of
//!   accounts an admin flagged with `users.is_suggested`
//!   ([`crate::models::suggestion`]) — local, active, and none of the
//!   accounts the viewer is themself, follows, blocks, mutes or has
//!   dismissed. v1 answers the [`Account`] entities; v2 wraps each in
//!   [`SuggestedAccount`] with its source, and every suggestion here comes
//!   from the staff pool because nothing else curates one.
//! - `DELETE /api/v1/suggestions/{account_id}` is that controller's
//!   `dismiss`: [`AuthUser`] plus `write:accounts`, the account resolved by
//!   [`account_or_404`], and the answer is the empty JSON object `{}`.
//!   [`dismiss`](crate::models::suggestion::dismiss) records the pair
//!   idempotently and clears nothing about the account: the flag stays set
//!   for every other viewer, only this one stops seeing it.
//! - `GET /api/v1/accounts/familiar_followers` is Pleroma's
//!   `AccountController.familiar_followers` over
//!   `User.get_familiar_followers`: [`AuthUser`] plus `read:follows`, Mastodon's
//!   `id[]` array (a lone `id=` is read too, Pleroma's `List.wrap`), and one
//!   [`FamiliarFollowers`] entry per *known* id in the order requested —
//!   values that are not `UUID` text and ids that name no stored account are
//!   skipped, as the relationships batch skips them. An entry's accounts are
//!   the viewer's follows who follow that account, at most
//!   `FAMILIAR_FOLLOWERS_LIMIT` (`20`) of them, newest follow first. Two privacy
//!   rules come from Pleroma's controller and query: a target who hides
//!   their followers ([`hide_followers`](crate::models::user::User::hide_followers))
//!   answers the empty set to everyone but themself, and an account that
//!   hides its follows never appears as a familiar follower.
//! - `GET /api/v1/pleroma/birthdays` is Pleroma's
//!   `PleromaAPI.AccountController.birthdays`
//!   (`lib/pleroma/web/pleroma_api/controllers/account_controller.ex`):
//!   [`AuthUser`] plus `read:accounts`, and the accounts whose birthday the
//!   day names — the viewer and the accounts of their accepted follows, each
//!   with [`show_birthday`](crate::models::user::User::show_birthday) set,
//!   newest first. `month` and `day` override the UTC calendar parts they
//!   stand for, independently, so `?day=1` asks for the first of this month;
//!   one that is not a number, or not a month or a day, is 400.
//!
//! None of these listings carries Pleroma's `Link` header: the directory
//! pages by `offset` over a set with no cursor to name, and the other three
//! are capped pages, not keyset walks.

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::Uri,
    routing::{delete, get},
};
use chrono::{Datelike as _, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::{QueryPairs, account_or_404, is_true, requested_ids},
    auth::{AuthUser, MaybeAuthUser},
    entities::account::{Account, AccountViewer},
    error::{AppError, AppResult},
    models::{follow, suggestion, user, user::User},
    oauth::scopes::{READ, READ_ACCOUNTS, READ_FOLLOWS, WRITE_ACCOUNTS},
    pagination::PageParams,
    state::AppState,
};

/// Default page size of the listings this module answers (`40`, what the
/// ticket pins; [`PageParams`]'s own default of `20` is Pleroma's global
/// one, which the relationship listings keep).
const LIST_LIMIT: i64 = 40;

/// Cap of [`LIST_LIMIT`]: the most either suggestions or the directory will
/// answer at once (Pleroma's `min(80)`).
const LIST_CAP: i64 = 80;

/// The most familiar followers one entry of the batch answers with. Pleroma
/// runs the query uncapped; this endpoint's ticket caps it, because the set
/// is a hint a client renders inline and not a list it pages through.
const FAMILIAR_FOLLOWERS_LIMIT: i64 = 20;

/// The only source a suggestion here can have: the staff pool
/// (`users.is_suggested`). Mastodon's v2 entity also spells `global` and
/// `past_interactions`, neither of which this server computes.
const SOURCE_STAFF: &str = "staff";

/// 400 body prefix for a query parameter this module cannot read:
/// `"Invalid parameter: day"`, the wording the chat endpoints pin for the
/// same failure.
const INVALID_PARAMETER: &str = "Invalid parameter: ";

/// `month` of a birthday query: the lowest and highest calendar months.
const MONTH_RANGE: (i32, i32) = (1, 12);
/// `day` of a birthday query: the widest day-of-month any month has. A day
/// a given month never reaches (the 31st of April) simply matches nothing,
/// and a `29` February is the leap day and no other.
const DAY_RANGE: (i32, i32) = (1, 31);
/// The month [`today_in_utc`](today_parts) falls back to if `chrono` ever
/// reports one that does not fit an `int4`: unreachable, and no `unwrap`
/// (decision 14) to find out.
const JANUARY: i32 = 1;
/// [`JANUARY`]’s counterpart for the day of the month.
const FIRST: i32 = 1;

/// The `/api/v1` account-listing router, merged into [`crate::api::v1`].
///
/// `birthdays` lives under the `/pleroma` prefix because that is where
/// Pleroma routes it (`lib/pleroma/web/router.ex`), the same way the
/// endorsements area contributes its `/pleroma` counterpart.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/directory", get(directory))
        .route("/suggestions", get(suggestions))
        .route("/suggestions/{account_id}", delete(dismiss))
        .route("/accounts/familiar_followers", get(familiar_followers))
        .route("/pleroma/birthdays", get(birthdays))
}

/// The `/api/v2` side of the account-listing router, merged into
/// [`crate::api::v2`]: the suggestions listing with Mastodon's v2 shape,
/// every entry carrying the source it came from.
pub fn v2_router() -> Router<AppState> {
    Router::new().route("/suggestions", get(suggestions_v2))
}

/// One entry of `GET /api/v2/suggestions`: an [`Account`] and where the
/// suggestion came from.
///
/// Mastodon's v2 suggestion entity names both a single `source` and a
/// `sources` list (the list is the newer spelling, kept alongside for
/// clients still reading the scalar); a suggestion this server makes has one
/// source either way — `SOURCE_STAFF`, i.e. `"staff"`.
#[derive(Debug, Serialize)]
pub struct SuggestedAccount {
    /// The one source of this suggestion: `"staff"`.
    pub source: &'static str,
    /// [`source`](Self::source) as Mastodon v2's list form: one element,
    /// the same value.
    pub sources: [&'static str; 1],
    /// The suggested account, rendered for the viewer whose pool this is.
    pub account: Account,
}

/// One entry of `GET /api/v1/accounts/familiar_followers`: the account that
/// was asked about and the viewer's follows who follow it.
#[derive(Debug, Serialize)]
pub struct FamiliarFollowers {
    /// The requested account's id, as clients know it (decision 4).
    pub id: String,
    /// The accounts the viewer follows that follow this one, newest follow
    /// first, at most `FAMILIAR_FOLLOWERS_LIMIT` (`20`) of them; empty when
    /// the target hides its followers and the viewer is not the target.
    pub accounts: Vec<Account>,
}

/// `GET /api/v1/directory` — the discoverable accounts of the instance,
/// `local`-filtered and `order`-sorted, one `offset` page at a time.
///
/// No auth is required, and none is *refused*: the viewer behind a
/// presented token is the account the entities render for (Pleroma renders
/// `for: user`), which changes only what that account is allowed to see of
/// each profile — hidden counters, role flags, birthdays — and never who is
/// in the listing. The ticket's directory has no per-viewer exclusions, so
/// unlike Pleroma's authenticated branch this one neither drops the viewer
/// from the page nor filters out accounts they block or mute.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`MaybeAuthUser`] for an invalid,
/// expired or deactivated token; [`AppError::BadRequest`] from
/// [`PageParams`] for a malformed pagination cursor. Any database failure
/// arrives through `?` as [`AppError::Database`].
async fn directory(
    State(state): State<AppState>,
    MaybeAuthUser(viewer, _): MaybeAuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Account>>> {
    let filters = DirectoryFilters::parse(uri.query());
    let page = PageParams::from_query_with_default(uri.query(), LIST_LIMIT, LIST_CAP)?;
    let accounts = user::directory_page(&state.db, filters.order, filters.local_only, page).await?;
    debug!(
        count = accounts.len(),
        local_only = filters.local_only,
        "directory listed"
    );
    Ok(Json(render_accounts(&state, viewer.as_ref(), &accounts)))
}

/// `GET /api/v1/suggestions` — the staff pool as the caller sees it, in the
/// plain [`Account`] shape v1 answers.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read` check. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`PageParams`] adds
/// [`AppError::BadRequest`] for a malformed cursor.
async fn suggestions(
    State(state): State<AppState>,
    auth: AuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Account>>> {
    let pool = suggested_pool(&state, &auth, uri.query()).await?;
    Ok(Json(render_accounts(&state, Some(&auth.0), &pool)))
}

/// `GET /api/v2/suggestions` — the same pool, each entry wrapped in its
/// [`SuggestedAccount`] source object.
///
/// # Errors
///
/// Exactly those of [`suggestions`].
async fn suggestions_v2(
    State(state): State<AppState>,
    auth: AuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<SuggestedAccount>>> {
    let pool = suggested_pool(&state, &auth, uri.query()).await?;
    let accounts = pool
        .iter()
        .map(|account| SuggestedAccount {
            source: SOURCE_STAFF,
            sources: [SOURCE_STAFF],
            account: Account::from_user_for(Some(&auth.0), account, &state.config, None),
        })
        .collect();
    Ok(Json(accounts))
}

/// The pool both suggestion versions answer: the `read` scope is Pleroma's
/// plug for the two actions, and the limit is its `Map.get(params, :limit,
/// 40) |> min(80)`.
///
/// # Errors
///
/// [`AppError::Forbidden`] when the token does not grant `read`;
/// [`AppError::BadRequest`] from [`PageParams`] for a malformed cursor. Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn suggested_pool(
    state: &AppState,
    auth: &AuthUser,
    query: Option<&str>,
) -> AppResult<Vec<User>> {
    auth.require(READ)?;
    let page = PageParams::from_query_with_default(query, LIST_LIMIT, LIST_CAP)?;
    let pool = suggestion::suggested_for(&state.db, auth.0.id, page.limit).await?;
    debug!(
        count = pool.len(),
        user = %auth.0.id,
        "suggestions listed"
    );
    Ok(pool)
}

/// `DELETE /api/v1/suggestions/{account_id}` — the caller stops seeing this
/// account in their suggestions; the answer is `{}`.
///
/// The dismissal is the caller's own and idempotent: an account that was
/// never suggested, or already dismissed, is recorded the same way and
/// answered the same way. What it never does is clear
/// `users.is_suggested`: the pool is the admins' to curate, and `dismiss`
/// writes to the viewer's dismissal table only.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::NotFound`] for a malformed,
/// unknown or deactivated account id. Any database failure arrives through
/// `?` as [`AppError::Database`].
async fn dismiss(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    let target = account_or_404(&state, &id).await?;
    suggestion::dismiss(&state.db, auth.0.id, target.id).await?;
    debug!(
        user = %auth.0.id,
        target = %target.id,
        "suggestion dismissed"
    );
    Ok(Json(json!({})))
}

/// `GET /api/v1/accounts/familiar_followers?id[]=a&id[]=b` — for each
/// account asked about, the accounts the caller follows that follow it.
///
/// The `id` values are read by [`requested_ids`]; ones that are not `UUID`
/// text and ones that name no stored account are skipped, so the response
/// holds one entry per known id in the order requested (a repeated id is
/// answered twice, because that is what was asked). No usable `id` at all is
/// the empty array, never an error.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user token;
/// [`AppError::Forbidden`] when the token does not grant `read:follows`.
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn familiar_followers(
    State(state): State<AppState>,
    auth: AuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<FamiliarFollowers>>> {
    auth.require(READ_FOLLOWS)?;
    let requested = requested_ids(uri.query());
    let known = user::get_many(&state.db, &requested).await?;
    let targets: HashMap<Uuid, &User> = known.iter().map(|found| (found.id, found)).collect();
    let mut entries = Vec::new();
    for id in requested {
        if let Some(target) = targets.get(&id) {
            entries.push(familiar_entry(&state, &auth.0, target).await?);
        }
    }
    debug!(
        count = entries.len(),
        user = %auth.0.id,
        "familiar followers listed"
    );
    Ok(Json(entries))
}

/// The one entry of the familiar-followers batch for `target`: the privacy
/// question first (Pleroma's controller answers the empty set for a target
/// with `hide_followers` unless the viewer *is* that target) and the
/// two-ended follow query second.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn familiar_entry(
    state: &AppState,
    viewer: &User,
    target: &User,
) -> AppResult<FamiliarFollowers> {
    let hidden = target.hide_followers.get() && target.id != viewer.id;
    let followers = if hidden {
        Vec::new()
    } else {
        follow::familiar_followers(&state.db, target.id, viewer.id, FAMILIAR_FOLLOWERS_LIMIT)
            .await?
    };
    Ok(FamiliarFollowers {
        id: target.id.to_string(),
        accounts: render_accounts(state, Some(viewer), &followers),
    })
}

/// `GET /api/v1/pleroma/birthdays` — the accounts whose birthday the named
/// day is: the caller and the accounts of their accepted follows that
/// chose to show it.
///
/// `month` and `day` each default to today's UTC part, so an absent
/// parameter asks about today and a partial pair asks about a day of the
/// current month. A birthday of `2000-02-29` matches `2`/`29` and nothing
/// else, which in a non-leap year is no day at all — Pleroma's
/// `birthday_month`/`birthday_day` filters compare the same two extracted
/// parts.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:accounts` check; [`AppError::BadRequest`] when `month` or
/// `day` is present but is not a number, or not a valid calendar part. Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn birthdays(
    State(state): State<AppState>,
    auth: AuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Account>>> {
    auth.require(READ_ACCOUNTS)?;
    let (month, day) = birthday_on(uri.query())?;
    let accounts = user::birthday_accounts(&state.db, auth.0.id, month, day).await?;
    debug!(
        count = accounts.len(),
        user = %auth.0.id,
        month,
        day,
        "birthdays listed"
    );
    Ok(Json(render_accounts(&state, Some(&auth.0), &accounts)))
}

/// The `month`/`day` a birthday listing answers for, read from `query` over
/// today's UTC parts: each parameter defaults to the part it names, so a
/// request carrying only `day` asks about that day of the current month.
///
/// # Errors
///
/// [`AppError::BadRequest`] from [`int_param`] for a value that is not a
/// number or not in its calendar range.
fn birthday_on(query: Option<&str>) -> AppResult<(i32, i32)> {
    let (today_month, today_day) = today_parts();
    let pairs = QueryPairs::parse(query);
    let month = int_param(&pairs, "month", MONTH_RANGE)?.unwrap_or(today_month);
    let day = int_param(&pairs, "day", DAY_RANGE)?.unwrap_or(today_day);
    Ok((month, day))
}

/// Today's UTC month and day, as the `int4`s the birthday query binds.
///
/// `chrono`'s calendar parts are `u32` bounded by a month and a day of a
/// month, so neither conversion can fail; the fallbacks exist because
/// decision 14 forbids the `unwrap` that would otherwise say so.
fn today_parts() -> (i32, i32) {
    let today = Utc::now().date_naive();
    let month = i32::try_from(today.month()).unwrap_or(JANUARY);
    let day = i32::try_from(today.day()).unwrap_or(FIRST);
    (month, day)
}

/// One integer query parameter of `pairs`: `Ok(None)` when `name` is
/// absent or empty, `Ok(Some(value))` when it is a number within `range`,
/// and [`AppError::BadRequest`] naming the parameter when it is either not
/// a number or outside its range.
fn int_param(pairs: &QueryPairs, name: &str, range: (i32, i32)) -> AppResult<Option<i32>> {
    let invalid = || AppError::BadRequest(format!("{INVALID_PARAMETER}{name}"));
    let Some(text) = pairs.value(name) else {
        return Ok(None);
    };
    let value = text.parse::<i32>().map_err(|_err| invalid())?;
    if range.0 <= value && value <= range.1 {
        return Ok(Some(value));
    }
    Err(invalid())
}

/// The two directory filters a request asked for: the sort order and the
/// `local` flag, each defaulted when absent or unreadable.
#[derive(Debug, Default)]
struct DirectoryFilters {
    /// The `order` parameter, [`DirectoryOrder::Active`] unless the request
    /// said `new` (Pleroma matches that one word and treats anything else
    /// as the default).
    order: user::DirectoryOrder,
    /// The `local` parameter: `true` only for a true spelling
    /// ([`is_true`]), so an absent or junk value lists local and remote
    /// accounts alike.
    local_only: bool,
}

impl DirectoryFilters {
    /// Reads `order` and `local` out of a raw query string.
    fn parse(query: Option<&str>) -> Self {
        let pairs = QueryPairs::parse(query);
        let order = match pairs.value("order").as_deref() {
            Some("new") => user::DirectoryOrder::New,
            _ => user::DirectoryOrder::default(),
        };
        Self {
            order,
            local_only: pairs.value("local").as_deref().is_some_and(is_true),
        }
    }
}

/// The whole answer of the three [`Account`] listings: every row rendered
/// for `viewer` as an [`Account`], in the order the query returned it.
///
/// `pleroma.relationship` stays the empty object Pleroma renders for a
/// viewer with no relationship loaded: none of these listings is a
/// relationship endpoint, so none runs the batch loader for it.
fn render_accounts(state: &AppState, viewer: AccountViewer<'_>, users: &[User]) -> Vec<Account> {
    users
        .iter()
        .map(|found| Account::from_user_for(viewer, found, &state.config, None))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `month`/`day` today's UTC date names, as `birthday_on` would
    /// default to them — the pair a test that sends no parameters expects
    /// back.
    fn today() -> (i32, i32) {
        today_parts()
    }

    /// The 400 body `int_param` answers for `name`.
    fn invalid(name: &str) -> AppError {
        AppError::BadRequest(format!("{INVALID_PARAMETER}{name}"))
    }

    /// [`int_param`] over a raw query string.
    fn int(query: Option<&str>, name: &str, range: (i32, i32)) -> AppResult<Option<i32>> {
        int_param(&QueryPairs::parse(query), name, range)
    }

    #[test]
    fn only_the_true_spellings_of_local_are_true() {
        for text in ["true", "TRUE", " true ", "1", "yes", "on"] {
            let query = format!("local={text}");
            assert!(
                DirectoryFilters::parse(Some(&query)).local_only,
                "`{text}` asks for local accounts"
            );
        }
        for query in [
            "",
            "local=",
            "local=false",
            "local=0",
            "local=no",
            "local=junk",
        ] {
            assert!(
                !DirectoryFilters::parse(Some(query)).local_only,
                "`{query}` is not a true flag, so the listing stays open to \
                 remote accounts"
            );
        }
    }

    #[test]
    fn directory_filters_default_to_the_active_order_and_everyone() {
        for query in [None, Some(""), Some("order=active"), Some("order=bogus")] {
            let filters = DirectoryFilters::parse(query);
            assert_eq!(
                filters.order,
                user::DirectoryOrder::Active,
                "`{query:?}` is not `new`, so Pleroma's activity order stands"
            );
            assert!(
                !filters.local_only,
                "`{query:?}` says nothing about locality"
            );
        }
        let filters = DirectoryFilters::parse(Some("order=new"));
        assert_eq!(
            filters.order,
            user::DirectoryOrder::New,
            "`new` is the one other spelling"
        );
        assert!(
            DirectoryFilters::parse(Some("local=true&order=new")).local_only,
            "`local=true` narrows the listing to this instance's own accounts"
        );
    }

    #[test]
    fn an_absent_limit_keeps_the_page_sizes() {
        // The two listings share the ticket's page sizes, so a request with
        // nothing in it must land on them rather than on `PageParams`'
        // global default of 20.
        let page = PageParams::from_query_with_default(None, LIST_LIMIT, LIST_CAP).unwrap();
        assert_eq!(page.limit, LIST_LIMIT, "the default page");
        assert_eq!(page.offset, None, "and the first page of it");
    }

    #[test]
    fn the_limit_is_capped_at_the_endpoints_own_cap() {
        for (query, expected) in [
            (Some("limit=80"), LIST_CAP),
            (Some("limit=200"), LIST_CAP),
            (Some("limit=0"), LIST_LIMIT),
            (Some("limit=nope"), LIST_LIMIT),
            (Some("limit=7"), 7),
        ] {
            let page = PageParams::from_query_with_default(query, LIST_LIMIT, LIST_CAP).unwrap();
            assert_eq!(page.limit, expected, "`{query:?}` clamps to {expected}");
        }
    }

    #[test]
    fn an_integer_param_is_optional_and_ranged() {
        assert_eq!(
            int(None, "month", MONTH_RANGE).unwrap(),
            None,
            "absent is nobody's opinion"
        );
        assert_eq!(
            int(Some("month=2"), "month", MONTH_RANGE).unwrap(),
            Some(2),
            "a number in range is read"
        );
        assert_eq!(
            int(Some("month=12&day=31"), "day", DAY_RANGE).unwrap(),
            Some(31),
            "among other parameters, and at the top of its range"
        );
        assert_eq!(
            int(Some("month=01"), "month", MONTH_RANGE).unwrap(),
            Some(1),
            "a leading zero is the same number"
        );
    }

    #[test]
    fn an_empty_param_is_absent_not_a_bad_request() {
        assert_eq!(
            int(Some("day="), "day", DAY_RANGE).unwrap(),
            None,
            "`?day=` names nothing, as Pleroma's `not_empty_string` guard reads it"
        );
        let (month, day) = today();
        assert_eq!(
            birthday_on(Some("day=")).unwrap(),
            (month, day),
            "so a birthday query with an empty day asks about today"
        );
        assert_eq!(
            birthday_on(Some("month=&day=")).unwrap(),
            (month, day),
            "and so does one with both parts empty"
        );
    }

    #[test]
    fn the_last_occurrence_of_a_param_wins() {
        assert_eq!(
            int(Some("day=3&day=4"), "day", DAY_RANGE).unwrap(),
            Some(4),
            "a repeated parameter folds to its last value"
        );
    }

    #[test]
    fn a_param_that_is_not_a_valid_calendar_part_is_a_bad_request() {
        for query in ["month=x", "month=13", "month=0", "month=-1"] {
            let err = int(Some(query), "month", MONTH_RANGE)
                .expect_err("a month outside the calendar is refused");
            assert_eq!(
                err.to_string(),
                invalid("month").to_string(),
                "the 400 names the parameter it could not read"
            );
        }
        for query in ["day=32", "day=0"] {
            assert_eq!(
                int(Some(query), "day", DAY_RANGE)
                    .expect_err("a bad day is refused")
                    .to_string(),
                invalid("day").to_string(),
                "and the same rule bounds the day"
            );
        }
    }

    #[test]
    fn a_birthday_query_defaults_to_today_and_overrides_either_part() {
        let (month, day) = today();
        assert_eq!(
            birthday_on(None).unwrap(),
            (month, day),
            "no parameters ask about today"
        );
        assert_eq!(
            birthday_on(Some("other=1")).unwrap(),
            (month, day),
            "and unrelated ones change nothing"
        );
        let asked = if month == 1 { 12 } else { month - 1 };
        assert_eq!(
            birthday_on(Some(&format!("month={asked}"))).unwrap(),
            (asked, day),
            "`month` alone asks for another month of this day"
        );
        let other_day = if day == 1 { 2 } else { day - 1 };
        assert_eq!(
            birthday_on(Some(&format!("day={other_day}"))).unwrap(),
            (month, other_day),
            "and `day` alone another day of this month"
        );
        assert_eq!(
            birthday_on(Some("month=2&day=29")).unwrap(),
            (2, 29),
            "both together name any day, the leap day included"
        );
    }

    #[test]
    fn a_birthday_query_refuses_an_unreadable_part() {
        for query in ["month=13", "day=0", "day=later"] {
            let err = birthday_on(Some(query)).expect_err("an unreadable part is refused");
            assert!(
                matches!(err, AppError::BadRequest(_)),
                "a bad calendar part is a 400, not a 422 or a silent default: {err}"
            );
        }
    }
}
