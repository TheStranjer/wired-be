//! Unit tests for the `Status` entity's rendering: the exact key set and
//! default values a plain render answers with, and the batch-loaded parts
//! that reach the wire — the cached card, the built poll, the rendered
//! quote, the nested reblog, and the media, mention and tag lists
//! [`Status::from_source`] maps off the rows the batch handed it.
//!
//! [`Status::from_source`]: super::Status::from_source

use serde_json::{Value, json};
use uuid::Uuid;

use super::test_support::{
    BASE, CARD_URL, EXPECTED_KEYS, PLEROMA_KEYS, Parts, attachment, base_row, base_user, hashtag,
    json_keys, other_user, render, render_nested, stamped,
};
use crate::{entities::poll::Poll, models::user::Flag};

#[test]
fn a_hidden_parent_renders_parent_visible_false() {
    let alice = base_user("alice");
    let row = base_row();
    let shown = render(&row, &Parts::of(&alice), None);
    assert_eq!(
        shown["pleroma"]["parent_visible"], true,
        "the source's flag defaults to a visible parent"
    );
    let hidden = render(&row, &Parts::of(&alice).parent_hidden(), None);
    assert_eq!(
        hidden["pleroma"]["parent_visible"], false,
        "the source's `parent_visible: false` reaches the pleroma block"
    );
}

#[test]
fn a_plain_status_emits_every_key_and_its_defaults() {
    let alice = base_user("alice");
    let row = base_row();
    let value = render(&row, &Parts::of(&alice), None);
    assert_eq!(json_keys(&value), EXPECTED_KEYS, "exact key set");
    assert_eq!(json_keys(&value["pleroma"]), PLEROMA_KEYS);

    assert_eq!(value["id"], row.id.to_string());
    assert_eq!(value["uri"], row.uri);
    assert_eq!(value["url"], json!(row.url));
    assert_eq!(value["created_at"], "2024-01-31T12:34:56.789Z");
    assert_eq!(value["content"], "<p>hello world</p>");
    assert_eq!(value["visibility"], "public");
    assert_eq!(value["sensitive"], false);
    assert_eq!(value["spoiler_text"], "");
    assert_eq!(value["media_attachments"], json!([]));
    assert_eq!(value["application"], Value::Null, "no generator, null");
    assert_eq!(value["mentions"], json!([]));
    assert_eq!(value["tags"], json!([]));
    assert_eq!(value["emojis"], json!([]), "emojis are a later ticket");
    assert_eq!(value["reblogs_count"], 0);
    assert_eq!(value["favourites_count"], 0);
    assert_eq!(value["replies_count"], 0);
    assert_eq!(value["reblog"], Value::Null, "not a reblog");
    assert_eq!(value["in_reply_to_id"], Value::Null);
    assert_eq!(value["in_reply_to_account_id"], Value::Null);
    assert_eq!(value["language"], Value::Null);
    assert_eq!(
        value["card"],
        Value::Null,
        "a page whose cards read found nothing renders null"
    );
    assert_eq!(
        value["poll"],
        Value::Null,
        "a row the batch found no poll on renders null"
    );
    assert_eq!(value["edited_at"], Value::Null);
    assert_eq!(
        value["reblogged"], false,
        "a page whose reblogs read found nothing renders the flag false"
    );
    assert_eq!(
        value["muted"], false,
        "a page whose thread-mutes read found nothing renders the flag \
             false, and the extension block agrees — one membership feeds both"
    );
    assert_eq!(value["pleroma"]["thread_muted"], false);
    assert_eq!(
        value["pinned"], false,
        "a page whose pins read found nothing renders the flag false"
    );
    assert_eq!(
        value["favourited"], false,
        "a page whose favourites read found nothing renders the flag false"
    );
    assert_eq!(
        value["bookmarked"], false,
        "a page whose bookmarks read found nothing renders the flag false"
    );
    assert_eq!(
        value["quote"],
        Value::Null,
        "a row that quotes nothing nests no quote"
    );
    assert_eq!(value["quote_id"], Value::Null, "and names no quoted target");
    assert_eq!(
        value["pleroma"]["quote"],
        Value::Null,
        "the pleroma mirror renders the same null"
    );
    assert_eq!(
        value["pleroma"]["quote_visible"], false,
        "and there is no quote to see"
    );
    assert_eq!(
        value["filtered"],
        json!([]),
        "the render itself never filters: the entity comes out with the \
             empty array and `services::filter` fills it as a page is post-processed"
    );
}

#[test]
fn a_linked_ok_card_renders_the_stored_json_verbatim() {
    let alice = base_user("alice");
    let row = base_row();
    let stored = json!({
        "url": CARD_URL,
        "title": "Example Domain",
        "type": "link",
        "image": null,
        "pleroma": { "opengraph": { "og:title": "Example Domain" } },
    });
    let value = render(&row, &Parts::of(&alice).with_card(stored.clone()), None);
    assert_eq!(
        value["card"], stored,
        "the card the fetch job stored renders exactly as cached"
    );
}

/// The [`Poll`] entity the batch would hand a polled render: the
/// fixture row's stored poll, two choices with one vote on the second,
/// and the viewer's mark at position 1. `expires_at` is the render
/// instant itself, and [`Poll::build`] expires a poll only strictly
/// past its deadline — so this one is open, and `hide_totals` (false
/// here, as stored) has nothing to withhold.
fn built_poll() -> Poll {
    use crate::models::poll::{Poll as PollRow, PollOption as PollOptionRow};

    let poll_id = Uuid::parse_str("01966666-6666-7666-8666-666666666666").unwrap();
    let option = |position: i32, title: &str, votes_count: i32| PollOptionRow {
        id: Uuid::now_v7(),
        poll_id,
        position,
        title: title.to_owned(),
        votes_count,
    };
    Poll::build(
        &PollRow {
            id: poll_id,
            status_id: Uuid::parse_str("01944444-4444-7444-8444-444444444444").unwrap(),
            expires_at: Some(stamped()),
            multiple: true,
            hide_totals: false,
            voters_count: 1,
            votes_count: 1,
            ended_notified: Flag::default(),
        },
        &[option(0, "tea", 0), option(1, "coffee", 1)],
        &[1],
        stamped(),
    )
}

#[test]
fn a_polled_row_renders_the_full_poll_entity() {
    let alice = base_user("alice");
    let row = base_row();
    let value = render(&row, &Parts::of(&alice).with_poll(built_poll()), None);
    assert_eq!(
        value["poll"],
        json!({
            "id": "01966666-6666-7666-8666-666666666666",
            "expires_at": "2024-01-31T12:34:56.789Z",
            "expired": false,
            "multiple": true,
            "votes_count": 1,
            "voters_count": 1,
            "voted": true,
            "own_votes": [1],
            "options": [
                {"title": "tea", "votes_count": 0},
                {"title": "coffee", "votes_count": 1},
            ],
            "emojis": [],
        }),
        "the batch's built poll reaches the wire whole, key for key"
    );
    assert_eq!(
        json_keys(&value),
        EXPECTED_KEYS,
        "and the entity's key set is unchanged by filling the slot"
    );
}

#[test]
fn a_rendered_quote_lands_in_the_entity_and_the_pleroma_block() {
    let alice = base_user("alice");
    let quoted_id = Uuid::parse_str("01955555-5555-7555-8555-555555555555").unwrap();
    let mut row = base_row();
    row.quote_of_id = Some(quoted_id);
    row.quotes_count = 2;
    let quote = json!({ "id": quoted_id.to_string(), "content": "<p>quoted</p>" });
    let value = render(&row, &Parts::of(&alice).quoting(quote.clone()), None);
    assert_eq!(
        value["quote_id"],
        quoted_id.to_string(),
        "the quoted target's id, hyphenated (decision 4)"
    );
    assert_eq!(
        value["quote"],
        json!({ "state": "accepted", "quoted_status": quote }),
        "Mastodon's Quote entity wraps the batch's nested render"
    );
    assert_eq!(
        value["pleroma"]["quote"], quote,
        "and the pleroma block carries the bare status"
    );
    assert_eq!(
        value["pleroma"]["quote_visible"], true,
        "a rendered quote is a visible one"
    );
    assert_eq!(
        value["pleroma"]["quotes_count"], 2,
        "the counter of quotes OF this row rides along, untouched"
    );
}

#[test]
fn a_quote_hidden_from_the_viewer_is_unauthorized() {
    let alice = base_user("alice");
    let quoted_id = Uuid::parse_str("01955555-5555-7555-8555-555555555555").unwrap();
    let mut row = base_row();
    row.quote_of_id = Some(quoted_id);
    let value = render(&row, &Parts::of(&alice).quoting_hidden(), None);
    assert_eq!(
        value["quote"],
        json!({ "state": "unauthorized", "quoted_status": null }),
        "a hidden quote names its state and withholds the status"
    );
    assert_eq!(value["quote_id"], quoted_id.to_string());
    assert_eq!(value["pleroma"]["quote"], Value::Null);
    assert_eq!(value["pleroma"]["quote_visible"], false);
}

#[test]
fn a_quote_that_is_gone_is_deleted() {
    let alice = base_user("alice");
    let quoted_id = Uuid::parse_str("01955555-5555-7555-8555-555555555555").unwrap();
    let mut row = base_row();
    row.quote_of_id = Some(quoted_id);
    for parts in [Parts::of(&alice).quoting_deleted(), Parts::of(&alice)] {
        let value = render(&row, &parts, None);
        assert_eq!(
            value["quote"],
            json!({ "state": "deleted", "quoted_status": null }),
            "a quote the batch could not find (or never handed over) is deleted"
        );
        assert_eq!(value["quote_id"], quoted_id.to_string());
        assert_eq!(value["pleroma"]["quote"], Value::Null);
        assert_eq!(value["pleroma"]["quote_visible"], false);
    }
}

#[test]
fn a_nested_quote_carries_only_state_and_the_quoted_id() {
    let alice = base_user("alice");
    let quoted_id = Uuid::parse_str("01955555-5555-7555-8555-555555555555").unwrap();
    let mut row = base_row();
    row.quote_of_id = Some(quoted_id);
    let value = render(&row, &Parts::of(&alice).quoting_linked(), None);
    assert_eq!(
        value["quote"],
        json!({ "state": "accepted", "quoted_status_id": quoted_id.to_string() }),
        "one level deep, as Pleroma renders a quote inside a quote"
    );
    assert_eq!(value["pleroma"]["quote"], Value::Null);
    assert_eq!(value["pleroma"]["quote_visible"], false);
}

#[test]
fn a_row_that_quotes_nothing_has_a_null_quote() {
    let alice = base_user("alice");
    let value = render(&base_row(), &Parts::of(&alice), None);
    assert_eq!(value["quote"], Value::Null, "no quote_of_id, no quote");
}

#[test]
fn the_nested_render_keeps_the_viewers_bookmark_out_of_the_original() {
    // The bookmarks read keys every row's flag to its own id, so a
    // reblog the viewer bookmarked renders flagged while the nested
    // original — which the client never named — does not.
    let alice = base_user("alice");
    let row = base_row();
    let nested = render_nested(&row, &alice);
    let value = render(&row, &Parts::of(&alice).bookmarked(), Some(nested));
    assert_eq!(value["bookmarked"], true, "the named row is flagged");
    assert_eq!(
        value["reblog"]["bookmarked"], false,
        "and the nested original is nobody's bookmark by itself"
    );
}

#[test]
fn a_reblog_nests_the_original_and_blanks_its_own_content() {
    let alice = base_user("alice");
    let bob = other_user("bob", "01934567-89ab-7cde-f012-3456789abcdf");

    let mut original = base_row();
    original.user_id = bob.id;
    let original_id = original.id;
    let original_uri = original.uri.clone();
    let nested = render_nested(&original, &bob);

    let mut reblog = base_row();
    reblog.reblog_of_id = Some(original_id);
    reblog.content = String::new();
    reblog.raw_text = String::new();
    let row_id = reblog.id;
    let row_uri = reblog.uri.clone();
    let value = render(&reblog, &Parts::of(&alice), Some(nested));

    assert_eq!(value["id"], row_id.to_string(), "the reblog's own id");
    assert_eq!(
        value["uri"], row_uri,
        "and uri: the reblog's own, not the original's"
    );
    assert_eq!(value["content"], "", "the outer content is empty");
    assert_eq!(
        value["pleroma"]["content"],
        json!({"text/plain": ""}),
        "and so is the plain-text variant"
    );
    assert_eq!(value["account"]["acct"], "alice", "the reblogger speaks");
    assert_eq!(value["reblog"]["id"], original_id.to_string());
    assert_eq!(value["reblog"]["uri"], original_uri);
    assert_eq!(value["reblog"]["content"], "<p>hello world</p>");
    assert_eq!(value["reblog"]["account"]["acct"], "bob");
    assert_eq!(
        value["reblog"]["reblog"],
        Value::Null,
        "one level deep: the nested entity's own reblog is null"
    );
}

#[test]
fn media_mentions_and_tags_render_from_the_loaded_rows() {
    let alice = base_user("alice");
    let mut carol = other_user(
        "carol@remote.example",
        "01934567-89ab-7cde-f012-3456789abcee",
    );
    carol.local = Flag::new(false);
    carol.ap_id = "https://remote.example/users/carol".to_owned();
    let carol_id = carol.id;
    let media = vec![
        attachment("https://media.example/1.png", "image/png"),
        attachment("https://media.example/2.mp4", "video/mp4"),
    ];
    let parts = Parts::of(&alice)
        .attached(media)
        .mentioning(vec![carol])
        .tagged(vec![hashtag("rust"), hashtag("news")]);
    let value = render(&base_row(), &parts, None);

    let rendered_media: Vec<&str> = value["media_attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["url"].as_str().unwrap())
        .collect();
    assert_eq!(
        rendered_media,
        ["https://media.example/1.png", "https://media.example/2.mp4"],
        "attachments keep their position order"
    );
    assert_eq!(value["media_attachments"][0]["type"], "image");
    assert_eq!(value["media_attachments"][1]["type"], "video");
    assert_eq!(
        value["mentions"],
        json!([{
            "id": carol_id.to_string(),
            "username": "carol",
            "url": "https://remote.example/users/carol",
            "acct": "carol@remote.example",
        }])
    );
    assert_eq!(
        value["tags"],
        json!([
            {"name": "rust", "url": format!("{BASE}/tags/rust")},
            {"name": "news", "url": format!("{BASE}/tags/news")},
        ])
    );
}
