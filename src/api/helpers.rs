//! Shared helpers for the API area handlers (`docs/ARCHITECTURE.md`
//! decision 3).
//!
//! Small request-level glue that several endpoints need and that belongs
//! neither inside one area's module nor in [`crate::util`] (which knows no
//! HTTP): resolving client-supplied ids to rows with exactly the error the
//! endpoint owes the client, reading the `id[]` array a batch endpoint
//! is keyed on, answering one [`TimelineQuery`] page as a status listing
//! ([`timeline_response`], with the `Link` header and the filter-parameter
//! readers the timeline routes and the account status listing share), and
//! more as later tickets need it.

use axum::{
    Json,
    http::{HeaderMap, Uri},
};
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    entities::status::Status as StatusEntity,
    error::{AppError, AppResult},
    models::{
        status::{self, Status, Visibility},
        status_visibility,
        user::{self, User},
    },
    pagination::{link_header, set_link_header},
    services::{filter, render_status::render_statuses, timeline::TimelineQuery},
    state::AppState,
    util::id::parse_id,
};

/// The keys the accounts batch endpoints read their ids from: Mastodon's
/// bracketed array plus the single value Pleroma's `List.wrap` accepts.
const ID_KEYS: [&str; 2] = ["id", "id[]"];
/// The deprecated alias of [`ID_KEYS`] that only the status batch lookup
/// answers to (`GET /api/v1/statuses`), per
/// [`requested_status_ids`].
const IDS_KEYS: [&str; 2] = ["ids", "ids[]"];

/// Resolves the `{id}` segment of an account route to its [`User`] row.
///
/// `id_str` must be `UUID` text as clients know it (decision 4: the
/// hyphenated or simple spelling, [`parse_id`]) and must name a row that
/// is still active. Anything else — a malformed id, an unknown id, a
/// deactivated account — is [`AppError::NotFound`]: Pleroma's
/// `assign_account_by_id` plus `visible_for` answer, and one that keeps
/// the endpoint from telling a bad id apart from a missing one. The show
/// endpoint (this ticket) and later routes keyed by account id share it.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
pub async fn account_or_404(state: &AppState, id_str: &str) -> AppResult<User> {
    let Some(id) = parse_id(id_str) else {
        return Err(AppError::NotFound);
    };
    user::get_by_id(&state.db, id)
        .await?
        .filter(|found| found.is_active.get())
        .ok_or(AppError::NotFound)
}

/// The `id` parameters of a raw query string, as parsed values in
/// occurrence order.
///
/// Both the bracketed array form Mastodon clients send (`id[]=a&id[]=b`,
/// percent-encodings included) and a single `id=a` (Pleroma's
/// `List.wrap(id)`), or any mix of the two. Values that are not `UUID` text
/// are dropped here, already skipped; whether an id names a stored account
/// is the caller's lookup (Pleroma's `User.get_all_by_ids` simply finds
/// nothing for an unknown one).
///
/// The batch endpoints keyed on a client-supplied id array share it: the
/// relationship batch (`GET /api/v1/accounts/relationships`) and the
/// familiar-followers batch (`GET /api/v1/accounts/familiar_followers`).
#[must_use]
pub fn requested_ids(query: Option<&str>) -> Vec<Uuid> {
    ids_under(query, &ID_KEYS)
}

/// The id array of `GET /api/v1/statuses`, which answers to both spellings.
///
/// [`requested_ids`]' `id[]` (and lone `id=`) and the deprecated `ids[]`
/// (and `ids=`) alias Pleroma's `StatusController.index` falls back to —
/// `Map.get(params, :id, Map.get(params, :ids))`. The `id` form wins
/// outright: a request that carries a usable `id` is answered from it and
/// its `ids` values are never read, exactly as that `Map.get` default
/// works. With no `id` at all the `ids` values are used, in occurrence
/// order; values that are not `UUID` text are dropped, already skipped.
///
/// How many of the ids an endpoint acts on is the caller's cap
/// ([`MAX_STATUS_IDS`](crate::api::statuses::MAX_STATUS_IDS)); this reads
/// them all.
#[must_use]
pub fn requested_status_ids(query: Option<&str>) -> Vec<Uuid> {
    let requested = ids_under(query, &ID_KEYS);
    if requested.is_empty() {
        return ids_under(query, &IDS_KEYS);
    }
    requested
}

/// The parsed values of the query params named by `keys`, in occurrence
/// order, with the values that are not `UUID` text dropped.
fn ids_under(query: Option<&str>, keys: &[&str]) -> Vec<Uuid> {
    let Some(query) = query else {
        return Vec::new();
    };
    form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _value)| keys.contains(&key.as_ref()))
        .filter_map(|(_key, value)| parse_id(&value))
        .collect()
}

/// Resolves the `{id}` segment of a status route to its live [`Status`]
/// row, as `viewer` may see it (`None`: an anonymous request).
///
/// `id_str` must be `UUID` text as clients know it (decision 4: the
/// hyphenated or simple spelling, [`parse_id`]). Everything that fails to
/// produce a row the viewer may read is the same [`AppError::NotFound`] —
/// a malformed id, an id that names no row, a tombstone (deleted), and a
/// status [`status_visibility`] hides from this viewer — so the endpoint
/// never reveals that a private post exists, which is Pleroma's
/// `StatusController.show` `with` block answering `{:error, :not_found}`
/// for both the missing and the invisible activity.
///
/// The row is the answer, not a rendered entity: this ticket's show route
/// renders it through [`crate::services::render_status`], and every later
/// `/api/v1/statuses/{id}/...` route (the context, history, source and
/// per-status state listings still to come) resolves and authorizes the
/// status through here and then reads what it needs from the row. A reblog
/// row is returned as itself; requesting a reblog's id yields the reblog,
/// whose original the renderer nests.
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
pub async fn status_for_viewer_or_404(
    state: &AppState,
    viewer: Option<&User>,
    id_str: &str,
) -> AppResult<Status> {
    let Some(id) = parse_id(id_str) else {
        return Err(AppError::NotFound);
    };
    let Some(row) = status::get_by_id(&state.db, id).await? else {
        return Err(AppError::NotFound);
    };
    // Rule 7 of the visibility model reads the original of a reblog with
    // the same tests, so a reblog of a status this viewer may not see
    // fails here and is answered the same 404 as the original would be.
    if status_visibility::can_view(&state.db, &row, viewer).await? {
        Ok(row)
    } else {
        Err(AppError::NotFound)
    }
}

/// The decoded pairs of one query string, read the way Pleroma's
/// `conn.query_params` hands them to its controller.
///
/// The pairs are kept in order, so a repeated array key can be read as the
/// list it is while a repeated scalar folds to its last value — how a
/// query-parameter map reads either way.
///
/// Every listing that filters on more than the cursors reads its parameters
/// through it rather than through `Query<HashMap<…>>`: a map refuses the
/// bracketed array spelling a client sends (`tags[]=a`) outright, and the
/// admin listings are exactly where clients send both shapes.
#[derive(Debug)]
pub struct QueryPairs(Vec<(String, String)>);

impl QueryPairs {
    /// Decodes `query` (an absent query string is no pairs at all).
    #[must_use]
    pub fn parse(query: Option<&str>) -> Self {
        let pairs = query.map_or_else(Vec::default, |raw| {
            form_urlencoded::parse(raw.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        });
        Self(pairs)
    }

    /// The value of the last occurrence of `key`, `None` when it is absent
    /// or empty — an empty parameter is nothing to filter on (Pleroma's
    /// `not_empty_string` guard).
    #[must_use]
    pub fn value(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .rev()
            .find(|(found, _)| found == key)
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty())
    }

    /// Every value of `key` in occurrence order, in both the bracketed
    /// spelling a client sends (`key[]`) and the bare one, empty values
    /// dropped; neither spelling present is the empty list.
    #[must_use]
    pub fn values(&self, key: &str) -> Vec<String> {
        let bracketed = format!("{key}[]");
        self.0
            .iter()
            .filter(|(found, value)| !value.is_empty() && (found == key || found == &bracketed))
            .map(|(_, value)| value.clone())
            .collect()
    }
}

/// The pagination keys the `Link` header replaces with its own cursors: the
/// three id cursors and `offset`, the cursors [`PageParams`] reads
/// (`limit` is a page size, not a cursor, and stays).
///
/// [`PageParams`]: crate::pagination::PageParams
const CURSOR_KEYS: [&str; 4] = ["max_id", "min_id", "since_id", "offset"];

/// The shared answer of every status-timeline page: one fetched, rendered,
/// filtered and linked [`TimelineQuery`].
///
/// [`fetch`](TimelineQuery::fetch) `query`, [`render_statuses`] the rows for
/// its viewer, post-process the page through
/// [`apply_active`](crate::services::filter::apply_active) in the source's
/// filter context ([`TimelineSource::filter_context`] — `None` for the
/// direct and list timelines, whose answer skips the pass entirely), and
/// answer the newest-first [`StatusEntity`] array with the `Link` header
/// this module's own page cursor derives from the request's `uri` and the
/// fetched rows' ids (Pleroma's `add_link_headers`). An empty page carries
/// no `Link` header at all.
///
/// Every route that reads a [`TimelineQuery`] — the `/api/v1/timelines/*`
/// endpoints and an account's status listing — builds its query and hands it
/// here, so the rendering, filtering, cursors and headers are stated once.
/// A route with its own order or its own header rule (the pinned profile
/// page) fetches and renders through the same two services and simply writes
/// no `Link`.
///
/// The filter pass runs after the render, and the `Link` keeps its meaning
/// either way: its cursors key on the fetched rows' ids, not the page's
/// survivors, so filtering one status off the page moves no cursor. An
/// anonymous viewer has no filters to read and comes back unfiltered — the
/// ticket's no-op.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`], and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`AppError::Internal`].
///
/// [`render_statuses`]: crate::services::render_status::render_statuses
/// [`TimelineSource::filter_context`]: crate::services::timeline::TimelineSource::filter_context
pub async fn timeline_response(
    state: &AppState,
    query: &TimelineQuery,
    uri: &Uri,
) -> AppResult<(HeaderMap, Json<Vec<StatusEntity>>)> {
    let rows = query.fetch(&state.db).await?;
    let link = page_link(state, uri, &rows);
    let rendered = render_statuses(state, query.viewer.as_ref(), &rows).await?;
    let statuses = match query.source.filter_context() {
        Some(context) => {
            filter::apply_active(state, query.viewer.as_ref(), rendered, context).await?
        }
        None => rendered,
    };
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    Ok((headers, Json(statuses)))
}

/// The `Link` header value for one fetched page: [`link_header`] over the
/// first (newest) and last (oldest) row's id — the same
/// [`add_link_headers`](crate::pagination::link_header) cursors every other
/// listing here writes — with the request's own parameters repeated per
/// Pleroma's `build_pagination_fields`, and `None` for an empty page.
fn page_link(state: &AppState, uri: &Uri, rows: &[Status]) -> Option<String> {
    let newest = rows.first()?.id.to_string();
    let oldest = rows.last()?.id.to_string();
    link_header(
        &state.config.base_url,
        uri.path(),
        &link_query(uri.query()),
        &newest,
        &oldest,
    )
}

/// The query parameters a page's `Link` targets repeat: the request's own
/// pairs with the four cursors [`PageParams`] reads (`CURSOR_KEYS`) dropped.
///
/// Each link carries its own cursor, and the next page starts where this one
/// ended. Every other parameter, `limit` included, rides on both links exactly
/// as it arrived, so a followed link re-asks the same filtered listing: the
/// timelines share it with the admin account listings, whose filters mean as
/// much to a second page as a `tag` does.
///
/// [`PageParams`]: crate::pagination::PageParams
#[must_use]
pub fn link_query(query: Option<&str>) -> Vec<(String, String)> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .filter(|(key, _value)| !CURSOR_KEYS.contains(&key.as_ref()))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

/// A request's tag name in the spelling the `hashtags` column stores.
///
/// Trimmed, leading `#`s dropped, lowercased (see
/// `crate::models::hashtag::normalize_name`, from which this differs only
/// in not capping the length — a name over the storage bound can be no
/// stored tag, and a timeline answers such a lookup the empty page rather
/// than an error). `None` when nothing usable is left.
#[must_use]
pub fn normalize_tag(raw: &str) -> Option<String> {
    let stripped = raw.trim().trim_start_matches('#').trim();
    (!stripped.is_empty()).then(|| stripped.to_lowercase())
}

/// Appends `raw` to `list` as [`normalize_tag`] spells it; a name that
/// normalizes to nothing is dropped, Pleroma's `Enum.filter(& &1)` — an
/// empty `any[]` entry widens the match with nothing.
pub fn push_tag(list: &mut Vec<String>, raw: &str) {
    if let Some(tag) = normalize_tag(raw) {
        list.push(tag);
    }
}

/// Appends `raw` to `list` when it spells one of the five stored
/// visibilities ([`Visibility`]'s `FromStr`, the verbatim lowercase
/// spellings clients send); anything else is ignored, narrowing nothing.
pub fn push_visibility(list: &mut Vec<Visibility>, raw: &str) {
    if let Ok(visibility) = raw.trim().parse::<Visibility>() {
        list.push(visibility);
    }
}

/// Whether a query parameter's text spells a true boolean.
///
/// `true` or `1`, trimmed and matched case-insensitively — the same reading
/// [`crate::api::search`] gives its flags; anything else (absent, empty,
/// `false`, junk) is `false`.
#[must_use]
pub fn is_true(text: &str) -> bool {
    let text = text.trim();
    text.eq_ignore_ascii_case("true") || text == "1"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One of the ids the fixtures pass around.
    const ID_A: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// A second id, distinct from [`ID_A`].
    const ID_B: &str = "01934567-89ab-7cde-f012-3456789abcdf";

    /// The ids [`requested_ids`] found in `query`, as strings, so the
    /// assertions read like the query they came from.
    fn parsed(query: Option<&str>) -> Vec<String> {
        requested_ids(query)
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    #[test]
    fn array_params_are_read_in_order() {
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&id[]={ID_B}"))),
            vec![ID_A, ID_B],
            "Mastodon's repeated bracketed params, in the order sent"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&other=x&id[]={ID_B}"))),
            vec![ID_A, ID_B],
            "unrelated params in between are ignored"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_A}&id[]={ID_A}"))),
            vec![ID_A, ID_A],
            "a repeated id is kept twice: the response answers what was asked"
        );
    }

    #[test]
    fn a_single_id_param_is_accepted() {
        assert_eq!(
            parsed(Some(&format!("id={ID_B}"))),
            vec![ID_B],
            "Pleroma's List.wrap accepts the single form"
        );
        assert_eq!(
            parsed(Some(&format!("id={ID_B}&id[]={ID_A}"))),
            vec![ID_B, ID_A],
            "and the two forms merge in occurrence order"
        );
    }

    #[test]
    fn values_are_percent_decoded_and_garbage_is_skipped() {
        assert_eq!(
            parsed(Some(&format!("id%5B%5D={ID_A}"))),
            vec![ID_A],
            "the bracketed key may arrive percent-encoded"
        );
        assert_eq!(
            parsed(Some("id[]=not-an-id")),
            Vec::<String>::new(),
            "a value that is not id text is skipped, not an error"
        );
        assert_eq!(
            parsed(Some(&format!("id[]={ID_B}&id=oops&id[]={ID_A}"))),
            vec![ID_B, ID_A],
            "one bad value does not sink its neighbours"
        );
    }

    #[test]
    fn nothing_usable_is_the_empty_list() {
        for query in [None, Some(""), Some("id="), Some("other=1"), Some("id")] {
            assert_eq!(
                parsed(query),
                Vec::<String>::new(),
                "`{query:?}` carries no usable id"
            );
        }
    }

    /// The ids [`requested_status_ids`] found in `query`, as strings.
    fn status_parsed(query: Option<&str>) -> Vec<String> {
        requested_status_ids(query)
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    #[test]
    fn the_status_batch_also_reads_the_ids_alias() {
        assert_eq!(
            status_parsed(Some(&format!("ids[]={ID_A}&ids[]={ID_B}"))),
            vec![ID_A, ID_B],
            "Pleroma's deprecated spelling arrives in occurrence order too"
        );
        assert_eq!(
            status_parsed(Some(&format!("ids={ID_B}"))),
            vec![ID_B],
            "and the alias has its own unwrapped form"
        );
        assert_eq!(
            status_parsed(Some(&format!("id[]={ID_A}&ids%5B%5D={ID_B}"))),
            vec![ID_A],
            "`id` wins outright, so the `ids` value alongside it is never read"
        );
    }

    #[test]
    fn the_status_batch_falls_back_only_when_id_carries_nothing() {
        assert_eq!(
            status_parsed(Some(&format!("id[]={ID_A}&id[]={ID_B}"))),
            vec![ID_A, ID_B],
            "the documented spelling is read as `requested_ids` reads it"
        );
        assert_eq!(
            status_parsed(Some("id[]=not-an-id&ids[]=also-not")),
            Vec::<String>::new(),
            "nothing usable under either spelling is the empty list"
        );
        assert_eq!(
            status_parsed(None),
            Vec::<String>::new(),
            "no query at all is the empty list, not an error"
        );
    }

    #[test]
    fn the_link_repeats_the_filters_and_drops_the_cursors() {
        let extras = link_query(Some(
            "any[]=a&local=1&limit=2&max_id=old&min_id=new&since_id=s&offset=3",
        ));
        assert_eq!(
            extras,
            vec![
                ("any[]".to_owned(), "a".to_owned()),
                ("local".to_owned(), "1".to_owned()),
                ("limit".to_owned(), "2".to_owned()),
            ],
            "filters and `limit` ride on, the cursors and `offset` do not"
        );
        assert!(link_query(None).is_empty(), "no query repeats nothing");
    }

    /// The pair reader's own rules: an absent key, an empty value, the order
    /// of a list and the percent-decoding both listings rely on.
    #[test]
    fn the_pair_reader_folds_duplicates_and_drops_empties() {
        let pairs = QueryPairs::parse(Some("a=1&a=&b=2&c[]=x&a[]=y&empty=&empty[]=&d=a%20b"));
        assert_eq!(pairs.value("a").as_deref(), None, "the empty last value");
        assert_eq!(pairs.value("b").as_deref(), Some("2"), "an ordinary value");
        assert_eq!(pairs.value("d").as_deref(), Some("a b"), "a decoded value");
        assert_eq!(pairs.value("missing"), None, "and an absent key");
        assert_eq!(
            pairs.values("empty"),
            Vec::<String>::new(),
            "a key whose every value is empty lists nothing"
        );
        assert_eq!(
            pairs.values("a"),
            vec!["1", "y"],
            "a key with values in both spellings lists them in order"
        );
        assert_eq!(pairs.values("c"), vec!["x"], "the bracketed spelling");
        assert_eq!(
            pairs.values("missing"),
            Vec::<String>::new(),
            "an absent key is the empty list"
        );
        let none = QueryPairs::parse(None);
        assert_eq!(
            none.value("anything"),
            None,
            "no query string holds no pairs"
        );
    }

    #[test]
    fn normalize_tag_lowercases_strips_hash_and_rejects_nothing_usable() {
        assert_eq!(normalize_tag("#Foo").as_deref(), Some("foo"));
        assert_eq!(normalize_tag("##Bar ").as_deref(), Some("bar"));
        assert_eq!(normalize_tag(" Café ").as_deref(), Some("café"));
        assert_eq!(normalize_tag("#").as_deref(), None, "a lone `#` is empty");
        assert_eq!(
            normalize_tag("  #  ").as_deref(),
            None,
            "whitespace is empty"
        );
        let mut names = Vec::new();
        push_tag(&mut names, "#One");
        push_tag(&mut names, "");
        push_tag(&mut names, "two");
        assert_eq!(
            names,
            ["one", "two"],
            "`push_tag` keeps the usable names in order and drops the rest"
        );
    }

    #[test]
    fn the_shared_readers_spell_true_and_a_stored_visibility() {
        for text in ["true", "1", "TRUE", " true "] {
            assert!(is_true(text), "`{text}` spells true");
        }
        for text in ["", " ", "false", "0", "yes"] {
            assert!(!is_true(text), "`{text}` is not true here");
        }
        let mut kept = Vec::new();
        push_visibility(&mut kept, " public ");
        push_visibility(&mut kept, "Public");
        push_visibility(&mut kept, "direct");
        push_visibility(&mut kept, "nonsense");
        assert_eq!(
            kept,
            vec![Visibility::Public, Visibility::Direct],
            "the verbatim spellings count, a capitalization and junk do not"
        );
    }
}
