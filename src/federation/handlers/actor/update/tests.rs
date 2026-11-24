//! Tests of the actor `Update` handler.

use serde_json::json;
use uuid::Uuid;

use super::super::{
    test_support::{
        ALICE, NEW_PEM, app_state, assert_handled, broken_state, fresh_user, ignored, local_user,
        mirror, person, ready_for_update, remote_user, view,
    },
    update_actor,
};
use super::*;
use crate::services::runtime_config;

/// The profile half of an `Update`: name, note, avatar, fields and the
/// endpoints the document changed all land on the mirror.
#[tokio::test]
async fn an_update_folds_the_new_profile_into_the_mirror() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = mirror(&pool).await;
    let alice_id = alice.id;
    assert_eq!(alice.name, "Alice", "the fixture mirrors the old profile");

    let mut document = person();
    document["name"] = json!("Alice the Second");
    document["summary"] = json!("<p>the <b>new</b> note</p>");
    document["icon"] = json!({"type": "Image", "url": "https://peer.example/avatar/new.png"});
    document["attachment"] = json!([
        {"type": "PropertyValue", "name": "Site", "value": "https://new.example"},
    ]);
    document["inbox"] = json!("https://peer.example/users/alice/inbox2");
    document["endpoints"] = json!({"sharedInbox": "https://peer.example/inbox"});
    document["publicKey"]["publicKeyPem"] = json!(NEW_PEM);

    let folded = update_actor(
        state.clone(),
        view("Update", "profile-1", ALICE, document),
        alice,
    )
    .await;
    assert_handled(&folded);

    let stored = fresh_user(&pool, alice_id).await;
    assert_eq!(stored.name, "Alice the Second", "the display name moved");
    assert_eq!(stored.bio, "<p>the <b>new</b> note</p>", "the note moved");
    assert_eq!(stored.raw_bio, "the new note\n", "with its source text");
    assert_eq!(
        stored.avatar_url.as_deref(),
        Some("https://peer.example/avatar/new.png"),
        "the avatar moved"
    );
    assert_eq!(
        stored.fields,
        json!([{"name": "Site", "value": "https://new.example"}]),
        "the profile fields moved"
    );
    assert_eq!(
        stored.inbox.as_deref(),
        Some("https://peer.example/users/alice/inbox2"),
        "the inbox endpoint moved"
    );
    assert_eq!(
        stored.shared_inbox.as_deref(),
        Some("https://peer.example/inbox"),
        "and the shared inbox arrived"
    );
    assert_eq!(
        stored.public_key.as_deref(),
        Some(NEW_PEM),
        "and so did the signing key"
    );
}

/// An `Update` that turns a `Person` into a `Service` flips the bot flag
/// on, and one that turns it back flips it off again.
#[tokio::test]
async fn an_update_follows_the_actor_type_into_and_out_of_bot() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = mirror(&pool).await;
    let alice_id = alice.id;
    assert!(!alice.bot.get(), "the fixture mirrors a human");

    let mut document = person();
    document["type"] = json!("Service");
    let turned_on = update_actor(
        state.clone(),
        view("Update", "bot-on", ALICE, document),
        alice,
    )
    .await;
    assert_handled(&turned_on);
    let stored = fresh_user(&pool, alice_id).await;
    assert_eq!(stored.actor_type, "Service", "the type moved");
    assert!(stored.bot.get(), "a Service is a bot");

    let human = person();
    let turned_off = update_actor(state, view("Update", "bot-off", ALICE, human), stored).await;
    assert_handled(&turned_off);
    let back = fresh_user(&pool, alice_id).await;
    assert_eq!(back.actor_type, "Person", "the type moved back");
    assert!(!back.bot.get(), "a Person is not a bot");
}

/// The MRF actor stage on the inbound half of the mirror: an `Update`
/// whose profile the domain lists strip lands without the fields they
/// name — the same rewrite a fetch folds, applied to what a peer
/// announces about itself.
#[tokio::test]
async fn an_update_lands_stripped_by_the_mrf_actor_lists() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    runtime_config::write(
        &pool,
        ":pleroma",
        ":mrf",
        &json!({
            "policies": ["simple"],
            "simple": {"avatar_removal": ["peer.example"], "banner_removal": ["*.example"]},
        }),
    )
    .await
    .expect("the mrf entry writes");
    let state = app_state(&pool);
    let alice = mirror(&pool).await;
    let alice_id = alice.id;
    assert!(
        alice.avatar_url.is_some(),
        "the fixture mirror holds an avatar to lose"
    );

    let mut document = person();
    document["name"] = json!("Alice stripped");
    document["icon"] = json!({"type": "Image", "url": "https://peer.example/avatar/new.png"});
    document["image"] = json!({"type": "Image", "url": "https://peer.example/banner.png"});

    let folded = update_actor(state, view("Update", "profile-mrf", ALICE, document), alice).await;
    assert_handled(&folded);

    let stored = fresh_user(&pool, alice_id).await;
    assert_eq!(stored.name, "Alice stripped", "the profile still folded");
    assert!(
        stored.avatar_url.is_none(),
        "the avatar the lists strip never reaches the row: {:?}",
        stored.avatar_url
    );
    assert!(
        stored.banner_url.is_none(),
        "nor does the banner: {:?}",
        stored.banner_url
    );
}

/// Backdates a mirror's `updated_at` to `stamp` and hands back the row, so
/// a test can put the document's `updated` on either side of it.
async fn stored_as_of(pool: &PgPool, id: Uuid, stamp: &str) -> User {
    let at = parse_time(stamp).expect("the fixture stamp parses");
    let _aged = sqlx::query("UPDATE users SET updated_at = $2 WHERE id = $1")
        .bind(id)
        .bind(at)
        .execute(pool)
        .await
        .expect("the fixture backdates the mirror");
    fresh_user(pool, id).await
}

/// A profile saved twice in quick succession sends two `Update`s; the
/// second, the final state, lands even though the first just stamped the
/// mirror, and so does one right behind a fetch.
#[tokio::test]
async fn two_updates_inside_ten_seconds_both_apply() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let upserted = upsert_remote_user(&pool, parse_actor(&person()).expect("the fixture parses"))
        .await
        .expect("the mirror fixture stores");
    let alice_id = upserted.id;
    let just_fetched = stored_as_of(&pool, alice_id, "2026-01-01T00:00:00Z").await;
    assert!(
        just_fetched.last_fetched_at.is_some(),
        "the fetch stamped the mirror a moment ago"
    );

    let mut first = person();
    first["name"] = json!("First rewrite");
    first["updated"] = json!("2026-01-01T00:00:01Z");
    let after_fetch = update_actor(
        state.clone(),
        view("Update", "rapid-1", ALICE, first),
        just_fetched,
    )
    .await;
    assert_handled(&after_fetch);
    assert_eq!(
        fresh_user(&pool, alice_id).await.name,
        "First rewrite",
        "the update right after a fetch folds"
    );

    let mut second = person();
    second["name"] = json!("Second rewrite");
    second["updated"] = json!("2026-01-01T00:00:02Z");
    let stamped = stored_as_of(&pool, alice_id, "2026-01-01T00:00:01Z").await;
    let folded = update_actor(
        state.clone(),
        view("Update", "rapid-2", ALICE, second),
        stamped,
    )
    .await;
    assert_handled(&folded);
    assert_eq!(
        fresh_user(&pool, alice_id).await.name,
        "Second rewrite",
        "the delivery right behind a fold folds as well"
    );
}

/// A document whose `updated` is not after the mirror's `updated_at` is an
/// older (or the same) profile than the one stored, and writes nothing.
#[tokio::test]
async fn an_update_older_than_the_mirror_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice_id = mirror(&pool).await.id;
    let stored = stored_as_of(&pool, alice_id, "2026-01-02T00:00:00Z").await;

    let mut older = person();
    older["name"] = json!("Older rewrite");
    older["updated"] = json!("2026-01-01T00:00:00Z");
    let behind = update_actor(
        state.clone(),
        view("Update", "stale-1", ALICE, older),
        stored.clone(),
    )
    .await;
    assert!(
        ignored(behind).contains("no newer"),
        "an older document is ignored"
    );

    let mut same = person();
    same["name"] = json!("Same-instant rewrite");
    same["updated"] = json!("2026-01-02T00:00:00Z");
    let level = update_actor(state, view("Update", "stale-2", ALICE, same), stored).await;
    assert!(
        ignored(level).contains("no newer"),
        "a document dated the mirror's own instant is ignored too"
    );
    assert_eq!(
        fresh_user(&pool, alice_id).await.name,
        "Alice",
        "nothing landed"
    );
}

/// A document with no `updated` says nothing about its age, so it applies,
/// even straight after a fetch.
#[tokio::test]
async fn an_update_without_updated_applies() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let just_fetched =
        upsert_remote_user(&pool, parse_actor(&person()).expect("the fixture parses"))
            .await
            .expect("the mirror fixture stores");
    let alice_id = just_fetched.id;

    let mut document = person();
    document["name"] = json!("Undated rewrite");
    let folded = update_actor(
        state,
        view("Update", "undated-1", ALICE, document),
        just_fetched,
    )
    .await;
    assert_handled(&folded);
    assert_eq!(
        fresh_user(&pool, alice_id).await.name,
        "Undated rewrite",
        "the undated document folded"
    );
}

/// An `Update` whose object is not the sender's own document describes an
/// account the signer has no authority over, and an object that is only a
/// reference declares nothing to store: both ignored, mirror untouched.
#[tokio::test]
async fn an_update_of_another_actors_id_or_of_a_bare_id_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice_id = mirror(&pool).await.id;
    let mallory = remote_user(&pool, "mallory", "https://peer.example/users/mallory").await;

    let mut document = person();
    document["id"] = json!("https://peer.example/users/mallory");
    document["name"] = json!("Mallory wearing alice's profile");
    let foreign = update_actor(
        state.clone(),
        view("Update", "wrong-id-1", ALICE, document),
        fresh_user(&pool, alice_id).await,
    )
    .await;
    assert!(
        ignored(foreign).contains("object id"),
        "a document that is not the sender's own is ignored"
    );
    assert_eq!(
        fresh_user(&pool, alice_id).await.name,
        "Alice",
        "alice's mirror is untouched"
    );
    assert_eq!(
        fresh_user(&pool, mallory.id).await.name,
        "",
        "and so is the account the document names"
    );

    let referenced = update_actor(
        state.clone(),
        view("Update", "bare-id-1", ALICE, json!(ALICE)),
        fresh_user(&pool, alice_id).await,
    )
    .await;
    assert!(
        ignored(referenced).contains("bare reference"),
        "an Update of an id alone declares nothing to store"
    );
    assert_eq!(
        fresh_user(&pool, alice_id).await.name,
        "Alice",
        "and it rewrote nothing"
    );
}

/// A document that is not a usable actor, one whose nickname another
/// stored account owns, and an account this instance owns are all
/// ignored rather than retried: no redelivery makes any of them foldable.
#[tokio::test]
async fn an_unfoldable_update_is_ignored_not_retried() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = mirror(&pool).await;
    let alice_id = alice.id;
    let squatter = local_user(&pool, "mallory@peer.example").await;
    let ours = local_user(&pool, "alice-here").await;
    let ours_id = ours.id;

    let mut keyless = person();
    keyless["publicKey"] = json!(null);
    let broken = update_actor(
        state.clone(),
        view("Update", "invalid-1", ALICE, keyless),
        alice,
    )
    .await;
    assert!(
        ignored(broken).contains("valid actor document"),
        "a keyless document is refused"
    );

    let mut renamed = person();
    renamed["preferredUsername"] = json!("mallory");
    let takeover = update_actor(
        state.clone(),
        view("Update", "conflict-1", ALICE, renamed),
        ready_for_update(&pool, alice_id).await,
    )
    .await;
    assert!(
        ignored(takeover).contains("nickname is owned"),
        "a takeover-shaped nickname is refused"
    );
    assert_eq!(
        fresh_user(&pool, squatter.id).await.nickname,
        "mallory@peer.example",
        "the squatter's row was neither renamed nor overwritten"
    );

    let local = update_actor(
        state.clone(),
        view("Update", "local-1", ALICE, person()),
        ours,
    )
    .await;
    assert!(
        ignored(local).contains("this instance owns"),
        "a local account's profile is not an inbound activity's to write"
    );

    let kept = fresh_user(&pool, alice_id).await;
    assert_eq!(kept.name, "Alice", "nothing any of them carried landed");
    assert!(
        fresh_user(&pool, ours_id).await.is_active.get(),
        "and the local row is exactly as it was"
    );
}

/// `movedTo` and `alsoKnownAs` land when the document changes them, stay
/// put when it repeats them, and clear again when the peer takes them
/// back.
#[tokio::test]
async fn an_update_stores_moved_to_and_aliases() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let alice = mirror(&pool).await;
    let alice_id = alice.id;
    assert!(
        alice.moved_to_ap_id.is_none() && alice.also_known_as.is_empty(),
        "the fixture moved nowhere and has no aliases"
    );

    let mut document = person();
    document["movedTo"] = json!("https://new.example/users/alice");
    document["alsoKnownAs"] = json!(["https://old.example/users/alice"]);
    let folded = update_actor(
        state.clone(),
        view("Update", "moved-1", ALICE, document.clone()),
        alice,
    )
    .await;
    assert_handled(&folded);

    let moved = fresh_user(&pool, alice_id).await;
    assert_eq!(
        moved.moved_to_ap_id.as_deref(),
        Some("https://new.example/users/alice"),
        "the move target is stored"
    );
    assert_eq!(
        moved.also_known_as,
        ["https://old.example/users/alice"],
        "the alias list is stored"
    );
    assert!(
        moved.last_move_at.is_none(),
        "a remote account's move is mirrored, not performed here, so no cooldown stamp"
    );

    let repeated = update_actor(
        state.clone(),
        view("Update", "moved-2", ALICE, document),
        ready_for_update(&pool, alice_id).await,
    )
    .await;
    assert_handled(&repeated);
    let same = fresh_user(&pool, alice_id).await;
    assert_eq!(
        same.moved_to_ap_id.as_deref(),
        Some("https://new.example/users/alice"),
        "repeating the same profile leaves the target alone"
    );
    assert_eq!(
        same.also_known_as,
        ["https://old.example/users/alice"],
        "and the alias list"
    );

    let mut undone = person();
    undone["name"] = json!("Alice is back");
    let cleared = update_actor(
        state.clone(),
        view("Update", "moved-3", ALICE, undone),
        ready_for_update(&pool, alice_id).await,
    )
    .await;
    assert_handled(&cleared);
    let back = fresh_user(&pool, alice_id).await;
    assert_eq!(back.name, "Alice is back", "the profile update landed");
    assert!(
        back.moved_to_ap_id.is_none() && back.also_known_as.is_empty(),
        "a document that no longer names them clears the mirror's copy"
    );
}

/// A database that cannot answer is transient: the queue retries, and the
/// ledger row goes back for the retry to find.
#[tokio::test]
async fn an_update_against_a_dead_database_is_transient() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = mirror(&pool).await;

    let outcome = update_actor(
        broken_state().await,
        view("Update", "dead-1", ALICE, person()),
        alice,
    )
    .await;

    assert!(
        matches!(outcome, Err(FederationError::Transient(_))),
        "the mirror write failing asks for another attempt: {outcome:?}"
    );
}
