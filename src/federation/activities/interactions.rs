//! The interaction activities: `Like`, `Announce`, `EmojiReact` and the
//! `Undo` that withdraws one.

use serde_json::{Value, json};

use super::{
    TYPE_ANNOUNCE, TYPE_EMOJI, TYPE_EMOJI_REACT, TYPE_IMAGE, TYPE_LIKE, TYPE_UNDO,
    contexts::{
        ActivityDocument, AnsweredActivity, EmojiReactContext, ObjectActivityContext,
        address_values, document_json, object_json,
    },
    relations::answered_json,
};

/// The reaction emoji as its `Emoji` tag, in the shape the note builder's
/// `emoji_tag` writes: the image URL as both `id` and `icon.url`, the
/// stored spelling (the `:shortcode:`) as the `name`.
fn react_emoji_tag(emoji: &str, url: &str) -> Value {
    json!({
        "type": TYPE_EMOJI,
        "id": url,
        "name": emoji,
        "icon": { "type": TYPE_IMAGE, "url": url },
    })
}

/// The `Like` in `ctx`: `actor_ap_id` favourited the status at
/// `object_uri`.
///
/// It travels under the caller's addressing: the outbound hook sends the
/// author in `to` and the actor's followers (plus the public address, for
/// a public status) in `cc`; the C2S route keeps this instance's
/// public-address convention. `published` is the favourite row's
/// `inserted_at`, `id` its stored `ap_id` (decision 6) — the id a matching
/// `Undo` embeds.
#[must_use]
pub fn like_json(ctx: &ObjectActivityContext<'_>) -> Value {
    object_json(TYPE_LIKE, ctx)
}

/// The `Announce` in `ctx`: `actor_ap_id` boosted the status whose object
/// id is `object_uri`, under the addressing the caller computed from the
/// boost's own visibility.
///
/// Decision 6: a reblog row *is* the activity, so `ctx.id` is the row's
/// `uri` — the id peers know the boost by, which their `Undo` of a
/// delivered one and a later fetch of it both name back. `published` is
/// the row's `inserted_at`.
#[must_use]
pub fn announce_json(ctx: &ObjectActivityContext<'_>) -> Value {
    object_json(TYPE_ANNOUNCE, ctx)
}

/// The `EmojiReact` behind the reaction row in `ctx`: `actor_ap_id`
/// reacted to the status at `object_uri` with the stored emoji, under the
/// addressing [`like_json`] is given.
///
/// Pleroma's answer to a `Like` that names its emoji in `content`. A
/// custom emoji (a `url` in `ctx`) also carries the emoji as a `tag`, in
/// the shape the note builder writes; a unicode reaction carries none.
#[must_use]
pub fn emoji_react_json(ctx: &EmojiReactContext<'_>) -> Value {
    let mut extra = vec![("content", json!(ctx.emoji))];
    if let Some(url) = ctx.emoji_url {
        extra.push(("tag", Value::Array(vec![react_emoji_tag(ctx.emoji, url)])));
    }
    document_json(ActivityDocument {
        config: ctx.config,
        id: ctx.id.to_owned(),
        type_name: TYPE_EMOJI_REACT,
        actor_ap_id: ctx.actor_ap_id,
        object: json!(ctx.object_uri),
        to: address_values(ctx.to),
        cc: address_values(ctx.cc),
        published: ctx.published,
        extra,
    })
}

/// The `Undo` that withdraws `answer`'s embedded activity.
///
/// `actor_ap_id` takes back what they once sent: unfavouriting,
/// unreacting, unboosting, unfollowing, unblocking, or withdrawing a
/// follow request that is still pending. The withdrawn activity rides
/// inside as an object, not a bare id, so the peer matches it without a
/// fetch, and the answer travels under the addressing `answer` carries —
/// for a withdrawn interaction, exactly the `to` and `cc` the activity
/// itself went out under.
#[must_use]
pub fn undo_json(answer: &AnsweredActivity<'_>) -> Value {
    answered_json(TYPE_UNDO, answer)
}

#[cfg(test)]
mod tests {
    use super::super::{
        AS_PUBLIC,
        test_support::{
            ACTIVITY_ID, ACTIVITY_KEYS, ALICE_AP, AUTHOR_TO, BASE, BOB_AP, EMOJI_URL, FOLLOWERS,
            FOLLOWERS_CC, PUBLIC_CC, PUBLIC_TO, PUBLISHED, STATUS_URI, activity_uuid, config,
            expected_context, follow_of_bob, json_keys, published, stored_id,
        },
    };
    use super::*;

    #[test]
    fn a_favourite_renders_the_like_its_maker_addressed() {
        let config = config();
        let published = published();
        let id = stored_id();
        let document = like_json(&ObjectActivityContext {
            config: &config,
            id: &id,
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            to: &AUTHOR_TO,
            cc: &PUBLIC_CC,
            published: &published,
        });
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": id,
                "type": "Like",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [FOLLOWERS, AS_PUBLIC],
            }),
            "the whole Like, field for field, under the caller's addressing"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn a_boost_renders_the_announce_under_its_stored_uri() {
        let config = config();
        let published = published();
        let id = stored_id();
        let document = announce_json(&ObjectActivityContext {
            config: &config,
            id: &id,
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            to: &PUBLIC_TO,
            cc: &PUBLIC_CC,
            published: &published,
        });
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": id,
                "type": "Announce",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "published": PUBLISHED,
                "to": [AS_PUBLIC],
                "cc": [FOLLOWERS, AS_PUBLIC],
            }),
            "the whole Announce: the id is the reblog row's own uri string"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }

    #[test]
    fn a_unicode_reaction_renders_content_without_a_tag() {
        let config = config();
        let published = published();
        let id = stored_id();
        let ctx = EmojiReactContext {
            config: &config,
            id: &id,
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            to: &AUTHOR_TO,
            cc: &FOLLOWERS_CC,
            emoji: "🎉",
            emoji_url: None,
            published: &published,
        };
        let document = emoji_react_json(&ctx);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": id,
                "type": "EmojiReact",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "content": "🎉",
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [FOLLOWERS],
            }),
            "a unicode reaction is a Like that says which emoji"
        );
        let mut keys = ACTIVITY_KEYS.to_vec();
        keys.push("content");
        keys.sort_unstable();
        assert_eq!(json_keys(&document), keys, "content, and no tag");
    }

    #[test]
    fn a_custom_reaction_carries_the_note_shapes_emoji_tag() {
        let config = config();
        let published = published();
        let id = stored_id();
        let ctx = EmojiReactContext {
            config: &config,
            id: &id,
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            to: &AUTHOR_TO,
            cc: &FOLLOWERS_CC,
            emoji: ":railroad_track:",
            emoji_url: Some(EMOJI_URL),
            published: &published,
        };
        let document = emoji_react_json(&ctx);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": id,
                "type": "EmojiReact",
                "actor": ALICE_AP,
                "object": STATUS_URI,
                "content": ":railroad_track:",
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [FOLLOWERS],
                "tag": [{
                    "type": "Emoji",
                    "id": EMOJI_URL,
                    "name": ":railroad_track:",
                    "icon": { "type": "Image", "url": EMOJI_URL },
                }],
            }),
            "the shortcode as content and the image as a tag"
        );
        let mut keys = ACTIVITY_KEYS.to_vec();
        keys.extend(["content", "tag"]);
        keys.sort_unstable();
        assert_eq!(json_keys(&document), keys, "content and tag, both");
    }

    #[test]
    fn an_undo_embeds_the_activity_it_withdraws_under_its_addressing() {
        let followed = follow_of_bob();
        let config = config();
        let published = published();
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: BOB_AP,
            to: &AUTHOR_TO,
            cc: &PUBLIC_CC,
            answered: &followed,
            published: &published,
        };
        let document = undo_json(&answer);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/activities/{ACTIVITY_ID}"),
                "type": "Undo",
                "actor": BOB_AP,
                "object": followed,
                "published": PUBLISHED,
                "to": [BOB_AP],
                "cc": [FOLLOWERS, AS_PUBLIC],
            }),
            "the withdrawn Follow rides inside as an object, and the \
             Undo travels under the addressing the answerer names — the \
             withdrawal of a delivered interaction repeats that \
             interaction's own lines"
        );
        assert_eq!(json_keys(&document), ACTIVITY_KEYS, "and nothing else");
    }
}
