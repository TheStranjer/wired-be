//! The pagination conformance sweep: every shared-`PageParams` listing
//! walked through its cursors and `Link` headers on a seeded world.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
use wired_be::models::user::User;

use super::server::{
    BASE, SCOPES, Server, Verb, action_ok, call, expect, get_json, get_value, write_ok,
};
use super::sockets::{account, favourite, follow, post_status};
use crate::common;

// ──────────────────  Scenario 4: the pagination conformance sweep  ──────────────────

/// The default page size every shared-`PageParams` listing answers with.
const DEFAULT_PAGE: usize = 20;
/// The cap every swept listing clamps an absurd `limit` to.
const PAGE_CAP: usize = 40;
/// How many rows the sweep world seeds into every listing it walks.
const SWEEP_ROWS: usize = 25;

/// One swept listing: the bearer it must be read with, its path, and a
/// label for the failure messages.
#[derive(Debug)]
struct SweepListing {
    /// The token the listing is keyed to.
    token: String,
    /// The endpoint path.
    path: String,
    /// Who the listing is, in the failure text.
    who: &'static str,
}

/// The cursor a `Link` header's `rel="{rel}"` entry carries, whatever
/// query the endpoint repeated ahead of it.
fn link_cursor(link: &str, rel: &str) -> String {
    let wanted = format!("rel=\"{rel}\"");
    let entry = link
        .split(", ")
        .find(|entry| entry.ends_with(&wanted))
        .unwrap_or_else(|| panic!("the header carries a {rel} link: {link}"));
    let url = entry
        .strip_prefix('<')
        .and_then(|rest| rest.split('>').next())
        .expect("every entry is a url in angle brackets");
    url.rsplit_once('=')
        .expect("the cursor is the last pair")
        .1
        .to_owned()
}

/// Asserts the `Link` header is Pleroma's `add_link_headers` shape: the
/// `next` entry hangs `max_id` off the endpoint's own address under the
/// instance origin, the `prev` entry hangs `min_id`, both cursors are
/// uuid texts (decision 4 keys them on the row id, which is not always
/// the entity id the page carries), and they are distinct — `next`
/// points at the page's oldest key, `prev` at its newest.
fn assert_link_shape(link: Option<&str>, path: &str, who: &str) -> (String, String) {
    let value = link.unwrap_or_else(|| panic!("a non-empty {who} page links: {path}"));
    let next = link_cursor(value, "next");
    let prev = link_cursor(value, "prev");
    let next_entry = format!(r#"<{BASE}{path}?max_id={next}>; rel="next""#);
    assert!(
        value.starts_with(next_entry.as_str()),
        "{who}: the next entry leads the header: {value}"
    );
    let prev_entry = format!(r#", <{BASE}{path}?min_id={prev}>; rel="prev""#);
    assert!(
        value.ends_with(prev_entry.as_str()),
        "{who}: the prev entry closes the header: {value}"
    );
    assert_ne!(next, prev, "{who}: a one-row page would repeat the cursor");
    for cursor in [&next, &prev] {
        assert!(
            Uuid::parse_str(cursor).is_ok(),
            "{who}: the {cursor} cursor is an id: {value}"
        );
    }
    (next, prev)
}

/// The `max_id` walk of one listing: page by page down to the empty tail
/// that must carry no `Link` header, every non-empty page carrying one.
/// Answers the oldest cursor the walk saw — the key of the very last
/// row.
async fn sweep_walk(
    srv: &Server,
    listing: &SweepListing,
    first_ids: Vec<String>,
    next_cursor: String,
) -> (Vec<String>, String) {
    let mut walked: Vec<String> = first_ids;
    let mut oldest_cursor = next_cursor;
    loop {
        let reply = call(
            srv,
            Verb::Get,
            &format!("{}?max_id={oldest_cursor}", listing.path),
            Some(&listing.token),
            None,
        )
        .await;
        expect(
            &reply,
            StatusCode::OK,
            &format!("{} max_id walk", listing.who),
        );
        let ids = common::ids_of(&reply.json());
        let Some(page_link) = reply.header("link").map(str::to_owned) else {
            assert!(
                ids.is_empty(),
                "{}: a non-empty page carries a Link header: {ids:?}",
                listing.who
            );
            return (walked, oldest_cursor);
        };
        assert!(
            !ids.is_empty(),
            "{}: only an empty page drops the Link header",
            listing.who
        );
        walked.extend(ids);
        oldest_cursor = link_cursor(&page_link, "next");
    }
}

/// The cursor parameters of one listing against cursors the walk itself
/// produced: `since_id` polling the newer end of the page from the oldest
/// row's key and from a mid-listing one, and `min_id` answering the page
/// after the oldest key still newest-first.
async fn sweep_cursors(
    srv: &Server,
    listing: &SweepListing,
    expected: &[String],
    oldest_cursor: &str,
    next_cursor: &str,
) {
    let polled = get_value(
        srv,
        Some(&listing.token),
        &format!("{}?since_id={oldest_cursor}&limit={PAGE_CAP}", listing.path),
    )
    .await;
    assert_eq!(
        common::ids_of(&polled),
        expected[..SWEEP_ROWS - 1],
        "{}: since_id reads the newer rows, still newest-first",
        listing.who
    );
    let flipped = get_value(
        srv,
        Some(&listing.token),
        &format!("{}?min_id={oldest_cursor}&limit={PAGE_CAP}", listing.path),
    )
    .await;
    assert_eq!(
        common::ids_of(&flipped),
        expected[..SWEEP_ROWS - 1],
        "{}: the min_id page after the cursor answers newest-first too",
        listing.who
    );
    let middle = get_value(
        srv,
        Some(&listing.token),
        &format!("{}?since_id={next_cursor}&limit={PAGE_CAP}", listing.path),
    )
    .await;
    assert_eq!(
        common::ids_of(&middle),
        expected[..DEFAULT_PAGE - 1],
        "{}: a mid-listing cursor polls exactly the newer rows",
        listing.who
    );
}

/// The keyset walk every `PageParams` listing must perform, against a
/// world that seeded it [`SWEEP_ROWS`] rows: the newest-first default page
/// of [`DEFAULT_PAGE`] rows under a well-shaped `Link` header; the `max_id`
/// walk to the tail and the empty page that carries no header; `since_id`
/// polling only the rows newer than the oldest cursor and than a
/// mid-listing one; `min_id` reading the page just after the cursor still
/// newest-first; `limit` honouring five; and a malformed cursor refused
/// with the shared 400. The cursors are read from the headers themselves:
/// decision 4 keys a listing on its row id, which for the relationship
/// pages is documented to be the follow row rather than the account the
/// entity carries.
async fn sweep(srv: &Server, listing: &SweepListing) {
    let all = get_json(
        srv,
        Some(&listing.token),
        &format!("{}?limit={PAGE_CAP}", listing.path),
    )
    .await;
    let expected = common::ids_of(&all.json());
    assert_eq!(expected.len(), SWEEP_ROWS, "{}: the full page", listing.who);

    let first = get_json(srv, Some(&listing.token), &listing.path).await;
    let first_ids = common::ids_of(&first.json());
    assert_eq!(
        first_ids,
        expected[..DEFAULT_PAGE],
        "{}: the default page is the newest twenty, newest first",
        listing.who
    );
    let (next_cursor, _prev_cursor) =
        assert_link_shape(first.header("link"), &listing.path, listing.who);

    let (walked, oldest_cursor) = sweep_walk(srv, listing, first_ids, next_cursor.clone()).await;
    assert_eq!(
        walked, expected,
        "{}: the cursor walk covers every row once",
        listing.who
    );

    sweep_cursors(srv, listing, &expected, &oldest_cursor, &next_cursor).await;

    let small = get_value(
        srv,
        Some(&listing.token),
        &format!("{}?limit=5", listing.path),
    )
    .await;
    assert_eq!(
        common::ids_of(&small),
        expected[..5],
        "{}: limit sizes the page",
        listing.who
    );

    let bogus = call(
        srv,
        Verb::Get,
        &format!("{}?max_id=nope", listing.path),
        Some(&listing.token),
        None,
    )
    .await;
    expect(
        &bogus,
        StatusCode::BAD_REQUEST,
        &format!("{} bogus cursor", listing.who),
    );
    assert_eq!(
        bogus.json()["error"],
        json!("Invalid id"),
        "{}: the shared refusal wording",
        listing.who
    );
}

/// Twenty-five public posts each by Alice and by Carol (whose posts are
/// the favourite and bookmark targets, kept out of Bob's home timeline by
/// standing outside his follow graph).
async fn seed_sweep_posts(srv: &Server, alice_token: &str, carol_token: &str) {
    for index in 1..=SWEEP_ROWS {
        let alice_post =
            post_status(srv, alice_token, &format!("sweep alice {index}"), "public").await;
        assert!(alice_post["id"].is_string(), "alice's post {index} lands");
        let carol_post =
            post_status(srv, carol_token, &format!("sweep carol {index}"), "public").await;
        assert!(carol_post["id"].is_string(), "carol's post {index} lands");
    }
}

/// The twenty-five followers of Bob, each following over HTTP: the
/// followers page and Bob's twenty-five-row notification queue. (They
/// follow Bob, not Alice, so that the home sweep's own follow of Alice —
/// the twenty-sixth telling her queue would otherwise carry — belongs to
/// neither of those listings.)
async fn seed_sweep_followers(srv: &Server, bob: &User) {
    for index in 1..=SWEEP_ROWS {
        let (follower, token) = account(srv, &format!("rt226-sw-f{index}"), SCOPES).await;
        assert_eq!(
            follower.nickname,
            format!("rt226-sw-f{index}"),
            "the follower {index} exists"
        );
        follow(srv, &token, bob.id).await;
    }
}

/// Alice's favourites and bookmarks of all twenty-five of Carol's posts,
/// read through the statuses listing the sweep itself will walk.
async fn seed_sweep_engagements(srv: &Server, alice_token: &str, carol: &User) {
    let carol_statuses = get_value(
        srv,
        Some(alice_token),
        &format!("/api/v1/accounts/{}/statuses?limit={PAGE_CAP}", carol.id),
    )
    .await;
    for status in carol_statuses.as_array().expect("a listing") {
        let id = common::id_of(status);
        favourite(srv, alice_token, &id).await;
        let _bookmarked = write_ok(
            srv,
            Verb::Post,
            &format!("/api/v1/statuses/{id}/bookmark"),
            alice_token,
            &json!({}),
        )
        .await;
    }
}

/// The remaining twenty-five-row worlds: Alice's mutes and blocks of two
/// disjoint crowds, and one direct thread per third crowd, opened by Alice.
async fn seed_sweep_relationships(srv: &Server, alice_token: &str) {
    for index in 1..=SWEEP_ROWS {
        let (mute_target, _mute_token) = account(srv, &format!("rt226-sw-m{index}"), SCOPES).await;
        let _muted = action_ok(
            srv,
            &format!("/api/v1/accounts/{}/mute", mute_target.id),
            alice_token,
        )
        .await;
        let (block_target, _block_token) =
            account(srv, &format!("rt226-sw-b{index}"), SCOPES).await;
        let _blocked = action_ok(
            srv,
            &format!("/api/v1/accounts/{}/block", block_target.id),
            alice_token,
        )
        .await;
        let (partner, _partner_token) = account(srv, &format!("rt226-sw-c{index}"), SCOPES).await;
        let dm = post_status(
            srv,
            alice_token,
            &format!("@{} quietly {index}", partner.nickname),
            "direct",
        )
        .await;
        assert!(dm["id"].is_string(), "the dm {index} lands");
    }
}

/// The conformance sweep over every listing that shares `PageParams`'
/// keyset on the row id (decision 4): the home timeline, an account's
/// statuses, the notification queue, the followers page, the favourites,
/// bookmarks, mutes and blocks listings, and the conversations — each on a
/// world of twenty-five rows built entirely over HTTP, each walked through
/// its cursors and its `Link` header the way any client would.
#[tokio::test]
async fn every_keyset_listing_walks_its_cursors_under_link_headers() {
    let srv = Server::start().await;
    let (alice, alice_token) = account(&srv, "rt226-sw-alice", SCOPES).await;
    let (bob, bob_token) = account(&srv, "rt226-sw-bob", SCOPES).await;
    let (carol, carol_token) = account(&srv, "rt226-sw-carol", SCOPES).await;
    follow(&srv, &bob_token, alice.id).await;
    seed_sweep_posts(&srv, &alice_token, &carol_token).await;
    seed_sweep_followers(&srv, &bob).await;
    seed_sweep_engagements(&srv, &alice_token, &carol).await;
    seed_sweep_relationships(&srv, &alice_token).await;

    let listings = vec![
        SweepListing {
            token: bob_token.clone(),
            path: "/api/v1/timelines/home".to_owned(),
            who: "home timeline",
        },
        SweepListing {
            token: alice_token.clone(),
            // Carol's history: a clean twenty-five public rows. Alice's
            // own listing would also carry the twenty-five threads of
            // `seed_sweep_relationships` addressed to her as direct,
            // which is the account-statuses endpoint's rule, not a
            // pagination deviation.
            path: format!("/api/v1/accounts/{}/statuses", carol.id),
            who: "account statuses",
        },
        SweepListing {
            token: bob_token,
            path: "/api/v1/notifications".to_owned(),
            who: "notifications",
        },
        SweepListing {
            token: alice_token.clone(),
            path: format!("/api/v1/accounts/{}/followers", bob.id),
            who: "followers",
        },
        SweepListing {
            token: alice_token.clone(),
            path: "/api/v1/favourites".to_owned(),
            who: "favourites",
        },
        SweepListing {
            token: alice_token.clone(),
            path: "/api/v1/bookmarks".to_owned(),
            who: "bookmarks",
        },
        SweepListing {
            token: alice_token.clone(),
            path: "/api/v1/mutes".to_owned(),
            who: "mutes",
        },
        SweepListing {
            token: alice_token.clone(),
            path: "/api/v1/blocks".to_owned(),
            who: "blocks",
        },
        SweepListing {
            token: alice_token,
            path: "/api/v1/conversations".to_owned(),
            who: "conversations",
        },
    ];
    for listing in &listings {
        sweep(&srv, listing).await;
    }
}
