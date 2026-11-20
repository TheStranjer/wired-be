//! Scenario 4: the social graph — an open follow, a locked account's
//! pending request and its authorization, the block and its unblock, a
//! timed mute, an endorsement, a private note, a list with its members,
//! the domain block resolved against the loopback
//! [`MockPeer`](super::support::MockPeer), and the follower pages walked
//! by their `Link` cursors.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    ACCOUNTS, Account, LIST, MUTE_SECONDS, MockPeer, NOT_FOUND, NOTE, UNKNOWN_ID, World,
    assert_flags, assert_refused, entity_ids, link_cursor, relationship, sorted, string_field,
};
use crate::common::{self, TestResponse};

// ───────────────────────  Scenario 4: the social graph  ───────────────────────

/// The social graph end to end: an open follow granted on the spot, a
/// locked account's request answered from its own queue, a block that
/// severs and an unblock that does not restore, a mute that carries its
/// expiry, a domain block that reaches a remote account alone, an
/// endorsement, a private note, a list with its members, and the follower
/// and following pages walked by the `Link` cursors they answer with.
#[tokio::test]
async fn the_social_graph_walks_follows_blocks_mutes_lists_and_pages() {
    let db = common::test_db().await;
    let world = World::open(db.pool().clone()).await;
    let alice = world.register("alice").await;
    let bob = world.register("bob").await;
    let dave = world.register("dave").await;
    let carol = world.register_locked("carol").await;

    // Step 1: an unlocked account grants, a locked one receives a request.
    follow_state(&world, &alice, &bob, json!({"following": true})).await;
    follow_state(&world, &alice, &dave, json!({"following": true})).await;
    follow_state(&world, &alice, &carol, json!({"requested": true})).await;

    // Step 2: carol reads her queue and answers it; alice now follows her.
    requested_then_granted(&world, &alice, &carol).await;

    // Step 3: the block severs the open follow; the unblock is not its undo.
    block_then_unblock(&world, &alice, &bob).await;

    // Step 4: a mute with a duration carries its expiry on the listing.
    timed_mute(&world, &alice, &bob).await;

    // Step 5: the endorsement and the private note, on the followed account.
    endorsed_then_noted(&world, &alice, &carol).await;

    // Step 6: a list, and its members both ways round.
    list_with_members(&world, &alice, &carol).await;

    // Step 7: the domain block, whose flag only a remote account raises.
    domain_block(&world, &alice, &bob).await;

    // Step 8: the follower and following pages under their `Link` cursors.
    follower_pages(&world, &alice, &bob, &carol, &dave).await;
}

/// The request carol holds from alice, and her answer to it: the queue
/// names alice alone, the authorization answers `followed_by`, and alice's
/// own read of the pair is a granted follow from then on.
async fn requested_then_granted(world: &World, alice: &Account, carol: &Account) {
    let queue = common::api_get(&world.app, Some(&carol.token), "/api/v1/follow_requests").await;
    common::assert_answer(&queue, StatusCode::OK, "the follow-request queue");
    assert_eq!(
        entity_ids(&queue),
        vec![alice.id.clone()],
        "the locked account's queue holds alice's request and nothing else"
    );
    let answered = common::api_post(
        &world.app,
        Some(&carol.token),
        &format!("/api/v1/follow_requests/{}/authorize", alice.id),
        &Value::Null,
    )
    .await;
    common::assert_answer(&answered, StatusCode::OK, "the authorization");
    assert_eq!(
        answered.json_value()["followed_by"],
        json!(true),
        "carol is followed by alice now"
    );
    follow_state(world, alice, carol, json!({"following": true})).await;
}

/// The block, its listing, and the unblock that leaves the severed follow
/// severed: Mastodon's relationship flags are what a client sees, and the
/// pair's `following` flag does not come back on its own.
async fn block_then_unblock(world: &World, alice: &Account, bob: &Account) {
    let blocked = write(world, alice, bob, "block").await;
    common::assert_answer(&blocked, StatusCode::OK, "the block write");
    assert_flags(
        &blocked.json_value(),
        &json!({"blocking": true, "following": false}),
        "alice's relationship to the account she blocked",
    );
    let blocks = common::api_get(&world.app, Some(&alice.token), "/api/v1/blocks").await;
    common::assert_answer(&blocks, StatusCode::OK, "the blocks listing");
    assert_eq!(
        entity_ids(&blocks),
        vec![bob.id.clone()],
        "the listing names the blocked account"
    );
    let unblocked = write(world, alice, bob, "unblock").await;
    common::assert_answer(&unblocked, StatusCode::OK, "the unblock write");
    assert_flags(
        &unblocked.json_value(),
        &json!({"blocking": false, "following": false}),
        "unblocked, and the follow stayed severed",
    );
}

/// The follow write as `follower`, asserted to answer the flags in
/// `granted` beside the pair's default shape.
async fn follow_state(world: &World, follower: &Account, target: &Account, granted: Value) {
    let response = write(world, follower, target, "follow").await;
    common::assert_answer(&response, StatusCode::OK, "the follow write");
    assert_flags(
        &response.json_value(),
        &granted,
        &format!("the follow of {}", target.id),
    );
    let read = relationship(world, &follower.token, &target.id).await;
    assert_flags(
        &read,
        &granted,
        "the same pair read back through `relationships`",
    );
}

/// The body-less relationship write `verb` on `target` as `actor`.
async fn write(world: &World, actor: &Account, target: &Account, verb: &str) -> TestResponse {
    common::api_post(
        &world.app,
        Some(&actor.token),
        &format!("{ACCOUNTS}/{}/{verb}", target.id),
        &Value::Null,
    )
    .await
}

/// The mute with a `duration` and the notifications parameter off, and the
/// expiry it leaves on the caller's mutes page.
async fn timed_mute(world: &World, alice: &Account, bob: &Account) {
    let muted = common::api_post(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/mute", bob.id),
        &json!({"duration": MUTE_SECONDS, "notifications": false}),
    )
    .await;
    common::assert_answer(&muted, StatusCode::OK, "the timed mute");
    assert_flags(
        &muted.json_value(),
        &json!({"muting": true, "muting_notifications": false}),
        "the mute's own relationship answer, with its parameters read",
    );
    let listing = common::api_get(&world.app, Some(&alice.token), "/api/v1/mutes").await;
    common::assert_answer(&listing, StatusCode::OK, "the mutes listing");
    assert_eq!(
        entity_ids(&listing),
        vec![bob.id.clone()],
        "the listing names the muted account"
    );
    assert_ne!(
        listing.json_value()[0]["mute_expires_at"],
        Value::Null,
        "and carries the expiry the duration computed"
    );
    let unmuted = write(world, alice, bob, "unmute").await;
    common::assert_answer(&unmuted, StatusCode::OK, "the mute withdrawal");
    assert_flags(
        &unmuted.json_value(),
        &json!({"muting": false, "muting_notifications": false}),
        "the withdrawn mute",
    );
}

/// The endorsement of a followed account and its listing, the private note
/// and its clearing, each read back from the relationship the writes answer.
async fn endorsed_then_noted(world: &World, alice: &Account, carol: &Account) {
    let endorsed = write(world, alice, carol, "pin").await;
    common::assert_answer(&endorsed, StatusCode::OK, "the endorsement");
    assert_eq!(
        endorsed.json_value()["endorsed"],
        json!(true),
        "the relationship answers pinned"
    );
    let pinned = common::api_get(&world.app, Some(&alice.token), "/api/v1/endorsements").await;
    common::assert_answer(&pinned, StatusCode::OK, "the endorsement listing");
    assert_eq!(
        entity_ids(&pinned),
        vec![carol.id.clone()],
        "the profile's own list names the pinned account"
    );

    let noted = common::api_post(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/note", carol.id),
        &json!({"comment": NOTE}),
    )
    .await;
    common::assert_answer(&noted, StatusCode::OK, "the note write");
    assert_eq!(
        noted.json_value()["note"],
        NOTE,
        "the relationship carries it"
    );
    let read = relationship(world, &alice.token, &carol.id).await;
    assert_eq!(read["note"], NOTE, "and so does the batch read");

    let withdrawn = write(world, alice, carol, "unpin").await;
    common::assert_answer(&withdrawn, StatusCode::OK, "the endorsement withdrawal");
    assert_eq!(
        withdrawn.json_value()["endorsed"],
        json!(false),
        "unpinning takes the endorsement back"
    );
    let cleared = common::api_post(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/note", carol.id),
        &json!({"comment": ""}),
    )
    .await;
    common::assert_answer(&cleared, StatusCode::OK, "the note clearing");
    assert_eq!(
        cleared.json_value()["note"],
        "",
        "and a blank comment clears the note"
    );
}

/// A list with a member: the create, the add, the member page, the
/// account's own membership read, and the removal. A member that names no
/// account is the shared 404 the whole `/lists` family answers with.
async fn list_with_members(world: &World, alice: &Account, carol: &Account) {
    let created = common::api_post(
        &world.app,
        Some(&alice.token),
        "/api/v1/lists",
        &json!({"title": LIST}),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "the list create");
    assert_eq!(created.json_value()["title"], LIST, "the entity carries it");
    let list_id = common::entity_id(&created);
    let members = format!("/api/v1/lists/{list_id}/accounts");

    let added = common::api_post(
        &world.app,
        Some(&alice.token),
        &members,
        &json!({"account_ids": [carol.id]}),
    )
    .await;
    common::assert_answer(&added, StatusCode::OK, "the member add");
    let listed = common::api_get(&world.app, Some(&alice.token), &members).await;
    common::assert_answer(&listed, StatusCode::OK, "the member page");
    assert_eq!(
        entity_ids(&listed),
        vec![carol.id.clone()],
        "the member page names the added account"
    );
    let belongs = common::api_get(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/lists", carol.id),
    )
    .await;
    common::assert_answer(&belongs, StatusCode::OK, "the membership read");
    assert_eq!(
        entity_ids(&belongs),
        vec![list_id.clone()],
        "and the account's own membership read names the list"
    );

    let stranger = common::api_post(
        &world.app,
        Some(&alice.token),
        &members,
        &json!({"account_ids": [UNKNOWN_ID]}),
    )
    .await;
    assert_refused(
        &stranger,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a member that names no account",
    );
    let removed = common::api_delete(
        &world.app,
        Some(&alice.token),
        &members,
        &json!({"account_ids": [carol.id]}),
    )
    .await;
    common::assert_answer(&removed, StatusCode::OK, "the member removal");
    let empty = common::api_get(&world.app, Some(&alice.token), &members).await;
    common::assert_answer(&empty, StatusCode::OK, "the emptied member page");
    assert_eq!(empty.json_value(), json!([]), "the page is empty again");
}

/// The domain block over the whole API: the remote actor resolved from a
/// peer on loopback and followed, the block that flags it and only it, the
/// caller's own list of blocked domains, and the unblock that clears both.
async fn domain_block(world: &World, alice: &Account, bob: &Account) {
    let peer = MockPeer::start().await;
    let followed = common::api_post(
        &world.app,
        Some(&alice.token),
        "/api/v1/follows",
        &json!({"uri": peer.actor_url()}),
    )
    .await;
    common::assert_answer(&followed, StatusCode::OK, "the follow of a remote actor");
    let mirror_id = string_field(&followed.json_value(), "id");
    let before = relationship(world, &alice.token, &mirror_id).await;
    assert_eq!(
        before["domain_blocking"],
        json!(false),
        "the mirror is not domain-blocked yet: {before}"
    );

    let blocked = common::api_post(
        &world.app,
        Some(&alice.token),
        "/api/v1/domain_blocks",
        &json!({"domain": peer.authority()}),
    )
    .await;
    common::assert_answer(&blocked, StatusCode::OK, "the domain block");
    assert_eq!(blocked.json_value(), json!({}), "Pleroma's empty object");
    let listing = common::api_get(&world.app, Some(&alice.token), "/api/v1/domain_blocks").await;
    common::assert_answer(&listing, StatusCode::OK, "the blocked domains listing");
    assert_eq!(
        listing.json_value(),
        json!([peer.authority()]),
        "the blocked domains are the caller's own list"
    );
    let remote = relationship(world, &alice.token, &mirror_id).await;
    assert_eq!(
        remote["domain_blocking"],
        json!(true),
        "the remote mirror of that domain is now blocked: {remote}"
    );
    let local = relationship(world, &alice.token, &bob.id).await;
    assert_eq!(
        local["domain_blocking"],
        json!(false),
        "while a local account never is: {local}"
    );

    let lifted = common::api_delete(
        &world.app,
        Some(&alice.token),
        &format!("/api/v1/domain_blocks?domain={}", peer.authority()),
        &Value::Null,
    )
    .await;
    common::assert_answer(&lifted, StatusCode::OK, "the domain unblock");
    let after = common::api_get(&world.app, Some(&alice.token), "/api/v1/domain_blocks").await;
    common::assert_answer(&after, StatusCode::OK, "the emptied domain list");
    assert_eq!(after.json_value(), json!([]), "the list is empty again");
    let clear = relationship(world, &alice.token, &mirror_id).await;
    assert_eq!(
        clear["domain_blocking"],
        json!(false),
        "and the flag went with the block: {clear}"
    );
}

/// The following page walked with `limit=1` and the `Link` header's own
/// cursors, the follower page of the account that was followed, and the
/// empty page that ends the walk without a header at all.
async fn follower_pages(
    world: &World,
    alice: &Account,
    bob: &Account,
    carol: &Account,
    dave: &Account,
) {
    let following = format!("{ACCOUNTS}/{}/following", alice.id);
    let first = common::api_get(
        &world.app,
        Some(&alice.token),
        &format!("{following}?limit=1"),
    )
    .await;
    common::assert_answer(&first, StatusCode::OK, "the first following page");
    let newest = entity_ids(&first);
    assert_eq!(newest.len(), 1, "one row per page");
    let next = link_cursor(&first, "next");
    assert!(
        next.contains("max_id="),
        "the next cursor pages older: {next}"
    );
    assert!(
        link_cursor(&first, "prev").contains("min_id="),
        "and the prev cursor pages newer"
    );
    let second = common::api_get(&world.app, Some(&alice.token), &next).await;
    common::assert_answer(&second, StatusCode::OK, "the second following page");
    let older = entity_ids(&second);
    assert_eq!(older.len(), 1, "the walk reaches the second row");
    assert_ne!(newest, older, "and never repeats a row");
    let mut walked = newest;
    walked.extend(older);
    assert_eq!(
        sorted(&walked),
        sorted(&[carol.id.clone(), dave.id.clone()]),
        "the two pages together are the two follows — the severed one of \
         bob's is not among them"
    );

    let followers = common::api_get(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/followers", carol.id),
    )
    .await;
    common::assert_answer(&followers, StatusCode::OK, "the follower listing");
    assert_eq!(
        entity_ids(&followers),
        vec![alice.id.clone()],
        "the locked account was followed once, and answers that follower"
    );
    let empty = common::api_get(
        &world.app,
        Some(&alice.token),
        &format!("{ACCOUNTS}/{}/followers", bob.id),
    )
    .await;
    common::assert_answer(&empty, StatusCode::OK, "the empty follower listing");
    assert_eq!(
        empty.json_value(),
        json!([]),
        "the blocked-and-unblocked pair is over"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "and an empty page sends no Link header at all"
    );
}
