//! The body of the `Note`/`Question` document: [`note_json`] and the field
//! builders it stitches together.
//!
//! The shared field list in Pleroma's render order, the object type, the
//! `source` and `summary` pair, the human-facing `url`, and the `to`/`cc`
//! addressing lines the activities copy verbatim from the note.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::{
    federation::{quotes::interaction_policy_json, vocab::AS_PUBLIC},
    models::{
        status::{Status, Visibility},
        user::{User, followers_address_of},
    },
    util::time::mastodon_time,
};

use super::{
    NoteContext, SELF_REPLIES_LIMIT, TYPE_COLLECTION, TYPE_COLLECTION_PAGE, TYPE_NOTE,
    TYPE_QUESTION, activities::json_ld_context_for, tags::attachments_json, tags::poll_fields,
    tags::tags_json,
};

/// The `Note`/`Question` object of the status in `ctx`, as this instance
/// federates and serves it.
///
/// `None` for a [`Visibility::Local`] status: that visibility exists to
/// keep a post on this instance, and the one thing it must never become is
/// an `ActivityPub` document.
#[must_use]
pub fn note_json(ctx: &NoteContext) -> Option<Value> {
    if ctx.status.visibility == Visibility::Local {
        return None;
    }
    let mut fields = note_fields(ctx);
    if let Some(edited_at) = ctx.status.edited_at {
        fields.push(("updated", json!(mastodon_time(&edited_at))));
    }
    if let Some(poll) = &ctx.poll {
        fields.extend(poll_fields(poll));
    }
    if let Some(quote_uri) = &ctx.quote_uri {
        fields.push(("quoteUrl", json!(quote_uri)));
        fields.push(("quoteUri", json!(quote_uri)));
        fields.push(("_misskey_quote", json!(quote_uri)));
        fields.push(("quote", json!(quote_uri)));
        if let Some(authorization) = &ctx.quote_authorization {
            fields.push(("quoteAuthorization", json!(authorization)));
        }
    }
    if let Some(policy) = interaction_policy_json(
        ctx.status.visibility,
        ctx.status.quote_approval_policy,
        &ctx.author,
        &author_followers(ctx),
    ) {
        fields.push(("interactionPolicy", policy));
    }
    Some(Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    ))
}

/// The fields every visibility shares, in Pleroma's render order; the
/// optional ones ([`note_json`] adds them: `updated`, the poll's `oneOf`
/// and friends, the quote references).
fn note_fields(ctx: &NoteContext) -> Vec<(&'static str, Value)> {
    let context_uri = format!(
        "{}/contexts/{}",
        ctx.config.base_url, ctx.status.conversation_id
    );
    let (to, cc) = addressing(ctx);
    let language = ctx.status.language.as_deref().filter(|tag| !tag.is_empty());
    let mut fields = vec![
        ("@context", json_ld_context_for(&ctx.config, language)),
        ("id", json!(&ctx.status.uri)),
        ("type", json!(object_type(ctx))),
        ("attributedTo", json!(&ctx.author.ap_id)),
        ("actor", json!(&ctx.author.ap_id)),
        ("to", Value::Array(to)),
        ("cc", Value::Array(cc)),
        ("content", json!(&ctx.status.content)),
    ];
    if let Some(tag) = language {
        fields.push(("contentMap", json!({ tag: &ctx.status.content })));
    }
    fields.extend([
        ("source", source_json(&ctx.status)),
        ("summary", summary_json(&ctx.status)),
        ("sensitive", json!(ctx.status.sensitive)),
        ("published", json!(mastodon_time(&ctx.status.inserted_at))),
        ("inReplyTo", json!(&ctx.reply_parent_uri)),
        ("url", json!(status_url(&ctx.status))),
        ("context", json!(&context_uri)),
        ("conversation", json!(&context_uri)),
        ("tag", Value::Array(tags_json(ctx))),
        (
            "attachment",
            Value::Array(attachments_json(&ctx.attachments)),
        ),
    ]);
    if let Some(replies) = replies_json(ctx) {
        fields.push(("replies", replies));
    }
    fields
}

/// The `replies` collection of a publicly readable status.
///
/// Shaped the way Mastodon's `NoteSerializer` writes it.
///
/// The collection's id is `{object}/replies`, the route that serves it;
/// its inline `first` page lists the author's own answers, and `next` leads
/// to the page of everyone else's (`only_other_accounts`). When the
/// author's own answers fill the inline page, `next` is the unfiltered
/// walk instead, so a longer run is not cut off. `None` for a `private`
/// or `direct` status: the replies route answers those with a 404, and the
/// document must not advertise it.
fn replies_json(ctx: &NoteContext) -> Option<Value> {
    if !matches!(
        ctx.status.visibility,
        Visibility::Public | Visibility::Unlisted
    ) {
        return None;
    }
    let id = format!("{}/replies", ctx.status.uri);
    let filled = i64::try_from(ctx.self_replies.len()).is_ok_and(|n| n >= SELF_REPLIES_LIMIT);
    let next = if filled {
        format!("{id}?page=true")
    } else {
        format!("{id}?only_other_accounts=true&page=true")
    };
    Some(json!({
        "id": &id,
        "type": TYPE_COLLECTION,
        "first": {
            "type": TYPE_COLLECTION_PAGE,
            "next": next,
            "partOf": &id,
            "items": &ctx.self_replies,
        },
    }))
}

/// `Question` when the status carries a poll, `Note` otherwise.
const fn object_type(ctx: &NoteContext) -> &'static str {
    if ctx.poll.is_some() {
        TYPE_QUESTION
    } else {
        TYPE_NOTE
    }
}

/// The `source` object: the post's own text as written, in the content
/// type it was written in (Pleroma stores the same pair inside the object).
fn source_json(status: &Status) -> Value {
    json!({
        "content": &status.raw_text,
        "mediaType": &status.content_type,
    })
}

/// The `summary` — the content warning line, JSON `null` when the status
/// carries none.
fn summary_json(status: &Status) -> Value {
    if status.spoiler_text.is_empty() {
        Value::Null
    } else {
        json!(&status.spoiler_text)
    }
}

/// The human-facing URL of the post: the stored `{BASE_URL}/notice/{id}`
/// of a local status, falling back to the object id when a row names no
/// page.
fn status_url(status: &Status) -> &str {
    status.url.as_deref().unwrap_or(status.uri.as_str())
}

/// The `to` and `cc` arrays `ctx`'s visibility addresses, as
/// [`audience_lines`] spells them from the three facts a note's audience
/// rests on.
///
/// The author's followers collection is the one stored on their
/// `activitypub_actors` row, which the loader put in
/// [`NoteContext::author_followers_url`].
fn addressing(ctx: &NoteContext) -> (Vec<Value>, Vec<Value>) {
    audience_lines(ctx.status.visibility, &author_followers(ctx), &ctx.mentions)
}

/// The address of the followers collection `ctx`'s author owns: the one on
/// their `activitypub_actors` row, else the one built from the actor id.
fn author_followers(ctx: &NoteContext) -> String {
    followers_address_of(&ctx.author.ap_id, ctx.author_followers_url.as_deref())
}

/// The `to` and `cc` lines one visibility addresses, given the author's
/// `followers` collection address.
///
/// `public` talks to the world and shows the post in the author's
/// followers' timelines, `unlisted` inverts the pair, `private` reaches only followers and
/// mentionees, `direct` only mentionees — and `local` addresses nothing,
/// which [`note_json`] has already refused to render. Mentioned accounts
/// dedupe by actor id, keeping first-mention order.
///
/// The rule is [`addressing`]'s own, offered apart from the note render for
/// the documents that must travel exactly where the note travelled but are
/// not built from a live row: the `Delete` of a status recomputes these two
/// lines from the row it held before the delete took it down, so the
/// withdrawal reaches the very inboxes the post reached and no other.
pub(crate) fn audience_lines(
    visibility: Visibility,
    followers: &str,
    mentions: &[User],
) -> (Vec<Value>, Vec<Value>) {
    let followers_addr = json!(followers);
    let mentioned = mention_addresses(mentions);
    match visibility {
        Visibility::Public => (vec![json!(AS_PUBLIC)], prepend(followers_addr, mentioned)),
        Visibility::Unlisted => (vec![followers_addr], prepend(json!(AS_PUBLIC), mentioned)),
        Visibility::Private => (prepend(followers_addr, mentioned), Vec::new()),
        Visibility::Direct => (mentioned, Vec::new()),
        Visibility::Local => (Vec::new(), Vec::new()),
    }
}

/// `first` followed by `rest`, as one freshly owned vector.
fn prepend(first: Value, rest: Vec<Value>) -> Vec<Value> {
    let mut addresses = Vec::with_capacity(rest.len() + 1);
    addresses.push(first);
    addresses.extend(rest);
    addresses
}

/// The mentioned actors' ids as addressing entries, deduped by actor id in
/// first-mention order.
fn mention_addresses(mentions: &[User]) -> Vec<Value> {
    let mut seen: HashSet<&str> = HashSet::new();
    mentions
        .iter()
        .filter(|person| seen.insert(person.ap_id.as_str()))
        .map(|person| json!(&person.ap_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::Config,
        federation::note::{create_activity_json, test_support::*, update_activity_json},
        models::status::QuoteApprovalPolicy,
        util::{id::new_id, time::parse_time},
    };

    /// The quoted post's object id on a peer instance.
    const QUOTE_URI: &str = "https://remote.example/objects/quoted-post";
    /// The authorization the quoted post's author granted.
    const AUTHORIZATION_URI: &str = "https://remote.example/quote_authorizations/1";

    #[test]
    fn a_public_note_renders_the_golden_object() {
        let document = render(Visibility::Public);
        assert_eq!(
            document,
            json!({
                "@context": expected_context(),
                "id": format!("{BASE}/objects/{STATUS_ID}"),
                "type": "Note",
                "attributedTo": AUTHOR_AP,
                "actor": AUTHOR_AP,
                "to": [AS_PUBLIC],
                "cc": [format!("{AUTHOR_AP}/followers")],
                "content": "<p>hello</p>",
                "source": {"content": "hello", "mediaType": "text/plain"},
                "summary": null,
                "sensitive": false,
                "published": PUBLISHED,
                "inReplyTo": null,
                "url": format!("{BASE}/notice/{STATUS_ID}"),
                "context": format!("{BASE}/contexts/{STATUS_ID}"),
                "conversation": format!("{BASE}/contexts/{STATUS_ID}"),
                "tag": [],
                "attachment": [],
                "replies": {
                    "id": format!("{BASE}/objects/{STATUS_ID}/replies"),
                    "type": "Collection",
                    "first": {
                        "type": "CollectionPage",
                        "next": format!(
                            "{BASE}/objects/{STATUS_ID}/replies?only_other_accounts=true&page=true"
                        ),
                        "partOf": format!("{BASE}/objects/{STATUS_ID}/replies"),
                        "items": [],
                    },
                },
                "interactionPolicy": {
                    "canQuote": {
                        "automaticApproval": [AS_PUBLIC],
                        "manualApproval": [],
                    }
                },
            }),
            "the whole document, field for field"
        );
        assert_eq!(json_keys(&document), NOTE_KEYS, "and nothing else");
    }

    #[test]
    fn the_replies_first_page_lists_the_authors_own_answers() {
        let mut ctx = note_context(Visibility::Unlisted);
        ctx.self_replies = vec![format!("{BASE}/objects/one"), format!("{BASE}/objects/two")];
        let document = note_json(&ctx).expect("an unlisted note renders");
        let replies = &document["replies"];
        assert_eq!(
            replies["id"],
            format!("{BASE}/objects/{STATUS_ID}/replies"),
            "the id is the route that serves the collection"
        );
        assert_eq!(
            replies["first"]["items"],
            json!([format!("{BASE}/objects/one"), format!("{BASE}/objects/two")]),
            "the author's own answers, in order"
        );
        assert_eq!(
            replies["first"]["next"],
            format!("{BASE}/objects/{STATUS_ID}/replies?only_other_accounts=true&page=true"),
            "the rest of the thread is the next page"
        );
    }

    #[test]
    fn a_full_inline_page_continues_through_the_unfiltered_walk() {
        let mut ctx = note_context(Visibility::Public);
        ctx.self_replies = (0..SELF_REPLIES_LIMIT)
            .map(|n| format!("{BASE}/objects/{n}"))
            .collect();
        let document = note_json(&ctx).expect("a public note renders");
        assert_eq!(
            document["replies"]["first"]["next"],
            format!("{BASE}/objects/{STATUS_ID}/replies?page=true"),
            "the author's own run may go on, so the next page is everything"
        );
    }

    #[test]
    fn a_followers_only_or_direct_note_advertises_no_replies() {
        for visibility in [Visibility::Private, Visibility::Direct] {
            assert!(
                render(visibility).get("replies").is_none(),
                "the replies route answers {visibility:?} posts with a 404"
            );
        }
    }

    #[test]
    fn the_quote_policy_follows_the_visibility() {
        for visibility in [Visibility::Public, Visibility::Unlisted] {
            assert_eq!(
                render(visibility)["interactionPolicy"]["canQuote"]["automaticApproval"],
                json!([AS_PUBLIC]),
                "public and unlisted posts may be quoted by anyone"
            );
        }
        let private = render(Visibility::Private);
        assert_eq!(
            private["interactionPolicy"]["canQuote"],
            json!({
                "automaticApproval": [format!("{AUTHOR_AP}/followers")],
                "manualApproval": [],
            }),
            "a private post may be quoted by its author's followers"
        );
        assert!(
            render(Visibility::Direct)
                .get("interactionPolicy")
                .is_none(),
            "a direct post carries no policy, so peers read it as unquotable"
        );
    }

    /// The `canQuote` of a public note whose author chose `policy`.
    fn can_quote_under(policy: QuoteApprovalPolicy, visibility: Visibility) -> Option<Value> {
        let mut ctx = note_context(visibility);
        ctx.status.quote_approval_policy = Some(policy);
        let document = note_json(&ctx);
        assert!(document.is_some(), "a note renders");
        document?
            .get("interactionPolicy")
            .map(|found| found["canQuote"].clone())
    }

    #[test]
    fn the_authors_quote_policy_narrows_the_advertised_one() {
        assert_eq!(
            can_quote_under(QuoteApprovalPolicy::Nobody, Visibility::Public),
            Some(json!({
                "automaticApproval": [AUTHOR_AP],
                "manualApproval": [],
            })),
            "nobody leaves only the author, as Mastodon spells it"
        );
        assert_eq!(
            can_quote_under(QuoteApprovalPolicy::Followers, Visibility::Public),
            Some(json!({
                "automaticApproval": [format!("{AUTHOR_AP}/followers")],
                "manualApproval": [],
            })),
            "followers narrows a public post to the author's followers"
        );
        assert_eq!(
            can_quote_under(QuoteApprovalPolicy::Public, Visibility::Private),
            Some(json!({
                "automaticApproval": [format!("{AUTHOR_AP}/followers")],
                "manualApproval": [],
            })),
            "a private post is never opened to everyone"
        );
        assert_eq!(
            can_quote_under(QuoteApprovalPolicy::Nobody, Visibility::Direct),
            None,
            "a direct post stays policy-less"
        );
    }

    #[test]
    fn a_status_language_federates_as_context_and_content_map() {
        let mut ctx = note_context(Visibility::Public);
        ctx.status.language = Some("en".to_owned());
        let document = note_json(&ctx).expect("a language-tagged note renders");
        assert_eq!(
            document["contentMap"],
            json!({"en": "<p>hello</p>"}),
            "the content, keyed by its language"
        );
        assert_eq!(document["content"], "<p>hello</p>", "content stays plain");
        assert_eq!(
            document["@context"][2],
            json!({"@language": "en"}),
            "the language map names the status language"
        );
        let mut keys: Vec<&str> = NOTE_KEYS.to_vec();
        keys.push("contentMap");
        keys.sort_unstable();
        assert_eq!(json_keys(&document), keys, "exactly one key more");
    }

    #[test]
    fn a_status_without_language_carries_no_content_map() {
        for language in [None, Some(String::new())] {
            let mut ctx = note_context(Visibility::Public);
            ctx.status.language = language;
            let document = note_json(&ctx).expect("a note renders");
            assert!(document.get("contentMap").is_none(), "no content map");
            assert_eq!(
                document["@context"][2],
                json!({"@language": "und"}),
                "the language stays undetermined"
            );
        }
    }

    #[test]
    fn the_activity_wrappers_carry_the_notes_context() {
        let mut ctx = note_context(Visibility::Public);
        ctx.status.language = Some("de".to_owned());
        let note = note_json(&ctx).expect("a language-tagged note renders");
        let config = Config::for_tests();
        let create = create_activity_json(&note, &ctx.status, &config);
        assert_eq!(create["@context"], note["@context"], "the create's context");
        assert_eq!(
            create["@context"][2],
            json!({"@language": "de"}),
            "carries the status language"
        );
        let update = update_activity_json(&note, new_id(), &config);
        assert_eq!(update["@context"], note["@context"], "the update's context");

        let bare = json!({"actor": AUTHOR_AP});
        assert_eq!(
            create_activity_json(&bare, &ctx.status, &config)["@context"],
            expected_context(),
            "a note without a context falls back to the undetermined one"
        );
    }

    #[test]
    fn an_unlisted_note_moves_the_public_collection_to_cc() {
        let document = render(Visibility::Unlisted);
        assert_eq!(
            document["to"],
            json!([format!("{AUTHOR_AP}/followers")]),
            "an unlisted post is addressed to the followers"
        );
        assert_eq!(
            document["cc"],
            json!([AS_PUBLIC]),
            "and the world only sees it in cc"
        );
    }

    #[test]
    fn a_note_uses_the_actor_rows_followers_url_over_the_built_one() {
        let stored = "https://test.example/collections/alice/fans";
        let mut public = note_context(Visibility::Public);
        public.author_followers_url = Some(stored.to_owned());
        let (to, cc) = addressing(&public);
        assert_eq!(to, vec![json!(AS_PUBLIC)], "the world is addressed");
        assert_eq!(cc, vec![json!(stored)], "the stored followers collection");
        let mut private = note_context(Visibility::Private);
        private.author_followers_url = Some(stored.to_owned());
        let (private_to, private_cc) = addressing(&private);
        assert_eq!(
            private_to,
            vec![json!(stored)],
            "private reaches the stored one"
        );
        assert!(private_cc.is_empty(), "and nobody is cc-ed");
    }

    #[test]
    fn a_note_builds_the_followers_url_when_the_actor_row_has_none() {
        let ctx = note_context(Visibility::Unlisted);
        assert_eq!(ctx.author_followers_url, None, "the fixture stores none");
        let (to, _cc) = addressing(&ctx);
        assert_eq!(
            to,
            vec![json!(format!("{AUTHOR_AP}/followers"))],
            "the `{{ap_id}}/followers` fallback"
        );
    }

    #[test]
    fn a_private_note_reaches_followers_and_mentionees_only() {
        let mut ctx = note_context(Visibility::Private);
        ctx.mentions = vec![mentioned_bob()];
        let document = note_json(&ctx).expect("private federates");
        assert_eq!(
            document["to"],
            json!([format!("{AUTHOR_AP}/followers"), BOB_AP]),
            "followers and the mentionee share the primary line"
        );
        assert_eq!(document["cc"], json!([]), "and nothing is cc-ed");
    }

    #[test]
    fn a_direct_note_addresses_its_mentionees_alone() {
        let mut ctx = note_context(Visibility::Direct);
        ctx.mentions = vec![mentioned_bob()];
        let document = note_json(&ctx).expect("direct federates");
        assert_eq!(document["to"], json!([BOB_AP]), "to is the mentionees");
        assert_eq!(document["cc"], json!([]), "cc stays empty");
        assert!(
            !document["to"]
                .as_array()
                .unwrap()
                .contains(&json!(AS_PUBLIC)),
            "a direct note never addresses the public collection"
        );
    }

    #[test]
    fn a_local_status_is_never_federated() {
        let ctx = note_context(Visibility::Local);
        assert!(
            note_json(&ctx).is_none(),
            "local visibility renders no object"
        );
    }

    #[test]
    fn a_reply_carries_its_parent_and_reaches_its_author() {
        let mut ctx = note_context(Visibility::Public);
        ctx.status.in_reply_to_id = Some(new_id());
        ctx.status.in_reply_to_account_id = Some(Uuid::nil());
        ctx.reply_parent_uri = Some(PARENT_URI.to_owned());
        ctx.mentions = vec![mentioned_bob()];
        let document = note_json(&ctx).expect("a reply federates");
        assert_eq!(document["inReplyTo"], PARENT_URI, "the parent address");
        assert_eq!(
            document["cc"],
            json!([format!("{AUTHOR_AP}/followers"), BOB_AP]),
            "the parent author rides along in cc, through the mentions"
        );
    }

    #[test]
    fn a_quote_carries_every_quote_reference_spelling() {
        let mut ctx = note_context(Visibility::Public);
        ctx.quote_uri = Some(QUOTE_URI.to_owned());
        let document = note_json(&ctx).expect("a quote federates");
        assert_eq!(document["quoteUrl"], QUOTE_URI, "Mastodon's legacy key");
        assert_eq!(document["quoteUri"], QUOTE_URI, "Fedibird's");
        assert_eq!(document["_misskey_quote"], QUOTE_URI, "Misskey's");
        assert_eq!(document["quote"], QUOTE_URI, "FEP-044f's");
        let mut keys: Vec<&str> = NOTE_KEYS.to_vec();
        keys.extend(["_misskey_quote", "quote", "quoteUri", "quoteUrl"]);
        keys.sort_unstable();
        assert_eq!(json_keys(&document), keys, "exactly four keys more");
    }

    #[test]
    fn a_granted_quote_names_its_authorization() {
        let mut ctx = note_context(Visibility::Public);
        ctx.quote_uri = Some(QUOTE_URI.to_owned());
        assert!(
            note_json(&ctx)
                .expect("a quote federates")
                .get("quoteAuthorization")
                .is_none(),
            "an unanswered quote names no authorization"
        );

        ctx.quote_authorization = Some(AUTHORIZATION_URI.to_owned());
        let document = note_json(&ctx).expect("a granted quote federates");

        assert_eq!(
            document["quoteAuthorization"], AUTHORIZATION_URI,
            "the grant rides on the note"
        );
        assert!(
            undefined_terms(&document).is_empty(),
            "the served schema defines the new terms"
        );
    }

    #[test]
    fn an_authorization_without_a_quote_is_not_rendered() {
        let mut ctx = note_context(Visibility::Public);
        ctx.quote_authorization = Some(AUTHORIZATION_URI.to_owned());
        let document = note_json(&ctx).expect("the note renders");
        assert!(
            document.get("quoteAuthorization").is_none(),
            "only a quote carries a grant"
        );
    }

    #[test]
    fn an_edited_status_reports_when_it_changed() {
        let mut ctx = note_context(Visibility::Public);
        ctx.status.edited_at = Some(parse_time(DELETED).unwrap());
        let document = note_json(&ctx).expect("an edit federates");
        assert_eq!(document["updated"], DELETED, "the update line");
        assert!(
            render(Visibility::Public).get("updated").is_none(),
            "and an unedited status carries none"
        );
    }
}
