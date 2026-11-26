//! Quoting end to end: the quote pair, the create probe and the quote
//! visibility tests, down to the blocked viewer's shared 404.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};

use super::support::{
    ACCOUNTS, EVERYONE, NOT_FOUND, OPEN, STATUSES, Shape, UNKNOWN_ID, World, get, posted, sweep,
    world,
};
use super::thread_support::{context, context_arms, page_ids};
use crate::common;

/// Part 1's quote exhibit: the author's `private` original and the
/// `public` post quoting it through the create's `quote_id` arm, both
/// answered as the entities their creates wrote.
async fn quote_pair(app: &Router, world: &World) -> (Value, Value) {
    let original = posted(
        app,
        &world.author.token,
        json!({"status": "the quoted word", "visibility": "private"}),
    )
    .await;
    let quote = posted(
        app,
        &world.author.token,
        json!({"status": "quoting in public", "quote_id": common::id_of(&original)}),
    )
    .await;
    (original, quote)
}

/// `POST /api/v1/statuses` as `token` answered raw — the probe create
/// whose refusal a test pins, where [`posted`] would only ever assert the
/// 200 and hand back the entity.
async fn create_probe(app: &Router, token: &str, body: Value) -> common::TestResponse {
    common::TestRequest::post(STATUSES)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The quote kind, posted through the create's `quote_id` arm: the
/// create's own answer nests the quoted original for the poster — the
/// render arm's nested `quote`, one level deep — and the original counts
/// exactly this one quote on its `pleroma.quotes_count`.
#[tokio::test]
async fn a_quote_create_nests_its_original_and_counts_it_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let (original, quote) = quote_pair(&app, &world).await;

    assert_eq!(
        quote["visibility"],
        json!("public"),
        "a quote is an ordinary post about another one"
    );
    assert_eq!(
        quote["quote"]["state"],
        json!("accepted"),
        "Mastodon's Quote entity reports the poster may see it"
    );
    let nested = &quote["quote"]["quoted_status"];
    assert_eq!(
        nested["id"], original["id"],
        "the create nests the quoted original for the poster"
    );
    assert_eq!(
        nested["content"],
        json!("<p>the quoted word</p>"),
        "the nested entity speaks as the original"
    );
    assert_eq!(
        nested["account"]["acct"],
        json!("author"),
        "the original's author rides the nest"
    );
    assert_eq!(
        nested["quote"],
        Value::Null,
        "one level deep: the original quotes nothing"
    );
    let shown = get(
        &app,
        Some(&world.author.token),
        &format!("{STATUSES}/{}", common::id_of(&original)),
    )
    .await;
    common::assert_answer(&shown, StatusCode::OK, "the author reads the original");
    assert_eq!(
        shown.json_value()["pleroma"]["quotes_count"],
        json!(1),
        "the original counts its one quote"
    );
}

/// The show sweep of the quote kind: every open viewer reads the
/// `public` quote (rule 3 — only the blocked account loses even this),
/// while the nested `quote` follows the *original's* audience: the author
/// and the accepted follower read the `private` original back inside it
/// (rule 5), every other admitted viewer reads `quote.state:
/// unauthorized` with no id or text leaking — while the
/// entity keeps the quoting contract: top-level `quote_id` names the
/// original to every viewer, `pleroma.quote` mirrors the bare quoted
/// status and `pleroma.quote_visible` is false exactly when it is null, and `pleroma`
/// carries the original's `quote_id` and `quote_url` to every viewer.
#[tokio::test]
async fn a_quote_shows_its_original_only_to_viewers_that_reach_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let (original, quote) = quote_pair(&app, &world).await;

    let url = original["url"].as_str().expect("the original has a URL");
    let naked = url.trim_start_matches("https://");
    let display: String = naked.chars().take(30).collect();
    let ellipsis = if naked.chars().count() > 30 {
        "…"
    } else {
        ""
    };
    let shape = Shape {
        id: common::id_of(&quote),
        visibility: "public",
        content: json!(format!(
            "<p>quoting in public</p><p class=\"quote-inline\">RE: <a href=\"{url}\" \
             rel=\"ugc nofollow noopener noreferrer\">{display}{ellipsis}</a></p>"
        )),
        acct: "author",
    };
    let admitted = sweep(&app, &world, &shape, &OPEN).await;
    assert_eq!(admitted.len(), 7, "the open audience reads the quote");
    for (entity, who) in admitted.iter().zip(OPEN) {
        let sees = matches!(who, "author" | "follower");
        if sees {
            assert_eq!(
                entity["quote"]["quoted_status"]["id"], original["id"],
                "{who}: the reachable original nests"
            );
        } else {
            assert_eq!(
                entity["quote"],
                json!({ "state": "unauthorized", "quoted_status": null }),
                "{who}: the hidden original is unauthorized, no id or text leaking"
            );
        }
        assert_eq!(
            entity["quote_id"], original["id"],
            "{who}: quote_id names the original whatever its visibility"
        );
        assert_eq!(
            entity["pleroma"]["quote_visible"],
            json!(sees),
            "{who}: quote_visible is false exactly when the quote is null"
        );
        assert_eq!(
            entity["pleroma"]["quote"], entity["quote"]["quoted_status"],
            "{who}: the pleroma block mirrors the nested render"
        );
        assert_eq!(
            entity["pleroma"]["quote_id"], original["id"],
            "{who}: the pleroma block names the original too"
        );
        assert_eq!(
            entity["pleroma"]["quote_url"], original["url"],
            "{who}: and its url, even when the original is not visible"
        );
    }
}

/// Quoting is not replying: the quote's own `context` answers two empty
/// arms — for the author and for a stranger reading the same `public`
/// quote — and the quoted original answers no descent into the post
/// quoting it.
#[tokio::test]
async fn a_quote_hangs_from_no_thread_in_either_context() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let (original, quote) = quote_pair(&app, &world).await;

    for (who, token) in [
        ("the author", Some(world.author.token.as_str())),
        ("the stranger", Some(world.stranger.token.as_str())),
    ] {
        let response = context(&app, token, &common::id_of(&quote)).await;
        common::assert_answer(&response, StatusCode::OK, &format!("{who} at the quote"));
        let (ancestors, descendants) = context_arms(&response.json_value(), who);
        assert!(ancestors.is_empty(), "{who}: a quote replies to nothing");
        assert!(descendants.is_empty(), "{who}: nothing threaded the quote");
    }

    let below = context(&app, Some(&world.author.token), &common::id_of(&original)).await;
    common::assert_answer(&below, StatusCode::OK, "the author at the original");
    let (_ancestors, descendants) = context_arms(&below.json_value(), "the author at the original");
    assert!(
        descendants.is_empty(),
        "the original answers no descent into the post quoting it"
    );
}

/// The create's `quote_id` gate is the show route's shared gate: quoting
/// a `private` post the poster never reaches, quoting anything at all for
/// the account the author blocked (rule 2 outranks the quote), and
/// quoting a never-stored or a malformed id all answer the one shared
/// 404 — byte-identical to the show route's refusal of an absent id, so
/// a refused quote leaks no existence anywhere.
#[tokio::test]
async fn a_quote_of_an_invisible_original_is_the_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let (original, quote) = quote_pair(&app, &world).await;

    let bodies = [
        json!({"status": "a stranger cannot", "quote_id": common::id_of(&original)}),
        json!({"status": "a block outranks", "quote_id": common::id_of(&quote)}),
        json!({"status": "nothing stored", "quote_id": UNKNOWN_ID}),
        json!({"status": "not an id", "quote_id": "not-an-id"}),
    ];
    let tokens = [
        world.stranger.token.as_str(),
        world.blocker.token.as_str(),
        world.stranger.token.as_str(),
        world.stranger.token.as_str(),
    ];
    let mut refused = Vec::new();
    for (token, body) in tokens.into_iter().zip(bodies) {
        refused.push(create_probe(&app, token, body).await);
    }
    refused.push(
        get(
            &app,
            Some(world.stranger.token.as_str()),
            &format!("{STATUSES}/{UNKNOWN_ID}"),
        )
        .await,
    );
    common::assert_all_refused(
        &refused,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "every refused quote create and the show route's absent id",
    );
    let show = &refused[4];
    for probe in &refused[..4] {
        assert_eq!(
            probe.body, show.body,
            "a refused quote is indistinguishable from a missing status"
        );
    }
}

/// The refused creates wrote nothing: the quoted original still counts
/// exactly its one accepted quote and the refused poster's own status
/// page answers empty, so a probe of the gate costs no row and no count.
#[tokio::test]
async fn a_refused_quote_writes_no_row_and_moves_no_counter() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let (original, _quote) = quote_pair(&app, &world).await;

    for probe in [
        json!({"status": "a stranger cannot", "quote_id": common::id_of(&original)}),
        json!({"status": "nothing stored", "quote_id": UNKNOWN_ID}),
    ] {
        let refused = create_probe(&app, &world.stranger.token, probe).await;
        common::assert_answer(&refused, StatusCode::NOT_FOUND, "the refused quote create");
    }

    let shown = get(
        &app,
        Some(&world.author.token),
        &format!("{STATUSES}/{}", common::id_of(&original)),
    )
    .await;
    assert_eq!(
        shown.json_value()["pleroma"]["quotes_count"],
        json!(1),
        "the original still counts only the one accepted quote"
    );

    let stranger_page = get(
        &app,
        Some(&world.stranger.token),
        &format!("{ACCOUNTS}/{}/statuses", world.stranger.user.id),
    )
    .await;
    common::assert_answer(&stranger_page, StatusCode::OK, "the stranger's own page");
    assert!(
        page_ids(&stranger_page).is_empty(),
        "the refused creates wrote no row"
    );
}

/// Rule 2 in the matrix's own words: the account the author blocked reads
/// the shared 404 of the author's public post *and* of a `direct` that
/// @-addresses the blockee — the mention would have admitted them, the
/// block outranks it — while the same account's own post answers, in
/// full, to themselves and to every other viewer.
#[tokio::test]
async fn the_blocked_viewer_reads_nothing_of_the_author_and_their_own_everything() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;

    let open = posted(
        &app,
        &world.author.token,
        json!({"status": "public past a block"}),
    )
    .await;
    let dm = posted(
        &app,
        &world.author.token,
        json!({"status": "@blocker, an unread note", "visibility": "direct"}),
    )
    .await;
    let own = posted(
        &app,
        &world.blocker.token,
        json!({"status": "the blocked still post"}),
    )
    .await;

    let mut blind = Vec::new();
    for id in [common::id_of(&open), common::id_of(&dm)] {
        blind.push(
            get(
                &app,
                Some(&world.blocker.token),
                &format!("{STATUSES}/{id}"),
            )
            .await,
        );
    }
    common::assert_all_refused(
        &blind,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the blocked reader of the author's public post and of a DM addressed to them",
    );

    let own_shape = Shape {
        id: common::id_of(&own),
        visibility: "public",
        content: json!("<p>the blocked still post</p>"),
        acct: "blocker",
    };
    let admitted = sweep(&app, &world, &own_shape, &EVERYONE).await;
    assert_eq!(
        admitted.len(),
        8,
        "every viewer reaches the blockee's own post"
    );
}

// ---------------------------------------------------------------------------
// Part 2: the thread kinds over HTTP, the context reads, and the account
// statuses filter sweep.
// ---------------------------------------------------------------------------
