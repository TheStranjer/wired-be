//! Scenario 2's second part: the `Keyword`, `Hellthread`, `ObjectAge` and
//! `Tag` policies, each configured through `POST /admin/config` and judged
//! against one delivered activity from the mock peer — the keyword note
//! replaced, the over-mention hellthread delisted and dropped, the
//! over-age note rejected, the tagged author's note forced unlisted — and
//! the five-policy chain read back off the instance document. The config
//! writes and delivery machinery are
//! [`mrf`](super::mrf)'s `pub(super)` helpers.

use axum::http::StatusCode;
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use wired_be::util::time::mastodon_time;

use super::mrf::{
    admin_entry, create_note, deliver, federation_block, set_mrf, wire_from, wrap_create,
};
use super::support::{BOB, USERS, World, assert_log};
use crate::{common, remote};

/// Scenario 2, part 2: the `Keyword`, `Hellthread`, `ObjectAge` and `Tag`
/// policies, each configured through `POST /admin/config` and judged
/// against one delivered activity — the keyword note replaced, the
/// over-mention hellthread delisted, the over-age note rejected, and the
/// tagged author's public note forced unlisted.
#[tokio::test]
async fn keyword_hellthread_object_age_and_tag_policies_judge_the_inbound_chain() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    keyword_replaces_a_note(&world, &peer, &actor).await;
    hellthread_delists_and_drops(&world, &peer, &actor).await;
    object_age_rejects_an_old_note(&world, &peer, &actor).await;
    tag_forces_unlisted(&world, &peer, &actor).await;

    // The final chain: all five policies named, and the transparency
    // reports every name in order.
    set_mrf(
        &world.app,
        &world.admin,
        &["simple", "keyword", "hellthread", "object_age", "tag"],
        &[],
    )
    .await;
    let federation = federation_block(&world.app).await;
    assert_eq!(
        federation["mrf_policies"],
        json!([
            "SimplePolicy",
            "KeywordPolicy",
            "HellthreadPolicy",
            "ObjectAgePolicy",
            "TagPolicy"
        ]),
        "the instance answers all five policies in chain order"
    );
}

/// The keyword `replace` list: a delivered note's matched word arrives
/// rewritten in the folded status.
async fn keyword_replaces_a_note(world: &World, peer: &remote::MockPeer, actor: &str) {
    let kw_rows: Vec<(&str, Value)> = vec![(
        ":mrf_keyword",
        wire_from(&json!({"replace": [["bad word", "b*tch"]]})),
    )];
    set_mrf(&world.app, &world.admin, &["keyword"], &kw_rows).await;
    let uri = peer.note("kw-replace");
    let kw_create = create_note(
        peer,
        actor,
        "kw-replace",
        "<p>say bad word out loud</p>",
        json!({}),
    );
    let kw_door = deliver(world, peer, &kw_create).await;
    common::assert_answer(
        &kw_door,
        StatusCode::ACCEPTED,
        "the replaced note is accepted",
    );
    let entry = admin_entry(world, &uri)
        .await
        .expect("the rewritten note folded");
    assert_eq!(
        entry["content"],
        json!("<p>say b*tch out loud</p>"),
        "the keyword policy rewrote the note's content"
    );
}

/// The hellthread thresholds: three recipients over the delist threshold
/// demote the note to unlisted; four over the reject threshold drop it
/// entirely.
async fn hellthread_delists_and_drops(world: &World, peer: &remote::MockPeer, actor: &str) {
    let ht_rows: Vec<(&str, Value)> = vec![(
        ":mrf_hellthread",
        wire_from(&json!({"delist_threshold": 2, "reject_threshold": 3})),
    )];
    set_mrf(&world.app, &world.admin, &["hellthread"], &ht_rows).await;

    let mentions = |count: usize| -> Vec<Value> {
        (0..count)
            .map(|i| json!(format!("https://mentioned.example/user{i}")))
            .collect()
    };
    let delist_uri = peer.note("ht-delist");
    let htd_create = wrap_create(
        create_note(
            peer,
            actor,
            "ht-delist",
            "<p>a crowd of mentions</p>",
            json!({}),
        ),
        json!({"to": [remote::PUBLIC], "cc": mentions(3)}),
    );
    let htd_door = deliver(world, peer, &htd_create).await;
    common::assert_answer(&htd_door, StatusCode::ACCEPTED, "the delisted note arrives");
    let entry = admin_entry(world, &delist_uri)
        .await
        .expect("the delisted note folded");
    assert_eq!(
        entry["visibility"],
        json!("unlisted"),
        "over the delist threshold: unlisted"
    );

    let hdr_create = wrap_create(
        create_note(peer, actor, "ht-drop", "<p>a larger crowd</p>", json!({})),
        json!({"to": [remote::PUBLIC], "cc": mentions(4)}),
    );
    let hdr_door = deliver(world, peer, &hdr_create).await;
    common::assert_answer(
        &hdr_door,
        StatusCode::ACCEPTED,
        "the door still 202s the chain drop",
    );
    assert!(
        admin_entry(world, &peer.note("ht-drop")).await.is_none(),
        "over the reject threshold the note never folds"
    );
}

/// The object-age gate: a note published before the configured timestamp
/// is rejected by the chain and never stored.
async fn object_age_rejects_an_old_note(world: &World, peer: &remote::MockPeer, actor: &str) {
    let oa_rows: Vec<(&str, Value)> = vec![(
        ":mrf_object_age",
        wire_from(&json!({"threshold": 604_800, "actions": ["reject"]})),
    )];
    set_mrf(&world.app, &world.admin, &["object_age"], &oa_rows).await;
    let oa_old_create = create_note(
        peer,
        actor,
        "oa-old",
        "<p>ancient</p>",
        json!({"published": "2020-01-01T00:00:00Z"}),
    );
    let oa_old_door = deliver(world, peer, &oa_old_create).await;
    common::assert_answer(
        &oa_old_door,
        StatusCode::ACCEPTED,
        "the door does not weigh ages",
    );
    assert!(
        admin_entry(world, &peer.note("oa-old")).await.is_none(),
        "the over-age note is rejected by the chain"
    );
    // A note published after the timestamp survives the same chain. Its
    // moment is read off the running clock — a day old, inside the
    // seven-day threshold whenever this test runs — so no pinned calendar
    // date can age it past the gate.
    let fresh_uri = peer.note("oa-fresh");
    let oa_fresh_create = create_note(
        peer,
        actor,
        "oa-fresh",
        "<p>recent</p>",
        json!({"published": mastodon_time(&(Utc::now() - ChronoDuration::days(1)))}),
    );
    let oa_fresh_door = deliver(world, peer, &oa_fresh_create).await;
    common::assert_answer(
        &oa_fresh_door,
        StatusCode::ACCEPTED,
        "a fresh delivery arrives",
    );
    assert!(
        admin_entry(world, &fresh_uri).await.is_some(),
        "the under-age note folds untouched"
    );
}

/// The tag switches: the author's mirror tagged `mrf_tag:force-unlisted`
/// (through the admin users surface, its `tag` entry logged) has its next
/// public note demoted to unlisted by the chain.
async fn tag_forces_unlisted(world: &World, peer: &remote::MockPeer, actor: &str) {
    set_mrf(&world.app, &world.admin, &["tag"], &[]).await;
    let tagged = common::api_put(
        &world.app,
        Some(&world.admin),
        &format!("{USERS}/tag"),
        &json!({"nicknames": [peer.handle(BOB)], "tags": ["mrf_tag:force-unlisted"]}),
    )
    .await;
    common::assert_answer(&tagged, StatusCode::NO_CONTENT, "the admin tag write");
    let uri = peer.note("tagged-public");
    let tag_create = create_note(
        peer,
        actor,
        "tagged-public",
        "<p>loud but hidden</p>",
        json!({}),
    );
    let tag_door = deliver(world, peer, &tag_create).await;
    common::assert_answer(
        &tag_door,
        StatusCode::ACCEPTED,
        "the tagged author's note arrives",
    );
    let entry = admin_entry(world, &uri)
        .await
        .expect("the tagged note folded");
    assert_eq!(
        entry["visibility"],
        json!("unlisted"),
        "the author's tag forces it unlisted"
    );
    assert_log(
        &world.app,
        &world.admin,
        "tag",
        &[format!(
            "@root added tags: mrf_tag:force-unlisted to users: @{}",
            peer.handle(BOB)
        )],
    )
    .await;
}
