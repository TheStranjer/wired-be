//! Pleroma's `AntiLinkSpamPolicy`: the first-post link gate of the
//! [pipeline](super).
//!
//! The reference is `lib/pleroma/web/activity_pub/mrf/anti_link_spam_policy.ex`:
//! an account that has never posted and follows nobody is treated as what
//! it most likely is — a bot — and its linked content is refused. Only
//! `Create` activities of a note-family object from a REMOTE author are
//! weighed; a local author posting to this instance is its own business,
//! and any other activity passes untouched. A note "contains a link" when
//! its `content` HTML holds an `<a href>` that is not a reference to a
//! local participant of the post itself: anchors carrying the `mention` or
//! `hashtag` class are markup this instance renders, not what a spammer
//! sells (Pleroma's `a.mention,a.hashtag` filter arms; its `a[rel~=tag]`
//! and `a.zrl` arms are left out, as the ticket's rule names the two
//! classes alone).
//!
//! The two facts the decision needs — the author's `users.followers_count`
//! and `users.statuses_count` — arrive preloaded in the
//! `author_follower_count`/`author_status_count` fields of the
//! [`MrfContext`], fetched by the chain runner only while this
//! policy (registered as `anti_link_spam`) is enabled. An author this
//! instance has never seen loads as no counts at all, which weighs as the
//! brand-new account it is: Pleroma's `get_or_fetch` failure arm rejects
//! too, and refusing a linked post from an unknown remote actor is the
//! conservative answer.
//!
//! ```json
//! {"policies": ["anti_link_spam"]}
//! ```
//!
//! The policy needs no parameters; whatever an operator writes under the
//! `anti_link_spam` key is only echoed back through
//! [`describe`](MrfPolicy::describe).

use scraper::{ElementRef, Html, Selector};
use serde_json::{Value, json};

use crate::federation::mrf::{
    MrfContext, MrfPolicy, MrfResult,
    simple::{activity_host, activity_type, is_note_activity},
};

/// The key of this policy's parameters inside the `mrf` instance setting,
/// and the name [`build_chain`](super::build_chain) resolves.
pub const PARAMS_KEY: &str = "anti_link_spam";

/// The `ActivityStreams` activity type this policy weighs.
const CREATE_TYPE: &str = "Create";
/// The `class` token of a mention anchor: markup, not a link.
const MENTION_CLASS: &str = "mention";
/// The `class` token of a hashtag anchor: markup, not a link.
const HASHTAG_CLASS: &str = "hashtag";
/// The selector every anchor carrying an `href` matches.
const ANCHOR_SELECTOR: &str = "a[href]";

/// The anti-link-spam stage: Pleroma's `MRF.AntiLinkSpamPolicy`.
#[derive(Clone, Debug)]
pub struct AntiLinkSpamPolicy {
    /// The parameter block this policy was configured with, for
    /// [`describe`](MrfPolicy::describe); the filter itself reads nothing.
    params: Option<Value>,
}

impl AntiLinkSpamPolicy {
    /// The policy beside the `anti_link_spam` key of the `mrf` setting
    /// (`None` when the setting carries none).
    #[must_use]
    pub const fn new(params: Option<Value>) -> Self {
        Self { params }
    }
}

impl MrfPolicy for AntiLinkSpamPolicy {
    fn name(&self) -> &'static str {
        "AntiLinkSpamPolicy"
    }

    fn filter(&self, ctx: &MrfContext, activity: Value) -> MrfResult {
        if !is_remote_note_create(ctx, &activity) || !note_has_link(&activity) {
            return Ok(activity);
        }
        // Pleroma's `old_user?`: any follower or any earlier post is
        // standing enough. A preload miss (no row for the actor) weighs as
        // zero of both — the unknown-actor arm of the reference rejects
        // too.
        let followers = ctx.author_follower_count.unwrap_or(0);
        let statuses = ctx.author_status_count.unwrap_or(0);
        if followers > 0 || statuses > 0 {
            return Ok(activity);
        }
        Err("[AntiLinkSpamPolicy] the author has no posts nor followers".to_owned())
    }

    fn describe(&self) -> Value {
        self.params.clone().unwrap_or_else(|| json!({}))
    }
}

/// Whether this is the activity the policy weighs: a `Create` of a
/// note-family object by an actor from a foreign domain. A host this policy
/// cannot read is not a foreign one; it passes like Pleroma's catch-all
/// clause passes an actorless activity.
fn is_remote_note_create(ctx: &MrfContext, activity: &Value) -> bool {
    let created = activity_type(activity).is_some_and(|kind| kind == CREATE_TYPE);
    let remote =
        activity_host(activity).is_some_and(|host| !host.eq_ignore_ascii_case(&ctx.local_domain));
    created && remote && is_note_activity(activity)
}

/// Whether the note embedded in the activity carries a real link: an
/// `<a href>` anchor whose classes name neither mention nor hashtag.
fn note_has_link(activity: &Value) -> bool {
    note_content(activity).is_some_and(contains_link)
}

/// The `content` string of the embedded note, `None` when the object spells
/// its content any other way (a multi-type array, an absent key).
fn note_content(activity: &Value) -> Option<&str> {
    activity
        .get("object")
        .and_then(Value::as_object)
        .and_then(|object| object.get("content"))
        .and_then(Value::as_str)
}

/// Whether parsed `content` HTML holds at least one anchor that is a link
/// in the policy's sense. Parsing goes through `scraper`, whose
/// `html5ever`-tolerant fragment reader sees the anchors the way a browser
/// does.
fn contains_link(content: &str) -> bool {
    let Ok(anchor) = Selector::parse(ANCHOR_SELECTOR) else {
        return false;
    };
    Html::parse_fragment(content)
        .select(&anchor)
        .any(|link| !is_reference_markup(&link))
}

/// Whether an anchor is mention/hashtag markup rather than a link: one of
/// its `class` tokens, exactly, names the reference.
fn is_reference_markup(link: &ElementRef<'_>) -> bool {
    link.value().attr("class").is_some_and(|classes| {
        classes
            .split_whitespace()
            .any(|token| token == MENTION_CLASS || token == HASHTAG_CLASS)
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Map, json};
    use std::sync::Arc;

    use super::*;
    use crate::federation::mrf::MrfConfig;

    /// The author the fixtures post as.
    const ALICE: &str = "https://peer.example/users/alice";
    /// The note the fixture `Create` announces.
    const NOTE: &str = "https://peer.example/objects/1";

    /// The context of a run with the given preloaded author facts.
    fn ctx(followers: Option<i32>, statuses: Option<i32>) -> MrfContext {
        MrfContext {
            config: Arc::new(MrfConfig::assemble(
                vec![PARAMS_KEY.to_owned()],
                Map::from_iter([(PARAMS_KEY.to_owned(), json!({}))]),
            )),
            now: Utc::now(),
            local_domain: "test.example".to_owned(),
            author_follower_count: followers,
            author_status_count: statuses,
            author_tags: Vec::new(),
            target_tags: Vec::new(),
        }
    }

    /// The policy over an empty parameter block.
    fn policy() -> AntiLinkSpamPolicy {
        AntiLinkSpamPolicy::new(Some(json!({})))
    }

    /// A `Create` of a note whose `content` is `content`, by alice.
    fn create_with(content: &str) -> Value {
        json!({
            "id": "https://peer.example/activities/1",
            "type": "Create",
            "actor": ALICE,
            "object": {
                "id": NOTE,
                "type": "Note",
                "attributedTo": ALICE,
                "content": content,
            },
        })
    }

    /// [`contains_link`]: a plain anchor is a link; a bare `<a>`, a
    /// mention, a hashtag and text without markup are not.
    #[test]
    fn only_anchors_with_an_href_count_as_links() {
        assert!(
            contains_link(r#"<p>visit <a href="https://spam.example/x">cheap</a></p>"#),
            "a plain linked anchor is the link the policy weighs"
        );
        assert!(
            !contains_link("<p>no markup at all, just &lt;a href=x&gt; text</p>"),
            "escaped angle brackets open no anchor"
        );
        assert!(
            !contains_link("<a>nameless</a><span>x</span>"),
            "an anchor without an href is not a link"
        );
        assert!(
            !contains_link(r#"<img src="https://spam.example/x">"#),
            "an image is not an anchor"
        );
    }

    /// [`contains_link`]: `mention` and `hashtag` anchors are reference
    /// markup, alone or beside other classes; an almost-token is not.
    #[test]
    fn mention_and_hashtag_anchors_are_not_links() {
        assert!(
            !contains_link(r#"<a class="mention" href="https://test.example/users/bob">@bob</a>"#),
            "a mention link is how a post addresses somebody"
        );
        assert!(
            !contains_link(r#"<a class="hashtag" href="https://test.example/tags/rust">#rust</a>"#),
            "and a hashtag link tags the post"
        );
        assert!(
            !contains_link(
                r#"<a class="u-url mention" href="https://test.example/users/bob">@bob</a>"#
            ),
            "the token may sit beside other classes"
        );
        assert!(
            contains_link(r#"<a class="mentions" href="https://spam.example/x">trick</a>"#),
            "but a look-alike class is not the token"
        );
        assert!(
            contains_link(
                r#"<a class="mention" href="https://test.example/users/bob">@bob</a> and <a href="https://spam.example/x">buy</a>"#
            ),
            "one real link beside a mention is still a link"
        );
    }

    /// The golden rejection: a remote `Create` note with a link, an author
    /// preloaded at zero followers and zero statuses.
    #[test]
    fn a_linked_note_from_a_brand_new_remote_author_is_rejected() {
        let activity = create_with(r#"<p><a href="https://spam.example/x">cheap watches</a></p>"#);

        let run = policy().filter(&ctx(Some(0), Some(0)), activity.clone());

        assert_eq!(
            run,
            Err("[AntiLinkSpamPolicy] the author has no posts nor followers".to_owned()),
            "the golden in and out of the rejection: {activity}"
        );
    }

    /// An author whose preload found no row at all weighs as the newcomer
    /// it is — the miss is not a pass.
    #[test]
    fn an_unstored_author_weighs_as_new() {
        let linked = create_with(r#"<a href="https://spam.example/x">x</a>"#);
        assert_eq!(
            policy().filter(&ctx(None, None), linked),
            Err("[AntiLinkSpamPolicy] the author has no posts nor followers".to_owned()),
            "no row, no standing"
        );
        assert_eq!(
            policy().filter(&ctx(None, None), create_with("<p>hello</p>")),
            Ok(create_with("<p>hello</p>")),
            "and the miss never touches a note without links"
        );
    }

    /// Standing is one follower or one earlier post; either one lets the
    /// linked note through unchanged, the golden no-rewrite pass.
    #[test]
    fn an_author_with_followers_or_posts_passes() {
        for standing in [ctx(Some(1), Some(0)), ctx(Some(0), Some(1))] {
            let activity = create_with(r#"<a href="https://example.org">fine</a>"#);
            assert_eq!(
                policy().filter(&standing, activity.clone()),
                Ok(activity),
                "one follower or one status is Pleroma's `old_user?`"
            );
        }
    }

    /// The local exemption: an author on this instance's own domain posts
    /// links freely, whatever the preload says.
    #[test]
    fn a_local_author_is_never_the_target() {
        let local = json!({
            "id": "https://test.example/activities/1",
            "type": "Create",
            "actor": "https://test.example/users/bob",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "object": {
                "id": "https://test.example/objects/1",
                "type": "Note",
                "attributedTo": "https://test.example/users/bob",
                "content": r#"<a href="https://spam.example/x">hello</a>"#,
            },
        });
        assert_eq!(
            policy().filter(&ctx(Some(0), Some(0)), local.clone()),
            Ok(local),
            "the rule guards this instance from remote newcomers, not itself"
        );
    }

    /// Everything outside the weighed shape passes untouched: a mention
    /// only note, an `Update`, a non-note object, a `Follow`, an
    /// actorless activity, and a content the object spells not as a
    /// string.
    #[test]
    fn the_policy_weighs_only_linked_note_creates() {
        let zeroed = ctx(Some(0), Some(0));
        for activity in [
            create_with(r#"<a class="mention" href="https://test.example/users/bob">@bob</a>"#),
            json!({
                "id": "https://peer.example/activities/2",
                "type": "Update",
                "actor": ALICE,
                "object": { "id": NOTE, "type": "Note", "content": "<a href=\"https://spam.example/x\">x</a>" },
            }),
            json!({
                "id": "https://peer.example/activities/3",
                "type": "Create",
                "actor": ALICE,
                "object": { "id": NOTE, "type": "ChatMessage", "content": "<a href=\"https://spam.example/x\">x</a>" },
            }),
            json!({
                "id": "https://peer.example/activities/4",
                "type": "Create",
                "actor": ALICE,
                "object": NOTE,
            }),
            json!({
                "id": "https://peer.example/activities/5",
                "type": "Follow",
                "actor": ALICE,
                "object": "https://test.example/users/bob",
            }),
            json!({
                "id": "https://peer.example/activities/6",
                "type": "Create",
                "object": { "id": NOTE, "type": "Note", "content": "<a href=\"https://spam.example/x\">x</a>" },
            }),
            create_with(""),
            json!({
                "id": "https://peer.example/activities/7",
                "type": "Create",
                "actor": ALICE,
                "object": { "id": NOTE, "type": "Note", "content": [{ "type": "Content", "source": "<a href=\"https://spam.example/x\">x</a>" }] },
            }),
        ] {
            assert_eq!(
                policy().filter(&zeroed, activity.clone()),
                Ok(activity),
                "a shape the rule does not speak about passes unchanged"
            );
        }
    }

    /// The policy is Pleroma's `AntiLinkSpamPolicy`: it names itself and
    /// describes back the subset it was configured with — the empty object
    /// when nothing was.
    #[test]
    fn the_policy_names_itself_and_describes_its_config() {
        assert_eq!(AntiLinkSpamPolicy::new(None).name(), "AntiLinkSpamPolicy");
        assert_eq!(
            AntiLinkSpamPolicy::new(None).describe(),
            json!({}),
            "no key, nothing to describe"
        );
        assert_eq!(
            AntiLinkSpamPolicy::new(Some(json!({"echo": true}))).describe(),
            json!({"echo": true}),
            "the configured subset comes back verbatim"
        );
    }

    /// The link check survives hostile markup the sanitizer would later
    /// rewrite: detection runs on the document as it arrived.
    #[test]
    fn detection_reads_the_raw_document() {
        assert!(
            contains_link(r#"<a href="javascript:alert(1)">x</a>"#),
            "a javascript: href still counts as the linked post it is"
        );
        assert!(
            contains_link("<a href=https://spam.example/x>bare</a>"),
            "and an unquoted attribute still parses as an anchor"
        );
    }
}
