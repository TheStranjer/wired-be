//! Emoji text: what counts as one emoji for the reaction endpoints.
//!
//! The reaction API names an emoji in a single path segment, so the text a
//! client sends is either one `unicode` emoji or a reference to one of this
//! instance's custom emojis. Pleroma's equivalent is `Pleroma.Emoji`
//! (`lib/pleroma/emoji.ex`): `unicode?/1` against its generated emoji list
//! plus `maybe_quote/1`, which wraps anything that is not unicode in
//! colons. Here the classification is [`Emoji::classify`], and
//! [`is_single_emoji`] is the grapheme-level question it asks.
//!
//! What this module cannot answer is whether a `:shortcode:` names an emoji
//! this instance actually has: that is a `custom_emojis` row, so it stays
//! with the caller that holds a database connection (see
//! [`crate::models::custom_emoji`]). Like the rest of [`util`](super) this
//! knows neither HTTP nor SQL; the 400 an unusable emoji gets is the
//! endpoint's to send.

use unicode_segmentation::UnicodeSegmentation as _;

mod table;

/// A reaction emoji as the API names it, classified from its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Emoji<'a> {
    /// One unicode emoji grapheme cluster, e.g. `🙂` or `👨‍👩‍👧`;
    /// the variant holds the client's own text.
    Unicode(&'a str),
    /// A `:shortcode:` reference to a custom emoji; the variant holds the
    /// bare `shortcode`, i.e. the `[A-Za-z0-9_]+` between the colons.
    Custom(&'a str),
}

impl<'a> Emoji<'a> {
    /// Classifies `text` as a reaction emoji, `None` for anything else.
    ///
    /// The two accepted spellings are [`Emoji::Custom`] for `:shortcode:`
    /// text whose shortcode is non-empty and only of `[A-Za-z0-9_]` —
    /// Pleroma's `maybe_quote` shape — and [`Emoji::Unicode`] for exactly one
    /// emoji grapheme cluster ([`is_single_emoji`]). Everything else — the
    /// empty string, a word, two emojis, one emoji glued to a letter, `::` —
    /// classifies to `None`, which the endpoints answer 400 `"Invalid emoji"`.
    ///
    /// A `Some(Self::Custom)` only says the *spelling* is a shortcode: whether
    /// this instance has that emoji is the caller's database question.
    #[must_use]
    pub fn classify(text: &'a str) -> Option<Self> {
        shortcode(text).map_or_else(
            || is_single_emoji(text).then_some(Self::Unicode(text)),
            |code| Some(Self::Custom(code)),
        )
    }
}

/// The shortcode of `:shortcode:` text, `None` when `text` is not that
/// shape: colons at both ends and at least one `[A-Za-z0-9_]` between them.
///
/// The inner text may not hold another colon, so `:a::b:` is not one
/// shortcode and neither is a colon-wrapped string with anything else added.
#[must_use]
pub fn shortcode(text: &str) -> Option<&str> {
    let inner = text.strip_prefix(':')?.strip_suffix(':')?;
    (!inner.is_empty()
        && inner
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'))
    .then_some(inner)
}

/// Whether `text` is exactly one grapheme cluster that is an emoji.
///
/// The cluster count is Unicode's extended grapheme cluster rule (what a
/// double-emoji sequence, a ZWJ family, a flag pair and a skin-tone modifier
/// each count as), so `🙂🙂` is two clusters while `👍🏽`, `🇺🇸` and
/// `👨‍👩‍👧` are one each. Whether that one cluster *is* an emoji is the
/// table's answer: every fully-qualified sequence of Unicode's
/// `emoji-test.txt`, those sequences with their U+FE0F selectors left out
/// (so the unqualified `❤` passes like `❤️`) and the lone regional
/// indicators — see the `table` submodule. A letter wearing a skin-tone
/// modifier, or a lone joiner or selector, is one cluster and is not an
/// emoji.
#[must_use]
pub fn is_single_emoji(text: &str) -> bool {
    let mut clusters = text.graphemes(true);
    let (Some(cluster), None) = (clusters.next(), clusters.next()) else {
        return false;
    };
    table::is_emoji(cluster)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `text` classifies to, as a plain string so the assertions read
    /// like the shapes they name.
    fn classified(text: &str) -> String {
        Emoji::classify(text).map_or_else(
            || "nothing".to_owned(),
            |emoji| match emoji {
                Emoji::Unicode(emoji) => format!("unicode {emoji}"),
                Emoji::Custom(shortcode) => format!("custom {shortcode}"),
            },
        )
    }

    #[test]
    fn one_emoji_grapheme_is_a_unicode_reaction() {
        for text in [
            "🙂",
            "👍",
            "🎉",
            "❤",
            "☺",
            "🍺",
            "🚀",
            "🧠",
            "🩷",
            "👍🏽",
            "🇺🇸",
            "🏴\u{200D}☠\u{FE0F}",
            "👨\u{200D}👩\u{200D}👧\u{200D}👦",
            "#\u{FE0F}\u{20E3}",
            "9\u{FE0F}\u{20E3}",
            "\u{2B50}",
            "\u{231B}",
            "\u{203C}\u{FE0F}",
            "\u{A9}\u{FE0F}",
            "\u{1F004}",
            "\u{1F7E5}",
            "\u{1F170}\u{FE0F}",
            "\u{2764}",
            "\u{2764}\u{FE0F}",
            "\u{1F44D}\u{1F3FD}",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            "\u{1F1EF}\u{1F1F5}",
        ] {
            assert!(is_single_emoji(text), "{text} is one emoji grapheme");
            assert_eq!(
                classified(text),
                format!("unicode {text}"),
                "{text} classifies as unicode"
            );
        }
    }

    #[test]
    fn anything_that_is_not_one_emoji_grapheme_is_not() {
        for text in [
            "",
            " ",
            "a",
            "word",
            "🙂🙂",
            "🙂a",
            "a🙂",
            "🙂 ",
            "\u{200D}",
            "\u{FE0F}",
            "\u{20E3}",
            "a\u{FE0F}",
            "\u{1F600}x",
            "\u{1F600}\u{1F600}",
            "a\u{1F3FB}",
            "ab",
            "x",
            "\u{1F44D}\u{1F44D}",
        ] {
            assert!(!is_single_emoji(text), "{text} is not one emoji");
            assert_eq!(classified(text), "nothing", "{text} classifies to nothing");
        }
    }

    #[test]
    fn a_shortcode_is_a_custom_reaction_reference() {
        for (text, code) in [
            (":cat:", "cat"),
            (":railroad_track:", "railroad_track"),
            (":Cat99:", "Cat99"),
            (":_:", "_"),
            (":2hu:", "2hu"),
        ] {
            assert_eq!(classified(text), format!("custom {code}"), "{text}");
        }
    }

    #[test]
    fn a_non_shortcode_spelling_is_rejected() {
        for text in [
            ":", "::", ":a b:", ":a-b:", ":a.b:", ":é:", ":a::b:", "cat", ":x",
        ] {
            assert_eq!(classified(text), "nothing", "{text} is no emoji");
        }
    }

    #[test]
    fn shortcode_reads_the_shape_only() {
        assert_eq!(shortcode(":cat:"), Some("cat"), "the colons are not kept");
        assert_eq!(shortcode(":"), None, "a lone colon is empty inside");
        assert_eq!(shortcode("::"), None, "so is nothing between colons");
        assert_eq!(shortcode("cat"), None, "colons are required");
        assert_eq!(shortcode(":a;b:"), None, "and only word characters ride");
    }

    #[test]
    fn classification_prefers_the_shortcode_shape() {
        assert_eq!(
            Emoji::classify(":cat:"),
            Some(Emoji::Custom("cat")),
            "colon text is a shortcode, whatever the caller then finds for it"
        );
        assert_eq!(
            Emoji::classify("🐱"),
            Some(Emoji::Unicode("🐱")),
            "and an emoji keeps the client's own spelling"
        );
    }
}
