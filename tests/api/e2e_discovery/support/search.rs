//! Search: the visibility matrix and the account and hashtag arms.

use super::*;

/// Scenario 6, the status-search visibility matrix: Alice posts one status
/// per visibility — the public, private and direct ones sharing the word
/// `zeta`, the unlisted one carrying its own word `umbra`, so the
/// stranger's shared-word answer is *only* the public row while the
/// unlisted status still searches as the publicly readable post the
/// visibility model says it is. Mallory is a stranger, Bob a follower of
/// Alice, Carol the mentioned party of the direct: each `v2` answer is
/// exactly what its viewer may see, and `v1` answers the same page.
#[tokio::test]
async fn search_answers_each_viewer_at_their_own_visibility() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let (_, carol_token) = user(&pool, "carol").await;
    let (_, mallory_token) = user(&pool, "mallory").await;
    let app = common::test_app(pool);

    let public = published(&app, &alice_token, "zeta probe public #findme", "public").await;
    let unlisted = published(&app, &alice_token, "umbra light note", "unlisted").await;
    let private = published(&app, &alice_token, "zeta probe private", "private").await;
    let direct = created(
        &app,
        &alice_token,
        json!({"status": "zeta probe direct @carol", "visibility": "direct"}),
    )
    .await;
    let pub_id = id_of(&public);
    let pri_id = id_of(&private);
    let dir_id = id_of(&direct);
    let zeta = format!("{SEARCH_V2}?q=zeta&type=statuses");

    let stranger = searched(&read(&app, &mallory_token, &zeta).await);
    assert_eq!(
        stranger,
        vec![pub_id.clone()],
        "the stranger's v2 status search finds only the public match"
    );
    let stranger_v1 = searched(
        &read(
            &app,
            &mallory_token,
            &format!("{SEARCH_V1}?q=zeta&type=statuses"),
        )
        .await,
    );
    assert_eq!(
        stranger_v1,
        vec![pub_id.clone()],
        "v1 answers the same page"
    );
    let umbra = searched(
        &read(
            &app,
            &mallory_token,
            &format!("{SEARCH_V2}?q=umbra&type=statuses"),
        )
        .await,
    );
    assert_eq!(
        umbra,
        vec![id_of(&unlisted)],
        "the unlisted row is searchable by anyone, its own word finding it"
    );

    follow(&app, &bob_token, &alice).await;
    let follower = searched(&read(&app, &bob_token, &zeta).await);
    assert_eq!(
        follower,
        vec![pri_id.clone(), pub_id.clone()],
        "the follower's answer adds the private post, newest first"
    );

    let mentioned = searched(&read(&app, &carol_token, &zeta).await);
    assert_eq!(
        mentioned,
        vec![dir_id.clone(), pub_id.clone()],
        "the mentioned party sees the direct, not the private"
    );
    let author = searched(&read(&app, &alice_token, &zeta).await);
    assert_eq!(
        author,
        vec![dir_id, pri_id, pub_id],
        "the author sees all three zeta posts, newest first"
    );
}

/// Scenario 6, the account and hashtag arms: the stranger's account search
/// finds Alice on the flat route and inside the combined document; the
/// `#findme` tag a public post created answers both versions' hashtag arm
/// — v1's plain name, v2's `Tag` entity — and an untyped `#` query fills
/// nothing else. The combined untyped document answers all three arms at
/// once, and the follower's home page keeps the same visibility answer the
/// search gives, so a regression in the shared clause shows on both.
#[tokio::test]
async fn search_fills_the_account_and_hashtag_arms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let (_, mallory_token) = user(&pool, "mallory").await;
    let app = common::test_app(pool);
    let secret = published(&app, &alice_token, "zeta on home only", "private").await;
    let tagged = published(&app, &alice_token, "zeta probe public #findme", "public").await;

    let accounts = read(&app, &mallory_token, &format!("{ACCOUNT_SEARCH}?q=alice")).await;
    common::assert_answer(&accounts, StatusCode::OK, "the account search answers");
    assert_eq!(
        accounts.json_value()[0]["id"],
        json!(alice.id.to_string()),
        "the flat array leads with the matching account"
    );

    let combined = read(&app, &mallory_token, &format!("{SEARCH_V2}?q=alice")).await;
    common::assert_answer(&combined, StatusCode::OK, "the untyped search answers");
    let document = combined.json_value();
    assert_eq!(
        document["accounts"][0]["acct"],
        json!("alice"),
        "the combined document fills the accounts arm"
    );
    assert!(
        document["statuses"].as_array().is_some_and(Vec::is_empty),
        "no status carries the word alice"
    );

    let v1 = read(&app, &mallory_token, &format!("{SEARCH_V1}?q=%23findme")).await;
    common::assert_answer(&v1, StatusCode::OK, "the v1 hashtag search answers");
    let v1_body = v1.json_value();
    assert_eq!(
        v1_body["hashtags"],
        json!(["findme"]),
        "v1 spells plain names"
    );
    assert!(
        v1_body["statuses"].as_array().is_some_and(Vec::is_empty),
        "a `#` query is a hashtag-only search: no status arm"
    );
    let v2 = read(&app, &mallory_token, &format!("{SEARCH_V2}?q=%23findme")).await;
    assert_eq!(
        v2.json_value()["hashtags"][0],
        json!({ "name": "findme", "url": format!("{BASE}/tags/findme"), "history": [] }),
        "v2 answers the Tag entity with the empty history"
    );

    follow(&app, &bob_token, &alice).await;
    let home = read(&app, &bob_token, HOME).await;
    assert_eq!(
        page_ids(&home),
        vec![id_of(&tagged), id_of(&secret)],
        "the follower's home page carries both alice's posts, newest first"
    );
    let searched_home = searched(
        &read(
            &app,
            &bob_token,
            &format!("{SEARCH_V2}?q=zeta&type=statuses"),
        )
        .await,
    );
    assert!(
        searched_home.contains(&id_of(&secret)),
        "and the follower's search agrees it is visible"
    );
}
