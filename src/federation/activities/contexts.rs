//! The context structs the activity builders render from and the shared
//! document skeleton they all go through.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

#[cfg(doc)]
use super::{
    accept_json, activity_id, add_json, announce_json, emoji_react_json, flag_json, like_json,
    move_json, reject_json, remove_json, undo_json,
};
use crate::{config::Config, federation::note::json_ld_context, util::time::mastodon_time};

/// The inputs [`like_json`] and [`announce_json`] render from.
///
/// One object-addressed activity: the acting account, the object the
/// activity names, the id the row stores as the activity's own, the
/// addressing the caller computed from the acted-on status, and the row's
/// moment — read in as plain borrows so the builder stays pure, the way
/// [`NoteContext`](crate::federation::note::NoteContext) is for notes.
#[derive(Debug)]
pub struct ObjectActivityContext<'a> {
    /// The instance whose origin and JSON-LD schema frame the document.
    pub config: &'a Config,
    /// The document's `id`: the row's stored `ap_id` (decision 6), which
    /// for a reblog row is its `uri` — the id peers know the activity by.
    pub id: &'a str,
    /// The acting account's actor id.
    pub actor_ap_id: &'a str,
    /// The object the activity names: the liked/boosted status' `uri`.
    pub object_uri: &'a str,
    /// The primary addressing line, in order.
    pub to: &'a [&'a str],
    /// The copy line, in order.
    pub cc: &'a [&'a str],
    /// The row's creation moment, written as `published`.
    pub published: &'a DateTime<Utc>,
}

/// Everything [`emoji_react_json`] renders one reaction's document from.
///
/// The two ends of the pair, the addressing [`like_json`] is given, plus
/// the stored emoji and its image URL — the
/// [`EmojiReaction`](crate::models::emoji_reaction::EmojiReaction) row's
/// `emoji` and `url` columns — read in by the caller so the builder itself
/// stays pure, the way [`NoteContext`](crate::federation::note::NoteContext)
/// is for notes.
#[derive(Debug)]
pub struct EmojiReactContext<'a> {
    /// The instance whose origin and JSON-LD schema frame the document.
    pub config: &'a Config,
    /// The document's `id`: the reaction row's stored `ap_id` (decision 6).
    pub id: &'a str,
    /// The reacting account's actor id.
    pub actor_ap_id: &'a str,
    /// The reacted-to status' object id.
    pub object_uri: &'a str,
    /// The primary addressing line, in order.
    pub to: &'a [&'a str],
    /// The copy line, in order.
    pub cc: &'a [&'a str],
    /// The emoji as stored: one unicode grapheme, or a custom emoji
    /// written `:shortcode:`.
    pub emoji: &'a str,
    /// The custom emoji's image URL — `Some` for a custom emoji only, and
    /// what makes [`emoji_react_json`] carry an `Emoji` tag; `None` (a
    /// unicode reaction) writes none.
    pub emoji_url: Option<&'a str>,
    /// The reaction's `inserted_at`, written as the activity's
    /// `published`.
    pub published: &'a DateTime<Utc>,
}

/// The inputs [`undo_json`], [`accept_json`] and [`reject_json`] share.
///
/// One actor answering an activity — a `Follow` with an `Accept` or a
/// `Reject`, or any sent activity with an `Undo` — carrying it inside as an
/// embedded object rather than naming it by id, and repeating the
/// addressing the answer must travel under: the two follow answers name
/// the other actor of the pair, and the withdrawal of a delivered activity
/// repeats that activity's own `to` and `cc`, so exactly the inboxes that
/// heard the fact hear its taking-back.
#[derive(Debug)]
pub struct AnsweredActivity<'a> {
    /// The instance whose origin and JSON-LD schema mint the document.
    pub config: &'a Config,
    /// The `UUIDv7` naming this answer, which also names it (decision 6).
    pub id: Uuid,
    /// The acting actor's id: the followee who answers, or the actor who
    /// withdraws.
    pub actor_ap_id: &'a str,
    /// The answer's primary addressing line, in order.
    pub to: &'a [&'a str],
    /// The answer's copy line, in order.
    pub cc: &'a [&'a str],
    /// The activity being answered or withdrawn, embedded as the object.
    pub answered: &'a Value,
    /// The answer row's creation moment, written as `published`.
    pub published: &'a DateTime<Utc>,
}

/// The inputs [`move_json`], [`add_json`] and [`remove_json`] share.
///
/// One activity that moves `object` in or out of `target`: the `Move`
/// carries the origin's own actor id (as `object`) onto the account it
/// moved to (as `target`), and the pin pair features the pinned status'
/// object uri (as `object`) in the actor's featured collection (as
/// `target`) or withdraws it. The three documents are one skeleton under a
/// different `type`, the way the object-addressed builders are.
#[derive(Debug)]
pub struct TargetedActivityContext<'a> {
    /// The instance whose origin and JSON-LD schema mint the document.
    pub config: &'a Config,
    /// The `UUIDv7` naming this activity, which also names it (decision 6).
    pub id: Uuid,
    /// The acting account's actor id.
    pub actor_ap_id: &'a str,
    /// The object: the moved account's actor id for a `Move`, the pinned
    /// status' uri for an `Add` or `Remove`.
    pub object: &'a str,
    /// The target: the destination actor id for a `Move`, the actor's
    /// featured collection url for an `Add` or `Remove`.
    pub target: &'a str,
    /// The primary addressing line, in order.
    pub to: &'a [&'a str],
    /// The copy line, in order.
    pub cc: &'a [&'a str],
    /// The moment of the write behind the activity, written as
    /// `published`.
    pub published: &'a DateTime<Utc>,
}

/// Everything [`flag_json`] renders one forwarded report's document from.
///
/// The instance speaking for its reporter: the object array names the
/// reported account first and the cited statuses' object uris after it,
/// exactly as Pleroma's `build_flag_object` lays them out, and `content`
/// carries the reporter's comment.
#[derive(Debug)]
pub struct FlagContext<'a> {
    /// The instance whose origin and JSON-LD schema frame the document.
    pub config: &'a Config,
    /// The document's `id`: the report row's stored `ap_id` (decision 6),
    /// the id [`crate::models::report::create`] minted for it.
    pub id: &'a str,
    /// The instance actor's id: a forwarded report travels as the
    /// instance's own statement, not the reporter's.
    pub actor_ap_id: &'a str,
    /// The `object` array, in order: the reported account's actor id
    /// first, then the cited statuses' object uris.
    pub object_ids: &'a [String],
    /// The reporter's comment, verbatim.
    pub content: &'a str,
    /// The primary addressing line, in order.
    pub to: &'a [&'a str],
    /// The copy line, in order.
    pub cc: &'a [&'a str],
    /// The report's `inserted_at`, written as the activity's `published`.
    pub published: &'a DateTime<Utc>,
}

/// The skeleton every document in this module renders through: the public
/// builders fill one of these in and [`document_json`] turns it into the
/// uniform shape the module docs promise, so the key set and the
/// `@context`/`to`/`cc` convention hold by construction.
#[derive(Debug)]
pub(super) struct ActivityDocument<'a> {
    /// The instance whose origin and JSON-LD schema mint the document.
    pub(super) config: &'a Config,
    /// The document's own id, spelled out: every builder either mints it
    /// with [`activity_id`] or takes the caller's stored string.
    pub(super) id: String,
    /// The activity type, one of this module's `TYPE_*` constants.
    pub(super) type_name: &'static str,
    /// The acting actor's id.
    pub(super) actor_ap_id: &'a str,
    /// The object: an id string, or an embedded activity document.
    pub(super) object: Value,
    /// The primary addressing line.
    pub(super) to: Vec<Value>,
    /// The copy line.
    pub(super) cc: Vec<Value>,
    /// The row's creation moment, written as `published`.
    pub(super) published: &'a DateTime<Utc>,
    /// The type's own extra fields — `content` and `tag` — in render
    /// order after the shared ones.
    pub(super) extra: Vec<(&'static str, Value)>,
}

/// `ctx` as the uniform activity document every builder here returns.
///
/// The JSON-LD `@context` first, then `id`, `type`, `actor`, `object`,
/// `published`, `to` and `cc`, plus [`ActivityDocument`]'s extras.
pub(super) fn document_json(ctx: ActivityDocument<'_>) -> Value {
    let mut fields = vec![
        ("@context", json_ld_context(ctx.config)),
        ("id", json!(ctx.id)),
        ("type", json!(ctx.type_name)),
        ("actor", json!(ctx.actor_ap_id)),
        ("object", ctx.object),
        ("published", json!(mastodon_time(ctx.published))),
        ("to", Value::Array(ctx.to)),
        ("cc", Value::Array(ctx.cc)),
    ];
    fields.extend(ctx.extra);
    Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

/// One addressing line as document entries.
pub(super) fn address_values(line: &[&str]) -> Vec<Value> {
    line.iter().map(|address| json!(*address)).collect()
}

/// The shared shape of the object-addressed activities: `type_name` naming
/// `ctx`'s object id under the caller's addressing and stored id.
pub(super) fn object_json(type_name: &'static str, ctx: &ObjectActivityContext<'_>) -> Value {
    document_json(ActivityDocument {
        config: ctx.config,
        id: ctx.id.to_owned(),
        type_name,
        actor_ap_id: ctx.actor_ap_id,
        object: json!(ctx.object_uri),
        to: address_values(ctx.to),
        cc: address_values(ctx.cc),
        published: ctx.published,
        extra: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::{
        accept_json, announce_json, block_json, emoji_react_json, follow_json, like_json,
        reject_json,
        test_support::{
            ALICE_AP, AUTHOR_TO, BOB_AP, FOLLOWERS_CC, NO_CC, PUBLIC_CC, STATUS_URI, activity_uuid,
            config, follow_of_bob, lifecycle_documents, published, stored_id,
        },
        undo_json,
    };
    use super::*;

    #[test]
    fn every_document_starts_with_the_shared_note_context() {
        let config = config();
        let published = published();
        let followed = follow_of_bob();
        let id = stored_id();
        let answer = AnsweredActivity {
            config: &config,
            id: activity_uuid(),
            actor_ap_id: ALICE_AP,
            to: &AUTHOR_TO,
            cc: &NO_CC,
            answered: &followed,
            published: &published,
        };
        let react_ctx = EmojiReactContext {
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
        let object_ctx = ObjectActivityContext {
            config: &config,
            id: &id,
            actor_ap_id: ALICE_AP,
            object_uri: STATUS_URI,
            to: &AUTHOR_TO,
            cc: &PUBLIC_CC,
            published: &published,
        };
        let documents = [
            ("Like", like_json(&object_ctx)),
            ("Announce", announce_json(&object_ctx)),
            ("EmojiReact", emoji_react_json(&react_ctx)),
            (
                "Follow",
                follow_json(&config, activity_uuid(), BOB_AP, ALICE_AP, &published),
            ),
            (
                "Block",
                block_json(&config, activity_uuid(), ALICE_AP, BOB_AP, &published),
            ),
            ("Undo", undo_json(&answer)),
            ("Accept", accept_json(&answer)),
            ("Reject", reject_json(&answer)),
        ];
        let mut documents: Vec<(&str, Value)> = documents.into_iter().collect();
        documents.extend(lifecycle_documents(&config, &id, &published));
        for (name, document) in documents {
            assert_eq!(
                document["@context"],
                json_ld_context(&config),
                "the {name} document shares the note module's context"
            );
            let first_key = document
                .as_object()
                .expect("a document is a JSON object")
                .keys()
                .next()
                .expect("and carries at least the context");
            assert_eq!(first_key, "@context", "the {name} document starts with it");
            assert!(
                document.get("to").is_some() && document.get("cc").is_some(),
                "the {name} document carries both addressing keys"
            );
        }
    }
}
