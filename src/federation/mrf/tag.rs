//! Pleroma's `TagPolicy`: user tags as moderation switches.
//!
//! The reference is `lib/pleroma/web/activity_pub/mrf/tag_policy.ex` —
//! "apply policies based on user tags". An account's `users.tags` array (the
//! `pleroma.tags` field of the Account entity, set by moderation) carries
//! `mrf_tag:` entries, and each entry switches an action on that user's
//! activities:
//!
//! * `mrf_tag:media-force-nsfw` — mark a note that carries attachments
//!   `sensitive`;
//! * `mrf_tag:media-strip` — drop the attachments of those notes;
//! * `mrf_tag:force-unlisted` — a public note becomes unlisted: the public
//!   address moves out of `to` into `cc` and the followers collection takes
//!   its place in `to`, exactly the swap Pleroma computes;
//! * `mrf_tag:sandbox` — the note becomes followers-only: the public
//!   address goes out of both fields and the author's followers collection
//!   joins `to` (the same rewrite the [`simple`](super::simple) policy's
//!   `followers_only` list performs, reached by tag instead of by domain);
//! * `mrf_tag:disable-remote-subscription` — a `Follow` naming a target
//!   whose tags carry it is refused when its actor is not this instance's
//!   own;
//! * `mrf_tag:disable-any-subscription` — that `Follow` is refused whoever
//!   sends it.
//!
//! ```json
//! {"policies": ["tag"]}
//! ```
//!
//! The tags themselves are not configured: they are read from the database
//! into the [`MrfContext`] as `author_tags` (the activity actor's tags,
//! weighed on `Create`/`Update`) and `target_tags` (the local `Follow`
//! target's tags, weighed on `Follow`), preloaded by the chain runner only
//! while this policy (registered as `tag`) is enabled. A document that is
//! neither shape — and an activity whose actor or target carries none of
//! the six tags — passes untouched. Like Pleroma, the tags are applied in
//! the order the row stores them: a user tagged both `media-force-nsfw`
//! and `media-strip` in that order gets a note marked *and* stripped,
//! while the reverse order strips first and finds nothing left to mark.
//!
//! The remote-subscription rule reads remoteness off the `Follow`'s actor
//! URL against [`MrfContext::local_domain`] rather than a fetched user row:
//! the policy runs synchronously and IO-free, and an actor whose host is
//! this instance's own is the only one that can be a local account.

use serde_json::{Map, Value, json};

use crate::federation::{
    json,
    mrf::{
        MrfContext, MrfPolicy, MrfResult, Transparency,
        simple::{
            activity_host, activity_type, add_address, addresses_public, followers_collection,
            has_attachments, is_note_activity, limit_document, note_object_mut, remove_public,
            remove_reference, rewrite_each_document,
        },
        single_block,
    },
    vocab::AS_PUBLIC,
};

/// The key of this policy's parameters inside the `mrf` instance setting,
/// and the name [`build_chain`](super::build_chain) resolves.
pub const PARAMS_KEY: &str = "tag";

/// The `ActivityStreams` type whose note the content rules act on, beside
/// `Update`.
const CREATE_TYPE: &str = "Create";
/// The other wrapping type the content rules act on.
const UPDATE_TYPE: &str = "Update";
/// The activity type the subscription rules act on.
const FOLLOW_TYPE: &str = "Follow";

/// One of the six `mrf_tag:` switches Pleroma's `TagPolicy` knows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tag {
    /// `mrf_tag:media-force-nsfw`: notes with attachments are marked
    /// `sensitive`.
    MediaForceNsfw,
    /// `mrf_tag:media-strip`: notes lose their attachments.
    MediaStrip,
    /// `mrf_tag:force-unlisted`: public notes become unlisted.
    ForceUnlisted,
    /// `mrf_tag:sandbox`: public notes become followers-only.
    Sandbox,
    /// `mrf_tag:disable-remote-subscription`: remote users may not follow
    /// the tagged account.
    DisableRemoteSubscription,
    /// `mrf_tag:disable-any-subscription`: nobody may follow the tagged
    /// account.
    DisableAnySubscription,
}

impl Tag {
    /// Every recognized tag, in the order Pleroma documents them.
    pub const ALL: [Self; 6] = [
        Self::MediaForceNsfw,
        Self::MediaStrip,
        Self::ForceUnlisted,
        Self::Sandbox,
        Self::DisableRemoteSubscription,
        Self::DisableAnySubscription,
    ];

    /// The full tag string, `mrf_tag:` prefix included — the spelling
    /// stored in `users.tags` and shown to clients.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::MediaForceNsfw => "mrf_tag:media-force-nsfw",
            Self::MediaStrip => "mrf_tag:media-strip",
            Self::ForceUnlisted => "mrf_tag:force-unlisted",
            Self::Sandbox => "mrf_tag:sandbox",
            Self::DisableRemoteSubscription => "mrf_tag:disable-remote-subscription",
            Self::DisableAnySubscription => "mrf_tag:disable-any-subscription",
        }
    }

    /// The switch one stored tag string names, `None` for anything else:
    /// a `users.tags` array may hold ordinary profile tags, and they
    /// switch nothing.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tag| tag.key() == key)
    }
}

/// The tag-driven stage: Pleroma's `MRF.TagPolicy`.
#[derive(Clone, Debug)]
pub struct TagPolicy {
    /// The parameter block this policy was configured with, for
    /// [`describe`](MrfPolicy::describe); the filter itself reads nothing.
    params: Option<Value>,
}

impl TagPolicy {
    /// The policy beside the `tag` key of the `mrf` setting (`None` when
    /// the setting carries none).
    #[must_use]
    pub const fn new(params: Option<Value>) -> Self {
        Self { params }
    }
}

impl MrfPolicy for TagPolicy {
    fn name(&self) -> &'static str {
        "TagPolicy"
    }

    fn filter(&self, ctx: &MrfContext, activity: Value) -> MrfResult {
        let kind = activity_type(&activity);
        if matches!(kind.as_deref(), Some(FOLLOW_TYPE)) {
            return refuse_follow(ctx, activity);
        }
        if matches!(kind.as_deref(), Some(CREATE_TYPE | UPDATE_TYPE)) {
            return Ok(rewrite_note(ctx, activity));
        }
        Ok(activity)
    }

    fn describe(&self, _transparency: &Transparency) -> Map<String, Value> {
        single_block(
            self.name(),
            self.params.clone().unwrap_or_else(|| json!({})),
        )
    }
}

/// Folds the author's tags over their note activity, Pleroma's
/// `Enum.reduce`: every recognized tag rewrites the document the previous
/// one left, unrecognized tags rewrite nothing.
fn rewrite_note(ctx: &MrfContext, mut activity: Value) -> Value {
    for name in &ctx.author_tags {
        match Tag::from_key(name) {
            Some(Tag::MediaForceNsfw) => mark_media_sensitive(&mut activity),
            Some(Tag::MediaStrip) => strip_media(&mut activity),
            Some(Tag::ForceUnlisted) => force_unlisted(&mut activity),
            Some(Tag::Sandbox) => sandbox(&mut activity),
            Some(Tag::DisableRemoteSubscription | Tag::DisableAnySubscription) | None => {}
        }
    }
    activity
}

/// Refuses a `Follow` the target's tags forbid — the subscription rules
/// read `target_tags`, the local account the activity addresses. A target
/// that is not stored, or stores none of the two tags, is followed freely;
/// so is a local follower of an account that only refuses remotes.
fn refuse_follow(ctx: &MrfContext, activity: Value) -> MrfResult {
    for name in &ctx.target_tags {
        let tag = Tag::from_key(name);
        if tag == Some(Tag::DisableAnySubscription) {
            return Err(subscription_refusal(Tag::DisableAnySubscription, &activity));
        }
        if tag == Some(Tag::DisableRemoteSubscription) && is_remote_author(ctx, &activity) {
            return Err(subscription_refusal(
                Tag::DisableRemoteSubscription,
                &activity,
            ));
        }
    }
    Ok(activity)
}

/// Pleroma's `[TagPolicy] Follow from #{actor} tagged with <tag>`: the
/// sending actor's id names the request that was refused.
fn subscription_refusal(tag: Tag, activity: &Value) -> String {
    let actor = activity
        .get("actor")
        .and_then(json::value_or_id)
        .unwrap_or_default();
    format!("[TagPolicy] Follow from {actor} tagged with {}", tag.key())
}

/// Whether the activity's actor lives on a foreign host: what the remote
/// subscription rule refuses. An unreadable host is not a foreign one.
fn is_remote_author(ctx: &MrfContext, activity: &Value) -> bool {
    activity_host(activity).is_some_and(|host| !host.eq_ignore_ascii_case(&ctx.local_domain))
}

/// `mrf_tag:media-force-nsfw`: a note that carries attachments is marked
/// `sensitive`; a note without media gets no flag.
fn mark_media_sensitive(activity: &mut Value) {
    let Some(object) = note_object_mut(activity) else {
        return;
    };
    if has_attachments(object) {
        let _previous = object.insert("sensitive".to_owned(), Value::Bool(true));
    }
}

/// `mrf_tag:media-strip`: a note that carries attachments loses them.
fn strip_media(activity: &mut Value) {
    let Some(object) = note_object_mut(activity) else {
        return;
    };
    if has_attachments(object) {
        let _gone = object.remove("attachment");
    }
}

/// `mrf_tag:force-unlisted`, on a `Create`: the public address swaps places
/// with the followers collection — `to` loses the public address and gains
/// the author's followers, `cc` loses the followers and gains the public
/// address — on the activity and its embedded note alike.
fn force_unlisted(activity: &mut Value) {
    if !is_create(activity) {
        return;
    }
    let followers = followers_collection(activity);
    rewrite_each_document(activity, |document| {
        move_public_to_cc(document, &followers);
    });
}

/// One document's `mrf_tag:force-unlisted` swap; a document whose `to`
/// names nothing public has no timeline to be removed from and stands.
fn move_public_to_cc(document: &mut Map<String, Value>, followers: &str) {
    if !addresses_public(document, "to") {
        return;
    }
    remove_public(document, "to");
    add_address(document, "to", followers);
    remove_reference(document, "cc", followers);
    add_address(document, "cc", AS_PUBLIC);
}

/// `mrf_tag:sandbox`, on a `Create`: the note becomes followers-only —
/// the same rewrite as [`simple`](super::simple)'s `followers_only` list.
fn sandbox(activity: &mut Value) {
    if !is_create(activity) {
        return;
    }
    let followers = followers_collection(activity);
    rewrite_each_document(activity, |document| {
        limit_document(document, &followers);
    });
}

/// Whether the activity is a `Create` — the type Pleroma's two addressing
/// tags match, as opposed to the media rules, which also act on `Update`s.
fn is_create(activity: &Value) -> bool {
    activity_type(activity).is_some_and(|kind| kind == CREATE_TYPE) && is_note_activity(activity)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use std::sync::Arc;

    use super::*;
    use crate::federation::mrf::MrfConfig;

    /// The author the note fixtures post as.
    const ALICE: &str = "https://peer.example/users/alice";
    /// That actor's followers collection, the `/followers` convention.
    const FOLLOWERS: &str = "https://peer.example/users/alice/followers";
    /// The public address, full `ActivityStreams` spelling.
    const PUBLIC: &str = AS_PUBLIC;
    /// A second recipient the fixtures keep in `cc`.
    const BOB: &str = "https://remote.example/users/bob";
    /// The note a fixture `Create` announces.
    const NOTE: &str = "https://peer.example/objects/1";
    /// The local account a fixture `Follow` addresses.
    const CAROL: &str = "https://test.example/users/carol";

    /// The context of a run with preloaded author and target tags.
    fn ctx(author_tags: &[&str], target_tags: &[&str]) -> MrfContext {
        MrfContext {
            config: Arc::new(MrfConfig::assemble(
                vec![PARAMS_KEY.to_owned()],
                Map::from_iter([(PARAMS_KEY.to_owned(), json!({}))]),
            )),
            now: Utc::now(),
            local_domain: "test.example".to_owned(),
            author_follower_count: None,
            author_status_count: None,
            author_tags: author_tags.iter().map(|tag| (*tag).to_owned()).collect(),
            target_tags: target_tags.iter().map(|tag| (*tag).to_owned()).collect(),
        }
    }

    /// The policy over an empty parameter block.
    fn policy() -> TagPolicy {
        TagPolicy::new(Some(json!({})))
    }

    /// A public `Create` of a note by alice, `to` the public address and
    /// `cc` the followers and one mention — the shape the addressing tags
    /// rewrite.
    fn create() -> Value {
        json!({
            "id": "https://peer.example/activities/1",
            "type": "Create",
            "actor": ALICE,
            "to": [PUBLIC],
            "cc": [FOLLOWERS, BOB],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [PUBLIC],
                "cc": [FOLLOWERS, BOB],
                "content": "<p>hello</p>",
            },
        })
    }

    /// The fixture `Create` with one image attachment on the note.
    fn create_with_media() -> Value {
        let mut activity = create();
        let _previous = activity["object"]
            .as_object_mut()
            .expect("the fixture embeds its note")
            .insert(
                "attachment".to_owned(),
                json!([{"type": "Document", "mediaType": "image/png", "url": "https://peer.example/media/1.png"}]),
            );
        activity
    }

    /// A `Follow` of the local `CAROL` by `follower`.
    fn follow(follower: &str) -> Value {
        json!({
            "id": "https://peer.example/activities/2",
            "type": "Follow",
            "actor": follower,
            "object": CAROL,
        })
    }

    /// One policy run on the note fixtures under `author_tags`.
    fn filtered_note(author_tags: &[&str], activity: Value) -> MrfResult {
        policy().filter(&ctx(author_tags, &[]), activity)
    }

    /// `mrf_tag:media-force-nsfw`: the golden in and out — a note with
    /// attachments gains only the `sensitive` flag, one without keeps its
    /// document verbatim, and an `Update` is marked as well as a `Create`.
    #[test]
    fn media_force_nsfw_marks_notes_that_carry_media() {
        let mut expected = create_with_media();
        expected["object"]["sensitive"] = json!(true);

        assert_eq!(
            filtered_note(&[Tag::MediaForceNsfw.key()], create_with_media()),
            Ok(expected),
            "the flag appears and nothing else moves"
        );
        assert_eq!(
            filtered_note(&[Tag::MediaForceNsfw.key()], create()),
            Ok(create()),
            "a note without media gets no flag"
        );
        let update = json!({
            "id": "https://peer.example/activities/3",
            "type": "Update",
            "actor": ALICE,
            "object": { "id": NOTE, "type": "Note", "attachment": [] },
        });
        assert_eq!(
            filtered_note(&[Tag::MediaForceNsfw.key()], update.clone()),
            Ok(update),
            "an empty attachment array is no media"
        );
    }

    /// `mrf_tag:media-strip`: the golden in and out — the `attachment` key
    /// goes and everything else stands.
    #[test]
    fn media_strip_drops_attachments() {
        assert_eq!(
            filtered_note(&[Tag::MediaStrip.key()], create_with_media()),
            Ok(create()),
            "the note stands without its attachment"
        );
        assert_eq!(
            filtered_note(&[Tag::MediaStrip.key()], create()),
            Ok(create()),
            "a note that never had attachments is not rewritten"
        );
    }

    /// `mrf_tag:force-unlisted`: the public address moves to `cc` and the
    /// followers collection to `to`, on the activity and the embedded note
    /// alike; a non-public note and an `Update` stand untouched.
    #[test]
    fn force_unlisted_swaps_public_for_followers() {
        let expected = json!({
            "id": "https://peer.example/activities/1",
            "type": "Create",
            "actor": ALICE,
            "to": [FOLLOWERS],
            "cc": [BOB, PUBLIC],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [FOLLOWERS],
                "cc": [BOB, PUBLIC],
                "content": "<p>hello</p>",
            },
        });

        assert_eq!(
            filtered_note(&[Tag::ForceUnlisted.key()], create()),
            Ok(expected),
            "the golden swap"
        );

        let update = json!({
            "id": "https://peer.example/activities/4",
            "type": "Update",
            "actor": ALICE,
            "to": [PUBLIC],
            "object": { "id": NOTE, "type": "Note", "to": [PUBLIC] },
        });
        assert_eq!(
            filtered_note(&[Tag::ForceUnlisted.key()], update.clone()),
            Ok(update),
            "Pleroma's addressing clauses speak about `Create` only"
        );

        let direct = json!({
            "id": "https://peer.example/activities/5",
            "type": "Create",
            "actor": ALICE,
            "to": [BOB],
            "object": { "id": NOTE, "type": "Note", "to": [BOB] },
        });
        assert_eq!(
            filtered_note(&[Tag::ForceUnlisted.key()], direct.clone()),
            Ok(direct),
            "a note nobody public was addressed keeps its recipients"
        );
    }

    /// `mrf_tag:sandbox`: the public address goes out of both fields, the
    /// followers collection joins `to`; a direct note keeps its recipients.
    #[test]
    fn sandbox_limits_public_notes_to_followers() {
        let expected = json!({
            "id": "https://peer.example/activities/1",
            "type": "Create",
            "actor": ALICE,
            "to": [FOLLOWERS],
            "cc": [FOLLOWERS, BOB],
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "to": [FOLLOWERS],
                "cc": [FOLLOWERS, BOB],
                "content": "<p>hello</p>",
            },
        });

        assert_eq!(
            filtered_note(&[Tag::Sandbox.key()], create()),
            Ok(expected),
            "public gone, followers in `to`, on both documents"
        );

        let direct = json!({
            "id": "https://peer.example/activities/5",
            "type": "Create",
            "actor": ALICE,
            "to": [BOB],
            "object": { "id": NOTE, "type": "Note", "to": [BOB] },
        });
        assert_eq!(
            filtered_note(&[Tag::Sandbox.key()], direct.clone()),
            Ok(direct),
            "a direct note is not widened into a followers post"
        );
    }

    /// `mrf_tag:disable-remote-subscription`: a `Follow` of the tagged
    /// local account from a foreign actor is refused with Pleroma's
    /// message; the local account's own follows pass.
    #[test]
    fn disable_remote_subscription_refuses_foreign_followers() {
        let remote = follow(ALICE);
        assert_eq!(
            policy().filter(
                &ctx(&[], &[Tag::DisableRemoteSubscription.key()]),
                remote.clone()
            ),
            Err(format!(
                "[TagPolicy] Follow from {ALICE} tagged with mrf_tag:disable-remote-subscription"
            )),
            "the golden rejection"
        );

        let local = follow("https://test.example/users/dave");
        assert_eq!(
            policy().filter(
                &ctx(&[], &[Tag::DisableRemoteSubscription.key()]),
                local.clone()
            ),
            Ok(local),
            "local follows stay"
        );
        assert_eq!(
            policy().filter(&ctx(&[], &[]), remote.clone()),
            Ok(remote),
            "and an untagged target is followed freely"
        );
    }

    /// `mrf_tag:disable-any-subscription`: no `Follow` of the tagged local
    /// account passes, whoever sends it.
    #[test]
    fn disable_any_subscription_refuses_every_follower() {
        for actor in [ALICE, "https://test.example/users/dave"] {
            let message = format!(
                "[TagPolicy] Follow from {actor} tagged with mrf_tag:disable-any-subscription"
            );
            assert_eq!(
                policy().filter(
                    &ctx(&[], &[Tag::DisableAnySubscription.key()]),
                    follow(actor)
                ),
                Err(message),
                "local and remote followers alike"
            );
        }
    }

    /// The two subscription tags say nothing about other activities, and
    /// the four content tags say nothing about a `Follow`: each family
    /// waits for its own shape.
    #[test]
    fn each_family_of_tags_waits_for_its_own_activity() {
        let remote_follow = follow(ALICE);
        for note_tag in [
            Tag::MediaForceNsfw,
            Tag::MediaStrip,
            Tag::ForceUnlisted,
            Tag::Sandbox,
        ] {
            assert_eq!(
                policy().filter(&ctx(&[], &[note_tag.key()]), remote_follow.clone()),
                Ok(remote_follow.clone()),
                "a content tag on the target does not speak about a Follow"
            );
        }
        let note = create_with_media();
        for subscription_tag in [Tag::DisableRemoteSubscription, Tag::DisableAnySubscription] {
            assert_eq!(
                filtered_note(&[subscription_tag.key()], note.clone()),
                Ok(note.clone()),
                "a subscription tag on the author does not speak about a note"
            );
        }
    }

    /// No-op when nothing was configured: with empty `author_tags` and
    /// empty `target_tags` every shape the policy speaks about passes
    /// verbatim.
    #[test]
    fn no_tags_governs_nothing() {
        let note = create_with_media();
        let follow = follow(ALICE);
        assert_eq!(
            filtered_note(&[], note.clone()),
            Ok(note),
            "the note stands"
        );
        assert_eq!(
            policy().filter(&ctx(&[], &[]), follow.clone()),
            Ok(follow),
            "and the follow lands"
        );
    }

    /// Tags fold in the order the row stores them, and a `users.tags`
    /// array is free to hold ordinary profile tags beside the switches:
    /// unrecognized strings rewrite nothing and break nothing.
    #[test]
    fn recognized_tags_fold_and_other_strings_are_inert() {
        let mut expected = create();
        expected["object"]["sensitive"] = json!(true);
        // mark-then-strip: the mark reads the attachments while they are
        // still there, the strip then takes them away — Pleroma's fold
        // applies the tags in row order, and so does this.
        assert_eq!(
            filtered_note(
                &[
                    "photography",
                    Tag::MediaForceNsfw.key(),
                    Tag::MediaStrip.key(),
                    "mrf_tag:unknown-switch",
                ],
                create_with_media()
            ),
            Ok(expected),
            "the mark saw the media before the strip emptied `attachment`"
        );
    }

    /// Anything that is not a `Create`/`Update`/`Follow` passes untouched.
    #[test]
    fn other_activities_pass_untouched() {
        let announce = json!({
            "id": "https://peer.example/activities/6",
            "type": "Announce",
            "actor": ALICE,
            "object": NOTE,
        });
        assert_eq!(
            policy().filter(&ctx(&["mrf_tag:sandbox"], &[]), announce.clone()),
            Ok(announce),
            "a boost is neither a note nor a follow"
        );
    }

    /// The policy is Pleroma's `TagPolicy`: it names itself, knows its six
    /// tags and their strings, and publishes the subset it was configured
    /// with as its one `mrf_tag` object — the empty object when nothing was.
    #[test]
    fn the_policy_names_itself_and_describes_its_config() {
        assert_eq!(TagPolicy::new(None).name(), "TagPolicy");
        assert_eq!(
            Value::Object(TagPolicy::new(None).describe(&Transparency::open())),
            json!({"mrf_tag": {}}),
            "no key, nothing to describe but its own name"
        );
        assert_eq!(
            Value::Object(
                TagPolicy::new(Some(json!({"echo": true}))).describe(&Transparency::open())
            ),
            json!({"mrf_tag": {"echo": true}}),
            "the configured subset comes back verbatim under the policy's key"
        );
        for tag in Tag::ALL {
            assert_eq!(Tag::from_key(tag.key()), Some(tag), "round-trips: {tag:?}");
        }
        assert_eq!(Tag::from_key("mention"), None, "an ordinary profile tag");
        assert_eq!(Tag::from_key("mrf_tag"), None, "the bare prefix");
        assert_eq!(Tag::from_key(""), None, "and the empty string");
    }
}
