//! Filters: what the home, public and notification surfaces hide.

use super::*;

/// Scenario 2, the `home` filter: an irreversible home filter drops the
/// whole-word matches from the viewer's home page — the public page, the
/// same author's followers aside, stands untouched. Pleroma's
/// `restrict_filtered` exempts the actor's own posts and Pleroma's phrase
/// test is whole-word by default, and both arms show here: `bob`'s own
/// `spam diary` stays on his home page, and `spammer` never matches a
/// `spam` filter.
#[tokio::test]
async fn the_home_filter_hides_matches_from_home_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user(&pool, "alice").await;
    let (_bob, bob_token) = user(&pool, "bob").await;
    let app = common::test_app(pool);
    follow(&app, &bob_token, &alice).await;

    let digest = published(&app, &alice_token, "the weekly spam digest", "public").await;
    let today = published(&app, &alice_token, "all about spam today", "public").await;
    let glued = published(&app, &alice_token, "spammer confessions", "public").await;
    let clean = published(&app, &alice_token, "clean thread", "public").await;
    let own = published(&app, &bob_token, "my own spam diary", "public").await;
    let every = || {
        vec![
            id_of(&own),
            id_of(&clean),
            id_of(&glued),
            id_of(&today),
            id_of(&digest),
        ]
    };
    assert_eq!(
        page_ids(&read(&app, &bob_token, HOME).await),
        every(),
        "home holds the whole fixture before any filter"
    );

    let _home_filter = filter_created(
        &app,
        &bob_token,
        json!({"phrase": "spam", "context": ["home"], "irreversible": true}),
    )
    .await;
    assert_eq!(
        page_ids(&read(&app, &bob_token, HOME).await),
        vec![id_of(&own), id_of(&clean), id_of(&glued)],
        "the home filter drops the whole-word matches; the glued word and \
         the exempt own post stand"
    );
    assert_eq!(
        page_ids(&read(&app, &bob_token, PUBLIC).await),
        every(),
        "a home-context filter hides nothing from the public page"
    );
    assert_eq!(
        page_ids(&read(&app, &alice_token, HOME).await),
        vec![id_of(&clean), id_of(&glued), id_of(&today), id_of(&digest)],
        "filters are per-user: alice's home page never heard of bob's"
    );
}

/// Scenario 2, the `public` filters: an irreversible public filter drops
/// its match from the public page, a warn (non-irreversible) filter of the
/// same context only annotates its match — which therefore keeps riding
/// the v1 page, its `filtered` array the telling — and deleting a filter
/// or running past its expiry takes the drops away with it.
#[tokio::test]
async fn the_public_filters_drop_and_their_removal_restores() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, alice_token) = user(&pool, "alice").await;
    let (_, bob_token) = user(&pool, "bob").await;
    let app = common::test_app(pool.clone());

    let digest = published(&app, &alice_token, "the weekly spam digest", "public").await;
    let thread = published(&app, &alice_token, "clean thread", "public").await;
    let kept = published(&app, &alice_token, "an unrelated note", "public").await;

    let warned = filter_created(
        &app,
        &bob_token,
        json!({"phrase": "thread", "context": ["public"]}),
    )
    .await;
    let irreversible = filter_created(
        &app,
        &bob_token,
        json!({"phrase": "digest", "context": ["public"], "irreversible": true}),
    )
    .await;
    let hidden_page = read(&app, &bob_token, PUBLIC).await;
    assert_eq!(
        page_ids(&hidden_page),
        vec![id_of(&kept), id_of(&thread)],
        "the irreversible filter drops its match; the warn filter's match \
         stays on the page, annotated"
    );
    let warned_entry = hidden_page
        .json_value()
        .as_array()
        .expect("a status listing is an array")
        .iter()
        .find(|status| status["id"] == json!(id_of(&thread)))
        .expect("the warned row rides the page")
        .clone();
    assert!(
        !warned_entry["filtered"]
            .as_array()
            .expect("filtered is an array")
            .is_empty(),
        "the warned row carries its `filtered` warning"
    );
    assert_eq!(
        listed_phrases(&read(&app, &bob_token, FILTERS).await),
        vec!["digest", "thread"],
        "the filter listing answers both, newest first"
    );

    let deleted =
        common::TestRequest::delete(&format!("{FILTERS}/{}", warned["id"].as_str().expect("id")))
            .bearer(&bob_token)
            .send(&app)
            .await;
    common::assert_answer(&deleted, StatusCode::OK, "the warn filter deletes");
    let after_removal = page_ids(&read(&app, &bob_token, PUBLIC).await);
    assert_eq!(
        after_removal,
        vec![id_of(&kept), id_of(&thread)],
        "deleting the warn filter changes the page nothing: its match never left"
    );

    let expired = sqlx::query(
        "UPDATE filters SET expires_at = now() - interval '1 hour' WHERE id = $1::uuid",
    )
    .bind(irreversible["id"].as_str().expect("id"))
    .execute(&pool)
    .await
    .expect("the expiry backdate runs");
    assert_eq!(expired.rows_affected(), 1, "and it hit the digest filter");
    let after_expiry = page_ids(&read(&app, &bob_token, PUBLIC).await);
    assert_eq!(
        after_expiry,
        vec![id_of(&kept), id_of(&thread), id_of(&digest)],
        "the expired filter hides nothing any more"
    );
    assert_eq!(
        listed_phrases(&read(&app, &bob_token, FILTERS).await),
        Vec::<String>::new(),
        "and an expired filter leaves the active listing"
    );
}

/// Scenario 2, the `notifications` filter: Pleroma's `Notification`
/// `exclude_filtered` twin. The recipient's own posts and the statusless
/// rows stand exempt; a home-context filter of the same phrase hides
/// nothing here; the notifications filter hides the matching mention from
/// the flat listing, and deleting it brings the row back.
#[tokio::test]
async fn the_notifications_filter_hides_matching_mentions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = user(&pool, "alice").await;
    let (bob, bob_token) = user(&pool, "bob").await;
    let app = common::test_app(pool);

    let alert = published(&app, &alice_token, "a spam alert lands here @bob", "public").await;
    follow(&app, &alice_token, &bob).await;
    follow(&app, &bob_token, &alice).await;
    assert_eq!(
        notification_types(&read(&app, &bob_token, NOTIFICATIONS).await),
        vec!["follow", "mention"],
        "bob's page holds alice's follow and her mention, newest first \
         (bob's own follow answers on alice's page, not here)"
    );

    let _home_filter = filter_created(
        &app,
        &bob_token,
        json!({"phrase": "spam", "context": ["home"], "irreversible": true}),
    )
    .await;
    assert_eq!(
        notification_types(&read(&app, &bob_token, NOTIFICATIONS).await),
        vec!["follow", "mention"],
        "a home filter hides no notification, and the mention's post is \
         hidden from home"
    );
    assert!(
        !page_ids(&read(&app, &bob_token, HOME).await).contains(&id_of(&alert)),
        "the same filter does hide it from the home page"
    );

    let spam_filter = filter_created(
        &app,
        &bob_token,
        json!({"phrase": "spam", "context": ["notifications"], "irreversible": true}),
    )
    .await;
    assert_eq!(
        notification_types(&read(&app, &bob_token, NOTIFICATIONS).await),
        vec!["follow"],
        "the notifications filter hides the mention and spares the \
         statusless follow row"
    );

    let deleted = common::TestRequest::delete(&format!(
        "{FILTERS}/{}",
        spam_filter["id"].as_str().expect("id")
    ))
    .bearer(&bob_token)
    .send(&app)
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the notifications filter deletes");
    assert_eq!(
        notification_types(&read(&app, &bob_token, NOTIFICATIONS).await),
        vec!["follow", "mention"],
        "and the mention notification stands again"
    );
}
