//! The mutes listing: entries, paging and the page cap.

use super::*;

/// The listing answers every mute of the caller with the muted account's
/// entities, newest mute first, each embedding the caller's relationship and
/// carrying `mute_expires_at` — the mute's expiry as a Mastodon time, `null`
/// for an indefinite one — under a `Link` header keyed on the mute row ids; an
/// account with nothing muted gets the empty array and no header, and another
/// account's mutes are nobody else's list.
#[tokio::test]
async fn the_listing_answers_accounts_with_their_expiry() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read write").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let app = common::test_app(pool.clone());

    let empty = get(&app, Some(&token), MUTES).await;
    assert_eq!(empty.status, StatusCode::OK, "an empty listing is a 200");
    assert_eq!(empty.json_value(), json!([]), "the empty array");
    assert_eq!(empty.header("link"), None, "with nothing to link from");

    mute_account(&app, &token, &bob.id).await;
    let response = common::TestRequest::post(&action_path(&carol.id, "mute"))
        .bearer(&token)
        .json(&json!({ "duration": 3_600, "notifications": false }))
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the timed mute is written");

    let stored = stored_mutes(&pool, &alice).await;
    assert_eq!(stored.len(), 2, "two fixture mutes");
    assert_eq!(
        stored[0].1,
        carol.id.to_string(),
        "the timed mute is the newer row"
    );

    let listed = get(&app, Some(&token), MUTES).await;
    assert_eq!(
        common::ids_of(&listed.json_value()),
        muted_ids(&stored),
        "the muted accounts, newest mute first"
    );
    assert_eq!(
        listed.header("link"),
        Some(expected_link(&stored[1].0, &stored[0].0).as_str()),
        "the Link cursors are the mute row ids, not the accounts"
    );

    let body = listed.json_value();
    assert_muted_entry(&body[0], &stored[0].1, stored[0].2.clone());
    assert_eq!(
        body[0]["pleroma"]["relationship"]["muting"],
        json!(true),
        "each entry embeds the caller's relationship"
    );
    assert_eq!(
        body[0]["pleroma"]["relationship"]["muting_notifications"],
        json!(false),
        "including the option the write set"
    );
    assert_muted_entry(&body[1], &stored[1].1, None);

    let (_, erin_token) = fixture(&pool, "erin", "read").await;
    let theirs = get(&app, Some(&erin_token), MUTES).await;
    assert_eq!(
        theirs.json_value(),
        json!([]),
        "another account's listing holds none of alice's mutes"
    );
}

/// The listing's page walk: `limit` at a time, `max_id` continuing where a page
/// ended, `min_id` answered newest-first again, the ends of the walk carrying no
/// header, and a malformed cursor the extractor's 400.
#[tokio::test]
async fn the_listing_pages_by_mute_row_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read write:mutes").await;
    let app = common::test_app(pool.clone());
    for nickname in ["bob", "carol", "dave", "erin"] {
        let muted = common::create_local_user(&pool, nickname).await;
        mute_account(&app, &token, &muted.id).await;
    }
    let stored = stored_mutes(&pool, &alice).await;
    assert_eq!(stored.len(), 4, "four fixture mutes");

    let first = get(&app, Some(&token), &format!("{MUTES}?limit=2")).await;
    assert_eq!(
        common::ids_of(&first.json_value()),
        vec![stored[0].1.clone(), stored[1].1.clone()],
        "two newest first"
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link(&stored[1].0, &stored[0].0).as_str()),
        "the cursors are the page's own ends"
    );

    let older = get(
        &app,
        Some(&token),
        &format!("{MUTES}?limit=2&max_id={}", stored[1].0),
    )
    .await;
    assert_eq!(
        common::ids_of(&older.json_value()),
        vec![stored[2].1.clone(), stored[3].1.clone()],
        "max_id continues where the page ended"
    );

    let newer = get(
        &app,
        Some(&token),
        &format!("{MUTES}?min_id={}", stored[2].0),
    )
    .await;
    assert_eq!(
        common::ids_of(&newer.json_value()),
        vec![stored[0].1.clone(), stored[1].1.clone()],
        "min_id answers the newest-first page just after the cursor"
    );

    let polled = get(
        &app,
        Some(&token),
        &format!("{MUTES}?since_id={}", stored[0].0),
    )
    .await;
    assert_eq!(
        polled.json_value(),
        json!([]),
        "nothing is newer than the head"
    );
    assert_eq!(
        polled.header("link"),
        None,
        "and an empty page links nowhere"
    );

    let past = get(
        &app,
        Some(&token),
        &format!("{MUTES}?max_id={}", stored[3].0),
    )
    .await;
    assert_eq!(past.status, StatusCode::OK, "walking off the end is a 200");
    assert_eq!(past.json_value(), json!([]), "with the empty array");
    assert_eq!(past.header("link"), None, "and no header");

    let malformed = get(&app, Some(&token), &format!("{MUTES}?max_id=nope")).await;
    assert_eq!(
        malformed.status,
        StatusCode::BAD_REQUEST,
        "a bad cursor is Pleroma's 400"
    );
    assert_eq!(malformed.text(), INVALID_ID);
}

/// The listing's cap is Mastodon's 80, not the global 40: a page of 41 mutes
/// comes back whole, while Pleroma's 20 stays the default when no `limit` is
/// asked for.
#[tokio::test]
async fn the_listing_pages_up_to_eighty_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    for n in 0..41 {
        let muted = common::create_local_user(&pool, &format!("muted{n}")).await;
        seed_mute(&pool, &alice, &muted, MuteOptions::default()).await;
    }
    let stored = stored_mutes(&pool, &alice).await;
    assert_eq!(stored.len(), 41, "and they are all on the books");
    let app = common::test_app(pool.clone());
    let token = common::create_app_and_token(&pool, &alice, "read:mutes").await;

    let default_page = get(&app, Some(&token), MUTES).await;
    assert_eq!(
        common::ids_of(&default_page.json_value()).len(),
        20,
        "Pleroma's default page of twenty"
    );

    let big = get(&app, Some(&token), &format!("{MUTES}?limit=41")).await;
    assert_eq!(
        common::ids_of(&big.json_value()).len(),
        41,
        "a page past the global 40 cap gets through: the listing's cap is 80"
    );
    assert_eq!(
        big.header("link"),
        Some(expected_link(&stored[40].0, &stored[0].0).as_str()),
        "and the whole page is one page, linked at both ends"
    );
}
