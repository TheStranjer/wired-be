//! The relation activities: `Follow`, `Block`, their `Accept`/`Reject`
//! answers, `Move`, `Add`, `Remove` and `Flag`.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::{
    TYPE_ACCEPT, TYPE_ADD, TYPE_BLOCK, TYPE_FLAG, TYPE_FOLLOW, TYPE_MOVE, TYPE_REJECT, TYPE_REMOVE,
    activity_id,
    contexts::{
        ActivityDocument, AnsweredActivity, FlagContext, TargetedActivityContext, address_values,
        document_json,
    },
};
use crate::config::Config;

/// The `Follow` whose activity id is `id`: `follower_ap_id` asks to
/// follow `target_ap_id`, who is the activity's object and the only
/// address `to` names — Pleroma's `FollowView`.
///
/// `id` is the follow row's stored `ap_id` (decision 6) — [`activity_id`]
/// of a uuid for a document minted now — and `published` is the follow's
/// creation moment.
#[must_use]
pub fn follow_json(
    config: &Config,
    id: &str,
    follower_ap_id: &str,
    target_ap_id: &str,
    published: &DateTime<Utc>,
) -> Value {
    let follow = FollowActivity {
        id,
        follower_ap_id,
        target_ap_id,
        published,
    };
    follow_json_to(config, &follow, &[target_ap_id])
}

/// The parts of one `Follow` document that [`follow_json_to`] renders.
#[derive(Debug, Clone, Copy)]
pub struct FollowActivity<'a> {
    /// The activity id: the follow row's stored `ap_id` (decision 6).
    pub id: &'a str,
    /// The actor asking to follow.
    pub follower_ap_id: &'a str,
    /// The followed actor, the activity's object.
    pub target_ap_id: &'a str,
    /// The follow's creation moment.
    pub published: &'a DateTime<Utc>,
}

/// [`follow_json`] addressed to `to` instead of the followed actor: the
/// relay handshake names the relay's *inbox*, the endpoint that must
/// receive the `Follow`.
#[must_use]
pub fn follow_json_to(config: &Config, follow: &FollowActivity<'_>, to: &[&str]) -> Value {
    document_json(ActivityDocument {
        config,
        id: follow.id.to_owned(),
        type_name: TYPE_FOLLOW,
        actor_ap_id: follow.follower_ap_id,
        object: json!(follow.target_ap_id),
        to: address_values(to),
        cc: Vec::new(),
        published: follow.published,
        extra: Vec::new(),
    })
}

/// The `Block` whose activity id is `id` (the block row's stored
/// `ap_id`): `blocker_ap_id` blocks `target_ap_id`.
///
/// Pleroma's `BlockView`: the blocked actor is the activity's object and
/// the only address `to` names, so the blocked peer learns of the block
/// and nobody else does.
#[must_use]
pub fn block_json(
    config: &Config,
    id: &str,
    blocker_ap_id: &str,
    target_ap_id: &str,
    published: &DateTime<Utc>,
) -> Value {
    document_json(ActivityDocument {
        config,
        id: id.to_owned(),
        type_name: TYPE_BLOCK,
        actor_ap_id: blocker_ap_id,
        object: json!(target_ap_id),
        to: vec![json!(target_ap_id)],
        cc: Vec::new(),
        published,
        extra: Vec::new(),
    })
}

/// The shared shape of the activities whose object is a full embedded
/// document: `type_name` answering under `answer`'s own addressing.
pub(super) fn answered_json(type_name: &'static str, answer: &AnsweredActivity<'_>) -> Value {
    document_json(ActivityDocument {
        config: answer.config,
        id: activity_id(answer.config, answer.id),
        type_name,
        actor_ap_id: answer.actor_ap_id,
        object: answer.answered.clone(),
        to: address_values(answer.to),
        cc: address_values(answer.cc),
        published: answer.published,
        extra: Vec::new(),
    })
}

/// The `Accept` that grants `answer`'s embedded `Follow`.
///
/// `actor_ap_id` — the followee — admits the follower, who is the only
/// address `to` names and must receive the grant to complete the
/// handshake.
#[must_use]
pub fn accept_json(answer: &AnsweredActivity<'_>) -> Value {
    answered_json(TYPE_ACCEPT, answer)
}

/// The `Reject` that refuses `answer`'s embedded `Follow`.
///
/// `actor_ap_id` — the followee — turns down the follower, who is the
/// only address `to` names and must receive the refusal to close the
/// handshake.
#[must_use]
pub fn reject_json(answer: &AnsweredActivity<'_>) -> Value {
    answered_json(TYPE_REJECT, answer)
}

/// The shared shape of the activities that carry a `target`: `type_name`
/// moving `ctx`'s object in or out of it under the caller's addressing.
fn targeted_json(type_name: &'static str, ctx: &TargetedActivityContext<'_>) -> Value {
    document_json(ActivityDocument {
        config: ctx.config,
        id: activity_id(ctx.config, ctx.id),
        type_name,
        actor_ap_id: ctx.actor_ap_id,
        object: json!(ctx.object),
        to: address_values(ctx.to),
        cc: address_values(ctx.cc),
        published: ctx.published,
        extra: vec![("target", json!(ctx.target))],
    })
}

/// The `Move` in `ctx`: `actor_ap_id` has moved — its own old actor id,
/// the object, onto the new actor id, the target — and tells the
/// addresses `ctx` names.
///
/// Pleroma's `ActivityPub.move`: `to` is the origin's followers
/// collection, whose remote entries the publisher expands when the fan-out
/// runs; local followers were already transferred by the move endpoint,
/// and remote ones are their own instances' to rewrite — this activity is
/// what those instances act on.
#[must_use]
pub fn move_json(ctx: &TargetedActivityContext<'_>) -> Value {
    targeted_json(TYPE_MOVE, ctx)
}

/// The `Add` in `ctx`: `actor_ap_id` featured the status at `object` in
/// the collection at `target`.
#[must_use]
pub fn add_json(ctx: &TargetedActivityContext<'_>) -> Value {
    targeted_json(TYPE_ADD, ctx)
}

/// The `Remove` in `ctx`: `actor_ap_id` withdrew the pin of the status at
/// `object` from the collection at `target`.
#[must_use]
pub fn remove_json(ctx: &TargetedActivityContext<'_>) -> Value {
    targeted_json(TYPE_REMOVE, ctx)
}

/// The `Flag` in `ctx`: the instance reports the account and statuses
/// `ctx.object_ids` names, with the reporter's comment as `content`.
///
/// Pleroma's `make_flag_data` reduced to the fields the ticket pins: the
/// `actor` is the instance actor (Mastodon's forwarded-report convention —
/// the instance vouches on the wire), and the recipient is the reported
/// account's inbox alone, which `ctx.to` names.
#[must_use]
pub fn flag_json(ctx: &FlagContext<'_>) -> Value {
    document_json(ActivityDocument {
        config: ctx.config,
        id: ctx.id.to_owned(),
        type_name: TYPE_FLAG,
        actor_ap_id: ctx.actor_ap_id,
        object: Value::Array(ctx.object_ids.iter().map(|id| json!(id)).collect()),
        to: address_values(ctx.to),
        cc: address_values(ctx.cc),
        published: ctx.published,
        extra: vec![("content", json!(ctx.content))],
    })
}

#[cfg(test)]
mod tests {
    use crate::federation::vocab::AS_PUBLIC;

    use super::super::{
        accept_json,
        test_support::{
            ACTIVITY_ID, ACTIVITY_KEYS, ALICE_AP, BASE, BOB_AP, FEATURED, FOLLOWERS, INSTANCE_AP,
            NO_CC, PUBLISHED, STATUS_URI, activity_uuid, config, expected_context, follow_of_bob,
            json_keys, published, stored_id,
        },
    };
    use super::*;

    #[test]
    fn a_follow_addresses_the_followed_actor_alone() {
        let document = follow_json(&config(), &stored_id(), BOB_AP, ALICE_AP, &published());
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Follow",
                "actor": BOB_AP,
                "object": ALICE_AP,
                "published": PUBLISHED,
                "to": [ALICE_AP],
                "cc": [],
            }),
            "the whole Follow: object and to are the followed actor"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn a_follow_renders_the_id_it_is_given_and_an_overriding_to() {
        let stored = "https://stored.example/activities/follow-row";
        let follow = FollowActivity {
            id: stored,
            follower_ap_id: BOB_AP,
            target_ap_id: ALICE_AP,
            published: &published(),
        };
        let document = follow_json_to(&config(), &follow, &["https://relay.example/inbox"]);
        assert_eq!(document["id"], json!(stored), "the stored id, verbatim");
        assert_eq!(
            document["object"],
            json!(ALICE_AP),
            "the object is unchanged"
        );
        assert_eq!(
            document["to"],
            json!(["https://relay.example/inbox"]),
            "to is exactly the override"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn a_block_addresses_the_blocked_actor_alone() {
        let document = block_json(&config(), &stored_id(), ALICE_AP, BOB_AP, &published());
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Block",
                "actor": ALICE_AP,
                "object": BOB_AP,
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [],
            }),
            "the whole Block: the blocked peer is the only addressee"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn an_accept_answers_the_followers_request() {
        let followed = follow_of_bob();
        let config = config();
        let published = published();
        let to = [BOB_AP];
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            to: &to,
            cc: &NO_CC,
            answered: &followed,
            published: &published,
        };
        let document = accept_json(&answer);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Accept",
                "actor": ALICE_AP,
                "object": followed,
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [],
            }),
            "the followee grants, addressing the follower"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn a_reject_refuses_the_followers_request() {
        let followed = follow_of_bob();
        let config = config();
        let published = published();
        let to = [BOB_AP];
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            to: &to,
            cc: &NO_CC,
            answered: &followed,
            published: &published,
        };
        let document = reject_json(&answer);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Reject",
                "actor": ALICE_AP,
                "object": followed,
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [],
            }),
            "the followee refuses, and only the follower learns"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn a_move_carries_the_origin_to_its_target() {
        let config = config();
        let published = published();
        let to = [FOLLOWERS];
        let document = move_json(&TargetedActivityContext {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            object: ALICE_AP,
            target: BOB_AP,
            to: &to,
            cc: &NO_CC,
            published: &published,
        });
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Move",
                "actor": ALICE_AP,
                "object": ALICE_AP,
                "target": BOB_AP,
                "published": PUBLISHED,
                "to": [FOLLOWERS],
                "cc": [],
            }),
            "the whole Move: object is the origin's own id, target the destination"
        );
        let mut keys = ACTIVITY_KEYS.to_vec();
        keys.push("target");
        keys.sort_unstable();
        assert_eq!(json_keys(&document), keys, "and nothing else");
    }

    #[test]
    fn a_flag_lists_the_reported_account_then_the_cited_statuses() {
        let config = config();
        let published = published();
        let id = stored_id();
        let objects = vec![
            BOB_AP.to_owned(),
            STATUS_URI.to_owned(),
            format!("{BASE}/objects/second-cited"),
        ];
        let to = [BOB_AP];
        let document = flag_json(&FlagContext {
            config: &config,
            id: &id,
            actor_ap_id: INSTANCE_AP,
            object_ids: &objects,
            content: "spam, again",
            to: &to,
            cc: &NO_CC,
            published: &published,
        });
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": id,
                "type": "Flag",
                "actor": INSTANCE_AP,
                "object": [BOB_AP, STATUS_URI, format!("{BASE}/objects/second-cited")],
                "content": "spam, again",
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [],
            }),
            "the whole Flag: the instance speaks, the account leads the \
             object list, and only it is addressed"
        );
        let mut keys = ACTIVITY_KEYS.to_vec();
        keys.push("content");
        keys.sort_unstable();
        assert_eq!(json_keys(&document), keys, "content, and nothing else");
    }

    #[test]
    fn a_pin_add_and_remove_share_the_feature_s_skeleton() {
        let config = config();
        let published = published();
        let to = [AS_PUBLIC];
        let cc = [FOLLOWERS];
        let ctx = TargetedActivityContext {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            object: STATUS_URI,
            target: FEATURED,
            to: &to,
            cc: &cc,
            published: &published,
        };
        let document = add_json(&ctx);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Add",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "target": FEATURED,
                "published": PUBLISHED,
                "to": [AS_PUBLIC],
                "cc": [FOLLOWERS],
            }),
            "the whole Add: the status into the featured collection"
        );
        let mut keys = ACTIVITY_KEYS.to_vec();
        keys.push("target");
        keys.sort_unstable();
        assert_eq!(json_keys(&document), keys, "and nothing else");

        let removal = remove_json(&ctx);
        assert_eq!(removal["type"], "Remove", "the withdrawal shares the shape");
        assert_eq!(removal["object"], json!(STATUS_URI));
        assert_eq!(removal["target"], json!(FEATURED));
        assert_eq!(json_keys(&removal), keys, "key for key");
    }
}
