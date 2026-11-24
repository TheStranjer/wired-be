//! The fixtures the `simple` children's tests share: the peer domains and
//! actor URIs the documents are built from, the policy and context builders
//! every case runs on, and the activity and profile fixtures they feed it.
//! Test-only.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{Map, Value, json};

use crate::federation::{
    mrf::{MrfConfig, MrfContext, MrfPolicy as _, MrfResult},
    vocab::AS_PUBLIC,
};

use super::{PARAMS_KEY, SimpleConfig, SimplePolicy};

/// The domain every peer fixture is delivered by.
pub(super) const PEER: &str = "peer.example";
/// This instance's own domain, as the context carries it.
pub(super) const LOCAL: &str = "test.example";
/// The actor the fixtures speak as.
pub(super) const ALICE: &str = "https://peer.example/users/alice";
/// That actor's followers collection, the `{actor}/followers`
/// convention the policy falls back to.
pub(super) const FOLLOWERS: &str = "https://peer.example/users/alice/followers";
/// The public address, full `ActivityStreams` spelling.
pub(super) const PUBLIC: &str = AS_PUBLIC;
/// A second recipient the fixtures keep in `cc`.
pub(super) const BOB: &str = "https://remote.example/users/bob";
/// The note a fixture `Create` announces.
pub(super) const NOTE: &str = "https://peer.example/objects/1";

/// The policy over the `simple` lists `lists` configures.
pub(super) fn policy(lists: &Value) -> SimplePolicy {
    SimplePolicy::new(Arc::new(SimpleConfig::from_params(Some(lists))))
}

/// The context of an instance whose own domain is [`LOCAL`], over the
/// same configuration the policy under test was built from. The
/// per-actor facts stay unloaded: `simple` reads none.
pub(super) fn ctx(lists: &Value) -> MrfContext {
    MrfContext {
        config: Arc::new(MrfConfig::assemble(
            vec![PARAMS_KEY.to_owned()],
            Map::from_iter([(PARAMS_KEY.to_owned(), lists.clone())]),
        )),
        now: Utc::now(),
        local_domain: LOCAL.to_owned(),
        author_follower_count: None,
        author_status_count: None,
        author_tags: Vec::new(),
        target_tags: Vec::new(),
    }
}

/// One policy run: the answer for `activity` under the lists `lists`
/// configure.
pub(super) fn filtered(lists: &Value, activity: Value) -> MrfResult {
    let ctx = ctx(lists);
    policy(lists).filter(&ctx, activity)
}

/// The accepted form of `activity`, with the rejection as the failure
/// message when the policy refused it.
pub(super) fn accepted(lists: &Value, activity: Value) -> Value {
    match filtered(lists, activity) {
        Ok(value) => value,
        Err(reason) => panic!("the policy refused the fixture: {reason}"),
    }
}

/// One actor-document run through the chain's actor stage.
pub(super) fn filtered_actor(lists: &Value, actor: Value) -> Value {
    let ctx = ctx(lists);
    policy(lists).filter_actor(&ctx, actor)
}

/// A public `Create` of a note by an account on `origin`: `to` names
/// the public address, `cc` the author's followers and one mention, on
/// the activity and on the embedded object alike.
pub(super) fn create_from(origin: &str) -> Value {
    let actor = format!("https://{origin}/users/alice");
    let followers = format!("{actor}/followers");
    json!({
        "id": format!("https://{origin}/activities/1"),
        "type": "Create",
        "actor": actor,
        "to": [PUBLIC],
        "cc": [followers, BOB],
        "object": {
            "id": format!("https://{origin}/objects/1"),
            "type": "Note",
            "attributedTo": actor,
            "to": [PUBLIC],
            "cc": [followers, BOB],
            "content": "<p>hello</p>",
        },
    })
}

/// The fixture `Create`, by `alice@peer.example`.
pub(super) fn create() -> Value {
    create_from(PEER)
}

/// The same `Create` with one image attachment on the note.
pub(super) fn create_with_media() -> Value {
    let mut activity = create();
    let _previous = activity["object"]
        .as_object_mut()
        .expect("the fixture embeds its note")
        .insert("attachment".to_owned(), json!([{"type": "Document", "mediaType": "image/png", "url": format!("https://{PEER}/media/1.png")}]));
    activity
}

/// A `Delete` of the fixture note by alice.
pub(super) fn delete() -> Value {
    json!({
        "id": format!("https://{PEER}/activities/2"),
        "type": "Delete",
        "actor": ALICE,
        "object": NOTE,
    })
}

/// An activity of `kind` by alice whose `object` is the bare reference
/// `object`.
pub(super) fn reacting(kind: &str, object: &str) -> Value {
    json!({
        "id": format!("https://{PEER}/activities/7"),
        "type": kind,
        "actor": ALICE,
        "to": [PUBLIC],
        "object": object,
    })
}

/// A `Flag` (a report) by alice.
pub(super) fn flag() -> Value {
    json!({
        "id": format!("https://{PEER}/activities/3"),
        "type": "Flag",
        "actor": ALICE,
        "object": [format!("https://{PEER}/objects/spam")],
    })
}

/// A `Follow` by alice: an activity the note rules must never touch.
pub(super) fn follow() -> Value {
    json!({
        "id": format!("https://{PEER}/activities/4"),
        "type": "Follow",
        "actor": ALICE,
        "object": format!("https://{LOCAL}/users/carol"),
    })
}

/// An actor document with the three images the removal lists strip.
pub(super) fn person(origin: &str) -> Value {
    json!({
        "id": format!("https://{origin}/users/alice"),
        "type": "Person",
        "preferredUsername": "alice",
        "inbox": format!("https://{origin}/users/alice/inbox"),
        "icon": {"type": "Image", "mediaType": "image/png", "url": format!("https://{origin}/alice.png")},
        "image": {"type": "Image", "url": format!("https://{origin}/banner.png")},
        "background": {"type": "Image", "url": format!("https://{origin}/bg.png")},
    })
}
