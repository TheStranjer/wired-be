//! The `tag` array split into its three lists: mention hrefs, hashtag
//! names and custom emojis, with the per-entry readers behind them.

use serde_json::Value;

use crate::{
    federation::{
        emoji_tag::parse_emoji_tag,
        json::{as_array, value_or_id},
    },
    models::{hashtag::normalize_name, user::User},
};

use super::{MAX_TAGS, types::TagSets};

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
            Some("Emoji") => sets
                .emojis
                .extend(parse_emoji_tag(entry, author.domain.as_deref())),
            Some(_) | None => {}
        }
    }
    sets
}

/// One `Hashtag` tag: its `name` as [`normalize_name`] spells it (no leading
/// `#`, lowercased); names it refuses — empty, over-long, outside the tag
/// character set or all digits — are skipped.
fn parse_hashtag(entry: &Value) -> Option<String> {
    normalize_name(entry.get("name").and_then(Value::as_str)?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::{
        emoji_tag::MAX_SHORTCODE_BYTES,
        note_parser::{RemoteEmoji, test_support::*},
    };

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
            {"type": "Emoji", "name": ":a.b:", "icon": icon},
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
            "only [A-Za-z0-9_+-] names up to the cap are kept"
        );
    }

    #[test]
    fn dashed_and_plus_emoji_names_parse_to_their_shortcode() {
        let icon = json!({"type": "Image", "url": "https://remote.example/emoji/x.png"});
        let mut document = minimal_note();
        document["tag"] = json!([
            {"type": "Emoji", "name": ":blob-cat:", "icon": icon},
            {"type": "Emoji", "name": "blobfox+1", "icon": icon},
        ]);
        let parsed = parse(&document);
        let names: Vec<&str> = parsed
            .emojis
            .iter()
            .map(|emoji| emoji.shortcode.as_str())
            .collect();
        assert_eq!(
            names,
            ["blob-cat", "blobfox+1"],
            "colons stripped, rest kept"
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

    #[test]
    fn hashtag_names_outside_the_tag_character_set_are_skipped() {
        let mut document = minimal_note();
        document["tag"] = json!([
            {"type": "Hashtag", "name": "#bad tag"},
            {"type": "Hashtag", "name": "#a/b?x=<y>"},
            {"type": "Hashtag", "name": "#123"},
            {"type": "Hashtag", "name": "#good_tag"},
        ]);
        let parsed = parse(&document);
        assert_eq!(parsed.hashtags, ["good_tag"], "only the clean name stays");
    }
}
