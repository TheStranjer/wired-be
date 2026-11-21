//! The body of the `Note`/`Question` document: [`note_json`] and the field
//! builders it stitches together.
//!
//! The shared field list in Pleroma's render order, the object type, the
//! `source` and `summary` pair, the human-facing `url`, and the `to`/`cc`
//! addressing lines the activities copy verbatim from the note.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::{
    models::{
        status::{Status, Visibility},
        user::{User, followers},
    },
    util::time::mastodon_time,
};

use super::{
    AS_PUBLIC, NoteContext, TYPE_NOTE, TYPE_QUESTION, activities::json_ld_context,
    tags::attachments_json, tags::poll_fields, tags::tags_json,
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
    vec![
        ("@context", json_ld_context(&ctx.config)),
        ("id", json!(&ctx.status.uri)),
        ("type", json!(object_type(ctx))),
        ("attributedTo", json!(&ctx.author.ap_id)),
        ("actor", json!(&ctx.author.ap_id)),
        ("to", Value::Array(to)),
        ("cc", Value::Array(cc)),
        ("content", json!(&ctx.status.content)),
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
    ]
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
/// [`address_lines`] spells them from the three facts a note's audience
/// rests on.
fn addressing(ctx: &NoteContext) -> (Vec<Value>, Vec<Value>) {
    address_lines(ctx.status.visibility, &ctx.author, &ctx.mentions)
}

/// The `to` and `cc` lines one visibility addresses: `public` talks to the
/// world and shows the post in the author's followers' timelines,
/// `unlisted` inverts the pair, `private` reaches only followers and
/// mentionees, `direct` only mentionees — and `local` addresses nothing,
/// which [`note_json`] has already refused to render. Mentioned accounts
/// dedupe by actor id, keeping first-mention order.
///
/// The rule is [`addressing`]'s own, offered apart from the note render for
/// the documents that must travel exactly where the note travelled but are
/// not built from a live row: the `Delete` of a status recomputes these two
/// lines from the row it held before the delete took it down, so the
/// withdrawal reaches the very inboxes the post reached and no other.
pub(crate) fn address_lines(
    visibility: Visibility,
    author: &User,
    mentions: &[User],
) -> (Vec<Value>, Vec<Value>) {
    let followers_addr = json!(followers(&author.ap_id));
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
        federation::note::test_support::*,
        util::{id::new_id, time::parse_time},
    };

    /// The quoted post's object id on a peer instance.
    const QUOTE_URI: &str = "https://remote.example/objects/quoted-post";

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
            }),
            "the whole document, field for field"
        );
        assert_eq!(json_keys(&document), NOTE_KEYS, "and nothing else");
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
    fn a_quote_carries_both_quote_reference_spellings() {
        let mut ctx = note_context(Visibility::Public);
        ctx.quote_uri = Some(QUOTE_URI.to_owned());
        let document = note_json(&ctx).expect("a quote federates");
        assert_eq!(document["quoteUrl"], QUOTE_URI, "Mastodon's key");
        assert_eq!(document["quoteUri"], QUOTE_URI, "and Fedibird's");
        let mut keys: Vec<&str> = NOTE_KEYS.to_vec();
        keys.extend(["quoteUri", "quoteUrl"]);
        keys.sort_unstable();
        assert_eq!(json_keys(&document), keys, "exactly two keys more");
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
