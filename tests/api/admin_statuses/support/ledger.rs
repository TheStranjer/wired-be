//! The ledger listing and the single-status read.

use super::*;

// --- GET /statuses ---------------------------------------------------------------------

/// The default ledger is the public one — the two public visibilities of
/// every author, boost rows held out, newest post first — and each entry is
/// the standard entity with the plain account entity beside it.
#[tokio::test]
async fn the_ledger_lists_the_public_statuses_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_alice").await;
    let rows = ladder(&pool, &alice).await;

    let answer = listed(&app, LEDGER, &token).await;
    assert_eq!(
        ids_of(&answer),
        newest(&rows[..2]),
        "public and unlisted only, newest first: the private visibilities \
         are held out until godmode is asked for"
    );

    let entity = answer.as_array().expect("an array")[0].clone();
    assert_eq!(
        entity["account"]["acct"].as_str(),
        Some("as_alice"),
        "the entity is the standard Status render, the author's ordinary \
         Account beside it"
    );
    assert_eq!(
        entity["visibility"].as_str(),
        Some("unlisted"),
        "the newest of the two listed visibilities leads the array"
    );
    assert!(
        entity["account"]["roles"].is_null(),
        "no admin account extension rides the entity"
    );
}

/// `godmode` is the visibility lift: with it the ledger holds `private`,
/// `direct` and Pleroma's `local` beside the two public arms, and the
/// explicit `godmode=false` is the default again.
#[tokio::test]
async fn godmode_admits_every_visibility() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_god_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_god_alice").await;
    let rows = ladder(&pool, &alice).await;

    let open = listed(&app, &format!("{LEDGER}?godmode=true"), &token).await;
    assert_eq!(
        ids_of(&open),
        newest(&rows),
        "every stored visibility, newest first"
    );

    let quiet = listed(&app, &format!("{LEDGER}?godmode=false"), &token).await;
    assert_eq!(
        ids_of(&quiet),
        newest(&rows[..2]),
        "the stated default is the public ledger again"
    );
}

/// `local_only` is the local authors' set, `with_reblogs` decides whether
/// boost rows are listed, and the two filters stack.
#[tokio::test]
async fn local_only_and_with_reblogs_narrow_the_ledger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_filter_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_filter_alice").await;
    let stranger = common::create_remote_user(&pool, "stranger", "remote.example").await;
    let own = post(&pool, &alice, Visibility::Public).await;
    let mirrored = mirror_of(&pool, &stranger, "remote.example", "ledger").await;
    let boosted = boost(&pool, &alice, &own).await;

    let default = listed(&app, LEDGER, &token).await;
    assert_eq!(
        ids_of(&default),
        vec![mirrored.id.to_string(), own.id.to_string()],
        "boosts are held out by default, remote posts are not"
    );

    let reblogs = listed(&app, &format!("{LEDGER}?with_reblogs=true"), &token).await;
    assert_eq!(
        ids_of(&reblogs),
        vec![
            boosted.id.to_string(),
            mirrored.id.to_string(),
            own.id.to_string()
        ],
        "with_reblogs lets the boost row in"
    );
    assert_eq!(
        reblogs.as_array().expect("an array")[0]["reblog"]["id"],
        json!(own.id.to_string()),
        "and it renders as the ordinary entity, nesting what it boosts"
    );

    let locals = listed(&app, &format!("{LEDGER}?local_only=true"), &token).await;
    assert_eq!(
        ids_of(&locals),
        vec![own.id.to_string()],
        "local_only drops the remote post, and the default still holds the \
         boost out"
    );

    let stacked = listed(
        &app,
        &format!("{LEDGER}?local_only=true&with_reblogs=true"),
        &token,
    )
    .await;
    assert_eq!(
        ids_of(&stacked),
        vec![boosted.id.to_string(), own.id.to_string()],
        "and the two filters stack: the local set with the boost row in"
    );
}

/// The window is the admin listing's: `page` from 1 and `page_size`
/// defaulting to 50 under the shared 200-row cap, both read leniently,
/// cutting the newest-first ledger.
#[tokio::test]
async fn the_page_window_walks_the_ledger() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_page_root", "admin:read:statuses").await;
    let alice = common::create_local_user(&pool, "as_page_alice").await;
    let rows = ladder(&pool, &alice).await;
    let every = newest(&rows);

    let first = listed(&app, &format!("{LEDGER}?godmode=true&page_size=2"), &token).await;
    assert_eq!(
        ids_of(&first),
        vec![every[0].clone(), every[1].clone()],
        "the first window is the two newest"
    );
    let second = listed(
        &app,
        &format!("{LEDGER}?godmode=true&page_size=2&page=2"),
        &token,
    )
    .await;
    assert_eq!(
        ids_of(&second),
        vec![every[2].clone(), every[3].clone()],
        "page 2 of the same window is the pair after them"
    );
    let past = listed(&app, &format!("{LEDGER}?godmode=true&page=99"), &token).await;
    assert!(ids_of(&past).is_empty(), "past the end is the empty array");

    let junk = listed(&app, &format!("{LEDGER}?page=nope&page_size=5000"), &token).await;
    assert_eq!(
        ids_of(&junk),
        vec![every[3].clone(), every[4].clone()],
        "an unusable `page` is the first page and a 5000-row ask is the \
         200-row cap, so the whole public ledger answers"
    );
}

// --- GET /statuses/{id} ----------------------------------------------------------------

/// The show route answers ANY live post — `private` included, no `godmode`
/// asked for — and every miss is Pleroma's one `{"error":"Not found"}`.
#[tokio::test]
async fn show_answers_any_live_post_and_404s_a_miss() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_root, token) = admin(&pool, "as_show_root", BOTH).await;
    let alice = common::create_local_user(&pool, "as_show_alice").await;
    let secret = post(&pool, &alice, Visibility::Private).await;
    let gone = post(&pool, &alice, Visibility::Public).await;
    let _deleted = removed(&app, &token, gone.id).await;

    let shown = listed(&app, &format!("{LEDGER}/{}", secret.id), &token).await;
    assert_eq!(shown["id"], secret.id.to_string(), "the entity is the row");
    assert_eq!(
        shown["visibility"], "private",
        "and it renders whatever its visibility is, no godmode asked"
    );

    let unknown = format!("{}/{}", LEDGER, Uuid::now_v7());
    let malformed = format!("{LEDGER}/not-a-uuid");
    let tombstoned = format!("{LEDGER}/{}", gone.id);
    for (who, path) in [
        ("an id that is no post", &unknown),
        ("a malformed id", &malformed),
        ("a deleted post", &tombstoned),
    ] {
        let missed = get_with(&app, path, &token).await;
        common::assert_answer(&missed, StatusCode::NOT_FOUND, who);
        common::assert_error(&missed, NOT_FOUND, who);
    }
}
