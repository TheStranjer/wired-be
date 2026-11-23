//! The `tag` array split into its three lists: mention hrefs, hashtag
//! names and custom emojis, with the per-entry readers behind them.

use serde_json::Value;

use crate::{
    federation::json::{as_array, first_string, value_or_id},
    models::user::User,
    util::emoji,
};

/// The longest emoji shortcode kept, in bytes (the name is ASCII once it
/// validates); Mastodon allows 2048 federated, a picker has no use for that.
const MAX_SHORTCODE_BYTES: usize = 128;

use super::{
    MAX_HASHTAG_CHARS, MAX_TAGS,
    envelope::is_http_url,
    types::{RemoteEmoji, TagSets},
};

/// The first `tag` entry of each kind, [`MAX_TAGS`] entries examined at
/// most; unknown tag types and non-object entries are ignored (Pleroma's
/// `fix_tag` keeps only objects).
pub(super) fn parse_tags(json: &Value, author: &User) -> TagSets {
    let mut sets = TagSets::default();
    for entry in json
        .get("tag")
        .map_or_default(as_array)
        .iter()
        .take(MAX_TAGS)
    {
        match entry.get("type").and_then(Value::as_str) {
            Some("Mention") => sets
                .mention_hrefs
                .extend(entry.get("href").and_then(value_or_id)),
            Some("Hashtag") => sets.hashtags.extend(parse_hashtag(entry)),
            Some("Emoji") => sets.emojis.extend(parse_emoji(entry, author)),
            Some(_) | None => {}
        }
    }
    sets
}

/// One `Hashtag` tag: its `name` without the leading `#`, lowercased;
/// empty or over-long ([`MAX_HASHTAG_CHARS`] characters) names are skipped.
fn parse_hashtag(entry: &Value) -> Option<String> {
    let name = entry.get("name").and_then(Value::as_str)?;
    let tag = name.strip_prefix('#').unwrap_or(name);
    if tag.is_empty() || tag.chars().count() > MAX_HASHTAG_CHARS {
        return None;
    }
    Some(tag.to_lowercase())
}

/// One `Emoji` tag: the colon-stripped `name` and the first `http(s)` url
/// under `icon`, in the author's domain (Pleroma's `fix_emoji` reads the
/// same fields). A tag with neither is skipped, and so is one whose name is
/// no shortcode (`[A-Za-z0-9_]+`, like Mastodon's model validation) or is
/// over [`MAX_SHORTCODE_BYTES`] long.
fn parse_emoji(entry: &Value, author: &User) -> Option<RemoteEmoji> {
    let trimmed = entry.get("name").and_then(Value::as_str)?.trim_matches(':');
    if trimmed.len() > MAX_SHORTCODE_BYTES || emoji::shortcode(&format!(":{trimmed}:")).is_none() {
        return None;
    }
    let shortcode = trimmed.to_owned();
    let url = entry
        .get("icon")
        .map_or_default(as_array)
        .iter()
        .filter_map(first_string)
        .find(|candidate| is_http_url(candidate))?;
    Some(RemoteEmoji {
        shortcode,
        url,
        domain: author.domain.clone(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::note_parser::test_support::*;

    #[test]
    fn the_mastodon_note_splits_its_tags() {
        let parsed = parse(&fixture(MASTODON));
        assert_eq!(
            parsed.hashtags,
            ["rust"],
            "lowercased, # dropped; the \
                   over-long name, the Show type and the bare string skipped"
        );
        assert_eq!(
            parsed.mention_hrefs,
            ["https://remote.example/users/carol"],
            "the Mention href"
        );
        assert_eq!(
            parsed.emojis,
            [RemoteEmoji {
                shortcode: "ralph".to_owned(),
                url: "https://remote.example/emoji/ralph.png".to_owned(),
                domain: Some("remote.example".to_owned()),
            }],
            "the Emoji tag, colons stripped, in the author's domain"
        );
    }

    #[test]
    fn the_tag_list_caps_at_one_hundred() {
        let mut document = minimal_note();
        document["tag"] = json!(
            (0..120)
                .map(|n| json!({"type": "Mention", "href": format!("{AUTHOR_AP_ID}?n={n}")}))
                .collect::<Vec<Value>>()
        );
        let parsed = parse(&document);
        assert_eq!(
            parsed.mention_hrefs.len(),
            100,
            "only the first hundred tags"
        );
    }

    #[test]
    fn emoji_names_that_are_no_shortcode_are_skipped() {
        let icon = json!({"type": "Image", "url": "https://remote.example/emoji/x.png"});
        let long = format!(":{}:", "a".repeat(200));
        let edge = format!(":{}:", "b".repeat(MAX_SHORTCODE_BYTES));
        let mut document = minimal_note();
        document["tag"] = json!([
            {"type": "Emoji", "name": ":ok_1:", "icon": icon},
            {"type": "Emoji", "name": ":a b:", "icon": icon},
            {"type": "Emoji", "name": ":<x>:", "icon": icon},
            {"type": "Emoji", "name": long, "icon": icon},
            {"type": "Emoji", "name": edge, "icon": icon},
            {"type": "Emoji", "name": "::", "icon": icon},
            {"type": "Emoji", "name": ":a::b:", "icon": icon},
        ]);
        let parsed = parse(&document);
        let names: Vec<&str> = parsed
            .emojis
            .iter()
            .map(|emoji| emoji.shortcode.as_str())
            .collect();
        assert_eq!(
            names,
            ["ok_1", "b".repeat(MAX_SHORTCODE_BYTES).as_str()],
            "only [A-Za-z0-9_] names up to the cap are kept"
        );
    }

    #[test]
    fn hashtag_names_lose_their_hash_and_their_case() {
        let mut document = minimal_note();
        document["tag"] = json!([
            {"type": "Hashtag", "name": "#RuSt"},
            {"type": "Hashtag", "name": "plain"},
            {"type": "Hashtag", "name": "#"},
            {"type": "Hashtag", "name": "#tada"},
        ]);
        let parsed = parse(&document);
        assert_eq!(
            parsed.hashtags,
            ["rust", "plain", "tada"],
            "hash dropped, lowercased; a bare hash is no tag"
        );
    }
}
