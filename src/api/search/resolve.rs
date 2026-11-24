//! The resolving arm's decision: which lists the answering route renders
//! ([`Arms`]), what one request may look up ([`Resolver`]), what its one
//! lookup produced ([`Resolution`]), and the query-shape routing that
//! spends the request's single remote lookup on it.

use url::Url;

use crate::{
    federation::webfinger_client::{split_acct, strip_finger_prefixes},
    models::{status::Status as StatusRow, user::User},
    state::AppState,
};

use super::{
    fetch::{fetch_actor_url, fetch_handle, fetch_status_url, web_url},
    params::{Resource, SearchParams},
};
/// Which lists the answering route renders, and so which arm the request's
/// single resolution is allowed to spend itself on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Arms {
    /// The combined document: both the `accounts` and the `statuses` lists
    /// answer, so either reading is worth running.
    Combined,
    /// The flat account array: only `accounts` answers, so a status reading
    /// would fetch a post this route has nowhere to put.
    AccountsOnly,
}

impl Arms {
    /// Whether this route renders `asked`'s list.
    fn renders(self, asked: Resource) -> bool {
        asked != Resource::Statuses || matches!(self, Self::Combined)
    }

    /// Whether a request on `params` may spend its one lookup on `asked`:
    /// the route renders that list, and the request's `type` selects it —
    /// `type=accounts` asks for no post, `type=statuses` for no account, and
    /// a `#`-prefixed untyped query asks for neither.
    fn attempts(self, params: &SearchParams, asked: Resource) -> bool {
        self.renders(asked) && params.searches(asked)
    }
}

/// Everything the one resolution of a request decides from: the parsed
/// request, the arms its route renders, the viewer whose credentials make
/// resolution possible at all, and the account rows the stored search has
/// already answered.
#[derive(Debug)]
pub(super) struct Resolver<'a> {
    /// The parsed request parameters.
    params: &'a SearchParams,
    /// Which lists the answering route renders.
    arms: Arms,
    /// The requesting viewer: `None` anonymously, which resolves nothing.
    viewer: Option<&'a User>,
    /// The rows [`account_rows`] answered for this request, in its ranking:
    /// the exact-match test reads them to keep a handle this instance
    /// already stores off the network.
    ///
    /// [`account_rows`]: crate::api::search::find::account_rows
    rows: &'a [User],
    /// The origin a mock `WebFinger` peer answers on, and `None` everywhere
    /// else: `WebFinger` is HTTPS-only by RFC 7033, so only the in-crate
    /// tests can point it at loopback (see
    /// [`crate::services::mentions::resolve_mentions_with_base`], the same
    /// seam).
    webfinger_base: Option<&'a str>,
}

impl<'a> Resolver<'a> {
    /// The resolver a handler runs, against the real `WebFinger`.
    pub(super) const fn new(
        params: &'a SearchParams,
        arms: Arms,
        viewer: Option<&'a User>,
        rows: &'a [User],
    ) -> Self {
        Self {
            params,
            arms,
            viewer,
            rows,
            webfinger_base: None,
        }
    }

    /// Whether this request asks for resolution at all: `resolve=true`, from
    /// an authenticated viewer. Resolution is a federation action on
    /// somebody's behalf, so an anonymous caller keeps the ordinary stored
    /// search's answer (Pleroma's `maybe_resolve` resolves for a user, too).
    const fn resolves(&self) -> bool {
        self.params.resolve && self.viewer.is_some()
    }

    /// Whether this request may spend its one lookup on `asked`'s arm.
    fn attempts(&self, asked: Resource) -> bool {
        self.arms.attempts(self.params, asked)
    }
}

/// What the one lookup of a resolving search produced. The payloads are
/// boxed: a [`User`] or a [`StatusRow`] outgrows the nothing-resolved arm by
/// an order of magnitude.
#[derive(Debug)]
pub(super) enum Resolution {
    /// Nothing resolved: no lookup ran, or every attempt failed quietly.
    Nothing,
    /// The account to answer first in `accounts`.
    Account(Box<User>),
    /// The status row to answer first in `statuses`.
    Status(Box<StatusRow>),
}

impl Resolution {
    /// The resolved account, when that is what the lookup found.
    pub(super) fn account(&self) -> Option<&User> {
        match self {
            Self::Account(found) => Some(found.as_ref()),
            Self::Nothing | Self::Status(_) => None,
        }
    }

    /// The resolved status row, when that is what the lookup found.
    pub(super) fn status(&self) -> Option<&StatusRow> {
        match self {
            Self::Status(found) => Some(found.as_ref()),
            Self::Nothing | Self::Account(_) => None,
        }
    }
}

/// The request's one remote lookup, silent on every failure. The query's
/// shape decides: an `http(s)` URL goes to [`resolve_url`], and any other
/// query that spells a `nick@domain` handle goes to [`resolve_handle`].
/// Anything else — a word, a `#tag`, a query in no scheme federation speaks
/// — resolves nothing, and the answer is the stored search's alone.
pub(super) async fn resolve_query(state: &AppState, request: &Resolver<'_>) -> Resolution {
    if !request.resolves() {
        return Resolution::Nothing;
    }
    let query = request.params.q.trim();
    match web_url(query) {
        Some(url) => resolve_url(state, request, &url).await,
        None => resolve_handle(state, request, query).await,
    }
}

/// The URL arm: the actor reading first, the status reading second, and the
/// request stops at the first thing it finds. A status URL run through the
/// actor reading fails there — a `Note` document is no actor — which is what
/// lets one query name either kind of remote object (the ticket's "try, and
/// fall through on failure"), while `type` still keeps either reading from
/// being attempted at all.
async fn resolve_url(state: &AppState, request: &Resolver<'_>, url: &Url) -> Resolution {
    if request.attempts(Resource::Accounts)
        && let Some(found) = fetch_actor_url(state, url).await
    {
        return Resolution::Account(Box::new(found));
    }
    if request.attempts(Resource::Statuses)
        && let Some(found) = fetch_status_url(state, url).await
    {
        return Resolution::Status(Box::new(found));
    }
    Resolution::Nothing
}

/// The handle arm: a `nick@domain` query (the leading `@` optional) that the
/// stored search matched no row for exactly, looked up through
/// [`get_or_fetch_by_acct`]. A handle this client would refuse to finger is
/// no handle at all for the search either — [`split_acct`] is what says so —
/// and a query the search already matched nickname for nickname needs no
/// lookup, because the row the fetch would return is already in the page.
///
/// [`get_or_fetch_by_acct`]: crate::federation::fetch_actor::get_or_fetch_by_acct
async fn resolve_handle(state: &AppState, request: &Resolver<'_>, query: &str) -> Resolution {
    if !request.attempts(Resource::Accounts)
        || split_acct(query).is_none()
        || matched_exact(request.rows, query)
    {
        return Resolution::Nothing;
    }
    fetch_handle(state, request.webfinger_base, query)
        .await
        .map_or(Resolution::Nothing, |found| {
            Resolution::Account(Box::new(found))
        })
}

/// Whether the stored search already answered with the account `query`
/// names: a row whose nickname *is* the query, case folded and its leading
/// `@` (or `acct:`) folded away. Such a query needs no lookup — the account
/// the fetch would return is already in the page.
fn matched_exact(rows: &[User], query: &str) -> bool {
    let handle = strip_finger_prefixes(query.trim());
    rows.iter()
        .any(|row| row.nickname.eq_ignore_ascii_case(handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::search::{
            find::account_entities,
            test_support::{
                dead_base, hits, local_user, mirror_user, mock_peer, parse, test_state,
            },
        },
        models::user::get_by_ap_id,
    };

    /// The one lookup a resolving combined search runs for `query`: `viewer`
    /// asking, `rows` already answered by the stored search, and `base` the
    /// mock `WebFinger` origin to point the handle arm at (`None` asks the
    /// real one, which no loopback mock answers).
    async fn resolution(
        state: &AppState,
        viewer: Option<&User>,
        query: &str,
        rows: &[User],
        base: Option<&str>,
    ) -> Resolution {
        let params = parse(query);
        let request = Resolver {
            webfinger_base: base,
            ..Resolver::new(&params, Arms::Combined, viewer, rows)
        };
        resolve_query(state, &request).await
    }

    /// A handle the stored search answered nothing for is fetched through
    /// `WebFinger`, mirrored into `users`, and rides the account list first;
    /// the next search for the same handle is served by that mirror, so the
    /// actor document is never dereferenced twice.
    #[tokio::test]
    async fn a_resolved_handle_rides_first_and_is_stored_for_later() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let (mock, server) = mock_peer().await;
        let viewer = local_user(&pool, "viewer").await;
        let follower = local_user(&pool, "alfred").await;

        let found = resolution(
            &state,
            Some(&viewer),
            "q=alice@mock.example&resolve=true",
            &[],
            Some(&mock.base),
        )
        .await;
        assert!(
            found.account().is_some() && found.status().is_none(),
            "the account arm is what answered"
        );
        let Resolution::Account(account) = found else {
            panic!("the handle resolved to an account: {found:?}");
        };
        assert_eq!(
            account.ap_id,
            format!("{}/users/alice", mock.base),
            "the actor the mock's WebFinger named"
        );
        assert!(!account.local.get(), "and a remote mirror, not a local row");
        let stored = get_by_ap_id(&pool, &account.ap_id)
            .await
            .expect("the mirror read")
            .expect("the fetched actor is stored for later");
        assert_eq!(stored.id, account.id, "the answer is the stored row");

        let follower_id = follower.id.to_string();
        let listed = account_entities(&state, Some(&viewer), &[follower], Some(&account))
            .await
            .expect("the entities render");
        let ids: Vec<String> = listed.iter().map(|entry| entry.id.clone()).collect();
        assert_eq!(
            ids,
            vec![account.id.to_string(), follower_id],
            "the resolved account comes first, the stored match follows"
        );

        let again = resolution(
            &state,
            Some(&viewer),
            "q=alice@mock.example&resolve=true",
            &[],
            Some(&mock.base),
        )
        .await;
        assert!(
            matches!(again, Resolution::Account(_)),
            "the mirror answers the next search too"
        );
        assert_eq!(
            (hits(&mock.webfinger_hits), hits(&mock.actor_hits)),
            (2, 1),
            "one actor dereference for the two searches: the second read the mirror"
        );
        server.abort();
    }

    /// A handle the stored search matched nickname for nickname runs no
    /// lookup at all: the account the fetch would return is already in the
    /// page, and the peer never hears about the request.
    #[tokio::test]
    async fn an_exact_handle_match_resolves_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let (mock, server) = mock_peer().await;
        let viewer = local_user(&pool, "viewer").await;
        let eve = mirror_user(&pool, "eve", "remote.example").await;
        assert!(
            matched_exact(std::slice::from_ref(&eve), "@Eve@remote.example"),
            "the stored nickname is an exact match for the typed handle"
        );

        let found = resolution(
            &state,
            Some(&viewer),
            "q=%40eve%40remote.example&resolve=true",
            &[eve],
            Some(&mock.base),
        )
        .await;
        assert!(
            matches!(found, Resolution::Nothing),
            "the exact match is the answer already: {found:?}"
        );
        assert_eq!(
            (hits(&mock.webfinger_hits), hits(&mock.actor_hits)),
            (0, 0),
            "and no lookup ran"
        );
        server.abort();
    }

    /// The peer behind a handle cannot be reached: the arm answers nothing,
    /// quietly — no error, no partial row, and the account list stays as the
    /// stored search left it.
    #[tokio::test]
    async fn an_unreachable_handle_resolves_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let viewer = local_user(&pool, "viewer").await;
        let base = dead_base().await;

        let found = resolution(
            &state,
            Some(&viewer),
            "q=alice@mock.example&resolve=true",
            &[],
            Some(&base),
        )
        .await;
        assert!(
            matches!(found, Resolution::Nothing),
            "an unreachable peer is the empty arm: {found:?}"
        );
    }

    /// Resolution needs both the flag and a viewer: `resolve=false` and an
    /// anonymous `resolve=true` are the ordinary stored search, and the mock
    /// peer sees neither request.
    #[tokio::test]
    async fn resolution_needs_the_flag_and_a_viewer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let (mock, server) = mock_peer().await;
        let viewer = local_user(&pool, "viewer").await;

        for (who, query) in [
            ("no flag", "q=alice@mock.example"),
            ("an anonymous caller", "q=alice@mock.example&resolve=true"),
            ("junk that is not a handle", "q=alice&resolve=true"),
            ("a tag query", "q=%23alice&resolve=true"),
        ] {
            let asked = match who {
                "an anonymous caller" => None,
                _ => Some(&viewer),
            };
            let found = resolution(&state, asked, query, &[], Some(&mock.base)).await;
            assert!(
                matches!(found, Resolution::Nothing),
                "{who} resolves nothing: {found:?}"
            );
        }
        assert_eq!(
            (hits(&mock.webfinger_hits), hits(&mock.actor_hits)),
            (0, 0),
            "and none of them reached the network"
        );
        server.abort();
    }

    /// `type` narrows the readings to the resource it names: a handle is
    /// never fetched for `type=statuses`, and the flat account search never
    /// spends its one lookup on a post it has no list for.
    #[tokio::test]
    async fn the_requested_type_narrows_what_resolves() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = test_state(&pool);
        let (mock, server) = mock_peer().await;
        let viewer = local_user(&pool, "viewer").await;

        let found = resolution(
            &state,
            Some(&viewer),
            "q=alice@mock.example&resolve=true&type=statuses",
            &[],
            Some(&mock.base),
        )
        .await;
        assert!(
            matches!(found, Resolution::Nothing),
            "`type=statuses` asks for no account: {found:?}"
        );
        assert_eq!(hits(&mock.webfinger_hits), 0, "and no lookup ran");

        let untyped = parse("q=bob");
        assert!(
            Arms::Combined.attempts(&untyped, Resource::Statuses),
            "the combined document can answer a resolved post"
        );
        assert!(
            !Arms::AccountsOnly.attempts(&untyped, Resource::Statuses),
            "the flat account array cannot, so it never fetches one"
        );
        for asked in [Resource::Accounts, Resource::Hashtags] {
            assert!(
                Arms::AccountsOnly.attempts(&untyped, asked),
                "the accounts arm is still there for {asked:?}"
            );
        }
        let only_tags = parse("q=%23bob");
        for asked in [Resource::Accounts, Resource::Statuses] {
            assert!(
                !Arms::Combined.attempts(&only_tags, asked),
                "a `#`-query resolves no {asked:?} either"
            );
        }
        server.abort();
    }
}
