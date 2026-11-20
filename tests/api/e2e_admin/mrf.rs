//! Scenario 2's first part and its shared MRF machinery: the `simple`
//! policy's `followers_only` and `media_removal` rewrites and its `reject`
//! refusal at the inbox door, with the chain's transparency read through
//! `GET /api/v1/instance`, `domain_blocks` and `NodeInfo`. The policy
//! config write ([`set_mrf`]), the mock peer's `Create Note` builders and
//! the signed delivery and admin-ledger reads every MRF phase judges are
//! `pub(super)` helpers here, drawn on also by
//! [`mrf_policies`](super::mrf_policies) and
//! [`domain_block`](super::domain_block).

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};

use super::support::{
    BOB, LOOPBACK, P_ADMIN, PAST, PUBLIC_TIMELINE, REJECTED, World, carries, get_array,
    post_configs, wire,
};
use crate::{common, remote};

/// The config POST that sets `:mrf` to `policies` and rewrites every
/// `:mrf_*` policy row from scratch: the phases of the MRF scenario
/// replace the whole chain, not merge into the last one's rows.
pub(super) async fn set_mrf(app: &Router, token: &str, policies: &[&str], rows: &[(&str, Value)]) {
    let mrf_write = json!({
        "group": ":pleroma",
        "key": ":mrf",
        "value": wire(&[("policies", json!(policies))]),
    });
    let mut items: Vec<Value> = vec![
        json!({"group": ":pleroma", "key": ":mrf", "delete": true}),
        json!({"group": ":pleroma", "key": ":mrf_simple", "delete": true}),
        json!({"group": ":pleroma", "key": ":mrf_keyword", "delete": true}),
        json!({"group": ":pleroma", "key": ":mrf_hellthread", "delete": true}),
        json!({"group": ":pleroma", "key": ":mrf_object_age", "delete": true}),
        mrf_write,
    ];
    for (key, value) in rows {
        items.push(json!({"group": ":pleroma", "key": key, "value": value}));
    }
    let response = post_configs(app, token, json!(items)).await;
    common::assert_answer(&response, StatusCode::OK, "the MRF config write");
}

/// The `pleroma.metadata.federation` block of `GET /api/v1/instance`.
pub(super) async fn federation_block(app: &Router) -> Value {
    let response = common::api_get(app, None, "/api/v1/instance").await;
    common::assert_answer(&response, StatusCode::OK, "the instance document");
    response.json_value()["pleroma"]["metadata"]["federation"].clone()
}

// ───────────────────────  Scenario 2: the MRF  ───────────────────────

/// The `Note`-family object on the peer's origin named `name`, public by
/// default; every field of `extra` merges over the defaults.
fn note_object(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    content: &str,
    extra: Value,
) -> Value {
    let mut object = json!({
        "id": peer.note(name),
        "type": "Note",
        "attributedTo": actor,
        "to": [remote::PUBLIC],
        "cc": [],
        "content": content,
        "published": PAST,
    });
    merge(&mut object, extra);
    object
}

/// A `Create` of a public `Note` named `name` with `content`; `object_extra`
/// merges over the note.
pub(super) fn create_note(
    peer: &remote::MockPeer,
    actor: &str,
    name: &str,
    content: &str,
    object_extra: Value,
) -> Value {
    let object = note_object(peer, actor, name, content, object_extra);
    json!({
        "id": peer.activity_id(name),
        "type": "Create",
        "actor": actor,
        "object": object,
    })
}

/// Merges `addressing` over the envelope — where a hellthread test hangs its
/// recipient list, since the policy counts the *activity's* `to` and `cc`.
pub(super) fn wrap_create(envelope: Value, addressing: Value) -> Value {
    let mut envelope = envelope;
    merge(&mut envelope, addressing);
    envelope
}

/// Merges `extra`'s fields over `target`.
fn merge(target: &mut Value, extra: Value) {
    let (Some(map), Value::Object(additions)) = (target.as_object_mut(), extra) else {
        panic!("both documents are JSON objects");
    };
    for (key, value) in additions {
        let _previous = map.insert(key, value);
    }
}

/// Delivers `activity` to the shared inbox as the peer's `BOB`, drains the
/// fold, and hands the raw inbox answer back to the caller.
pub(super) async fn deliver(
    world: &World,
    peer: &remote::MockPeer,
    activity: &Value,
) -> common::TestResponse {
    let response = peer
        .deliver(&world.app, &peer.actor(BOB), remote::INBOX, activity)
        .await;
    let _drained = world.drain().await;
    response
}

/// Whether `entries` carry the status whose `uri` is `uri`.
pub(super) fn carries_uri(entries: &[Value], uri: &str) -> bool {
    carries(entries, "uri", uri)
}

/// The `GET {P_ADMIN}/statuses?godmode=true` ledger, narrowed to the entry
/// whose `uri` is `uri`.
pub(super) async fn admin_entry(world: &World, uri: &str) -> Option<Value> {
    let entries = get_array(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/statuses?godmode=true"),
    )
    .await;
    entries
        .into_iter()
        .find(|entry| entry["uri"].as_str() == Some(uri))
}

/// Scenario 2, part 1: the `simple` policy's three ticket-lists. A public
/// note with an attachment first folds untouched; `followers_only` and
/// `media_removal` then rewrite the next one to a private, media-less
/// post; `reject` refuses the following delivery at the inbox door; and
/// the chain's transparency reads through `GET /api/v1/instance` and
/// `NodeInfo` beside the domain's own `domain_blocks` telling.
#[tokio::test]
async fn simple_policy_rewrites_refuses_and_reports_itself() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let peer = remote::MockPeer::start().await;
    let actor = peer.actor(BOB);

    let clean_uri = peer.note("mrf-clean");
    let baseline_create = create_note(&peer, &actor, "mrf-clean", "<p>plain hello</p>", json!({}));
    let baseline_door = deliver(&world, &peer, &baseline_create).await;
    common::assert_answer(
        &baseline_door,
        StatusCode::ACCEPTED,
        "the baseline inbox answer",
    );
    let clean = admin_entry(&world, &clean_uri)
        .await
        .expect("the baseline note folded");
    assert_eq!(
        clean["visibility"],
        json!("public"),
        "the baseline is public"
    );

    simple_policy_followers_only_and_media_removal_rewrite(&world, &peer, &actor).await;
    simple_policy_refuses_and_reports_itself(&world, &peer, &actor).await;
}

/// The `followers_only` + `media_removal` lists: the next note arrives
/// private, media-less and off the public timeline.
async fn simple_policy_followers_only_and_media_removal_rewrite(
    world: &World,
    peer: &remote::MockPeer,
    actor: &str,
) {
    let rewrite_lists = json!({
        "followers_only": [[LOOPBACK, "press"]],
        "media_removal": [LOOPBACK],
    });
    let rewrite_rows: Vec<(&str, Value)> = vec![(":mrf_simple", wire_from(&rewrite_lists))];
    set_mrf(&world.app, &world.admin, &["simple"], &rewrite_rows).await;
    let limited_uri = peer.note("mrf-limited");
    let note_attachment = json!({
        "attachment": [{
            "type": "Document",
            "mediaType": "image/png",
            "url": "https://media.example/pic.png",
        }],
    });
    let rewrite_create = create_note(
        peer,
        actor,
        "mrf-limited",
        "<p>look, a photo</p>",
        note_attachment,
    );
    let rewrite_door = deliver(world, peer, &rewrite_create).await;
    common::assert_answer(
        &rewrite_door,
        StatusCode::ACCEPTED,
        "the rewrite is a 202 at the door",
    );
    let limited = admin_entry(world, &limited_uri)
        .await
        .expect("the rewritten note folded");
    assert_eq!(
        limited["visibility"],
        json!("private"),
        "followers_only lands the note private"
    );
    assert_eq!(
        limited["media_attachments"],
        json!([]),
        "media_removal strips the attachment"
    );
    assert!(
        !carries_uri(
            &get_array(&world.app, None, PUBLIC_TIMELINE).await,
            &limited_uri
        ),
        "the followers-only note is off the public timeline"
    );
}

/// Scenario 2, part 1: the `simple` policy with no list writes folds a
/// plain note in untouched; a `reject` entry then refuses the next delivery at
/// the inbox door; and the chain's transparency reads through
/// `GET /api/v1/instance`, `NodeInfo` and `domain_blocks` alike.
async fn simple_policy_refuses_and_reports_itself(
    world: &World,
    peer: &remote::MockPeer,
    actor: &str,
) {
    // The rejection list refuses the *next* delivery at the door.
    let reject_lists = json!({"reject": [[LOOPBACK, "spammers"]]});
    let reject_rows: Vec<(&str, Value)> = vec![(":mrf_simple", wire_from(&reject_lists))];
    set_mrf(&world.app, &world.admin, &["simple"], &reject_rows).await;
    let refuse_create = create_note(peer, actor, "mrf-refused", "<p>spam</p>", json!({}));
    let refuse_door = deliver(world, peer, &refuse_create).await;
    common::assert_answer(
        &refuse_door,
        StatusCode::FORBIDDEN,
        "the blocked host is refused",
    );
    common::assert_error(&refuse_door, REJECTED, "the door's word");
    assert!(
        admin_entry(world, &peer.note("mrf-refused"))
            .await
            .is_none(),
        "the refused note never folded"
    );
    // Transparency: the instance's federation block, the domain_blocks
    // twin and the NodeInfo copy all tell the configured chain.
    let federation = federation_block(&world.app).await;
    assert_eq!(
        federation["mrf_policies"],
        json!(["SimplePolicy"]),
        "the named chain"
    );
    assert_eq!(
        federation["mrf_simple"]["reject"],
        json!([LOOPBACK]),
        "the published reject list"
    );
    assert_eq!(
        federation["mrf_simple_info"]["reject"][LOOPBACK]["reason"],
        json!("spammers"),
        "the reason rides beside it"
    );
    let blocks = get_array(&world.app, None, "/api/v1/instance/domain_blocks").await;
    assert!(
        blocks
            .iter()
            .any(|row| row["domain"].as_str() == Some(LOOPBACK)
                && row["comment"].as_str() == Some("spammers")),
        "domain_blocks renders the reject list: {blocks:?}"
    );
    let nodeinfo = common::api_get(&world.app, None, "/nodeinfo/2.0").await;
    common::assert_answer(&nodeinfo, StatusCode::OK, "the NodeInfo document");
    assert_eq!(
        nodeinfo.json_value()["metadata"]["federation"]["mrf_policies"],
        json!(["SimplePolicy"]),
        "NodeInfo tells the same chain"
    );
}

/// The wire form of a whole policy-parameter object: its keys as
/// `{"tuple": [key, value]}` entries.
pub(super) fn wire_from(object: &Value) -> Value {
    object
        .as_object()
        .expect("a policy parameter object")
        .iter()
        .map(|(key, value)| json!({"tuple": [key, value]}))
        .collect()
}
