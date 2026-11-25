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

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation as _;

mod table;

/// The U+FE0F variation selector that asks for emoji presentation.
const VARIATION_SELECTOR: char = '\u{FE0F}';

/// The U+200D joiner that glues the parts of a ZWJ sequence together.
const ZERO_WIDTH_JOINER: char = '\u{200D}';

/// A reaction emoji as the API names it, classified from its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Emoji<'a> {
    /// One unicode emoji grapheme cluster, e.g. `🙂` or `👨‍👩‍👧`;
    /// the variant holds the client's own text.
    Unicode(&'a str),
    /// A `:shortcode:` reference to a custom emoji; the variant holds the
    /// bare `shortcode`, i.e. the `[A-Za-z0-9_+-]+` between the colons.
    Custom(&'a str),
}

impl<'a> Emoji<'a> {
    /// Classifies `text` as a reaction emoji, `None` for anything else.
    ///
    /// The two accepted spellings are [`Emoji::Custom`] for `:shortcode:`
    /// text whose shortcode is non-empty and only of `[A-Za-z0-9_+-]` —
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
/// shape: colons at both ends and at least one `[A-Za-z0-9_+-]` between them.
///
/// The set is a superset of Pleroma's own matcher (`[A-Za-z0-9_-]+`), so
/// pack names like `blob-cat` and `blobfox+1` work. The inner text may not
/// hold another colon, so `:a::b:` is not one shortcode and neither is a
/// colon-wrapped string with anything else added.
#[must_use]
pub fn shortcode(text: &str) -> Option<&str> {
    let inner = text.strip_prefix(':')?.strip_suffix(':')?;
    is_shortcode_body(inner).then_some(inner)
}

/// The `(shortcode, domain)` of a remote custom emoji spelled
/// `shortcode@domain`, with or without the surrounding colons; `None` for any
/// other text.
///
/// This is Pleroma's `:shortcode@domain:` reaction shape
/// (`@emoji_regex ~r/:[A-Za-z0-9_-]+(@.+)?:/`). The colons come both or not at
/// all, the shortcode is [`shortcode`]'s character set, and the domain is a
/// dotted host name of letters, digits, `-` and `.` with an optional numeric
/// `:port`, so it is returned lowercased the way stored domains are.
#[must_use]
pub fn remote_shortcode(text: &str) -> Option<(&str, String)> {
    let inner = match (text.strip_prefix(':'), text.ends_with(':')) {
        (Some(rest), true) => rest.strip_suffix(':')?,
        (None, false) => text,
        _ => return None,
    };
    let (code, domain) = inner.split_once('@')?;
    let code_ok = !code.is_empty() && code.chars().all(is_shortcode_char);
    let domain_ok = is_domain(domain);
    (code_ok && domain_ok).then(|| (code, domain.to_ascii_lowercase()))
}

/// Whether `domain` is a plausible host name with an optional `:port`:
/// non-empty labels of ASCII letters, digits and `-`, separated by single dots.
fn is_domain(domain: &str) -> bool {
    let (host, port) = domain
        .split_once(':')
        .map_or((domain, None), |(host, port)| (host, Some(port)));
    let port_ok = port
        .is_none_or(|digits| !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()));
    port_ok
        && host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        })
}

/// The shortcode of a bare `shortcode` spelling of a local custom emoji, the
/// way Pleroma names its own; `None` for anything else.
///
/// Bare means no colons and no `@domain`: non-empty, only `[A-Za-z0-9_+-]`,
/// and not itself one unicode emoji. Whether this instance has such an emoji is
/// the caller's database question.
#[must_use]
pub fn bare_shortcode(text: &str) -> Option<&str> {
    (is_shortcode_body(text) && !is_single_emoji(text)).then_some(text)
}

/// The name a reaction goes by on the API: `shortcode@domain` for a custom
/// emoji of another instance and the bare `shortcode` for a local one (Pleroma's
/// spellings), the stored text otherwise.
///
/// `emoji` is the stored spelling and `domain` the emoji's origin, `None` for
/// a unicode or local emoji. Text that is not a `:shortcode:` keeps its own
/// spelling whatever `domain` says.
#[must_use]
pub fn display_name(emoji: &str, domain: Option<&str>) -> String {
    match (domain, shortcode(emoji)) {
        (Some(domain), Some(code)) => format!("{code}@{domain}"),
        (None, Some(code)) => code.to_owned(),
        _ => qualified(emoji),
    }
}

/// The grouping key of a unicode reaction: `emoji` with every U+FE0F removed
/// unless it holds a zero-width joiner, in which case it is returned as it is.
///
/// This is Misskey's own rule (`ReactionService.normalize`), so `❤` and `❤️`
/// are one reaction and a keycap with or without its selector is too. Text
/// without a U+FE0F, a `:shortcode:` among it, is its own key.
#[must_use]
pub fn canonical(emoji: &str) -> Cow<'_, str> {
    if emoji.contains(ZERO_WIDTH_JOINER) || !emoji.contains(VARIATION_SELECTOR) {
        return Cow::Borrowed(emoji);
    }
    Cow::Owned(
        emoji
            .chars()
            .filter(|&ch| ch != VARIATION_SELECTOR)
            .collect(),
    )
}

/// The fully qualified spelling of a unicode reaction: what Mastodon and
/// Pleroma clients render with emoji presentation (`❤️`, not `❤`).
///
/// A sequence with a zero-width joiner, and any text the emoji table does not
/// know (a `:shortcode:` among it), comes back as it is.
#[must_use]
pub fn qualified(emoji: &str) -> String {
    if emoji.contains(ZERO_WIDTH_JOINER) {
        return emoji.to_owned();
    }
    table::qualify(emoji).map_or_else(|| emoji.to_owned(), str::to_owned)
}

/// Whether `ch` may appear between the colons of a shortcode.
#[must_use]
pub const fn is_shortcode_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '+')
}

/// Whether `inner` is a usable shortcode between colons: non-empty, only
/// `[A-Za-z0-9_+-]`.
fn is_shortcode_body(inner: &str) -> bool {
    !inner.is_empty() && inner.chars().all(is_shortcode_char)
}

/// Every `:shortcode:` token in `text`, bare and in order of appearance,
/// repeats included.
///
/// A token is a run of `[A-Za-z0-9_+-]` with a colon at both ends, so `:a:b:`
/// yields `a` and `b`, and `12:30` or `http://x` yield nothing. Whether a
/// token names a real emoji is the caller's database question.
#[must_use]
pub fn shortcodes_in(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut parts = text.split(':').skip(1).peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_some() && is_shortcode_body(part) {
            found.push(part);
        }
    }
    found
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
            (":blob-cat:", "blob-cat"),
            (":blobfox+1:", "blobfox+1"),
            (":a-b_c:", "a-b_c"),
        ] {
            assert_eq!(classified(text), format!("custom {code}"), "{text}");
        }
    }

    #[test]
    fn a_non_shortcode_spelling_is_rejected() {
        for text in [":", "::", ":a b:", ":a.b:", ":é:", ":a::b:", "cat", ":x"] {
            assert_eq!(classified(text), "nothing", "{text} is no emoji");
        }
    }

    #[test]
    fn shortcode_reads_the_shape_only() {
        assert_eq!(shortcode(":cat:"), Some("cat"), "the colons are not kept");
        assert_eq!(shortcode(":"), None, "a lone colon is empty inside");
        assert_eq!(shortcode("::"), None, "so is nothing between colons");
        assert_eq!(shortcode("cat"), None, "colons are required");
        assert_eq!(
            shortcode(":a;b:"),
            None,
            "and only shortcode characters ride"
        );
        assert_eq!(
            shortcode(":a b:"),
            None,
            "a space is no shortcode character"
        );
        assert_eq!(shortcode(":a:b:"), None, "a colon inside is refused");
    }

    #[test]
    fn shortcodes_in_finds_colon_wrapped_runs_in_order() {
        assert_eq!(
            shortcodes_in("hi :cat: and :dog: or :cat:"),
            vec!["cat", "dog", "cat"],
            "order kept, repeats kept"
        );
        assert_eq!(shortcodes_in(":a:b:"), vec!["a", "b"], "adjacent tokens");
        assert_eq!(shortcodes_in("<p>:x_1:</p>"), vec!["x_1"], "markup around");
        assert_eq!(
            shortcodes_in("x :blob-cat: :blobfox+1:"),
            vec!["blob-cat", "blobfox+1"],
            "hyphen and plus are shortcode characters"
        );
    }

    #[test]
    fn shortcodes_in_ignores_text_that_is_no_token() {
        for text in [
            "",
            "cat",
            ":cat",
            "cat:",
            "12:30",
            "http://x.example",
            "::",
            ":a b:",
        ] {
            assert!(shortcodes_in(text).is_empty(), "{text:?} has no token");
        }
    }

    #[test]
    fn a_remote_shortcode_reads_with_or_without_colons() {
        for text in [
            "misskeycat@misskey.test",
            ":misskeycat@misskey.test:",
            "misskeycat@Misskey.Test",
        ] {
            assert_eq!(
                remote_shortcode(text),
                Some(("misskeycat", "misskey.test".to_owned())),
                "{text}"
            );
        }
        assert_eq!(
            remote_shortcode("cat@127.0.0.1:8080"),
            Some(("cat", "127.0.0.1:8080".to_owned())),
            "a port rides along"
        );
        assert_eq!(
            remote_shortcode(":cat@127.0.0.1:8080:"),
            Some(("cat", "127.0.0.1:8080".to_owned())),
            "inside colons too"
        );
        assert_eq!(
            remote_shortcode(":blob-cat+1@a-b.example.org:"),
            Some(("blob-cat+1", "a-b.example.org".to_owned())),
            "the shortcode set and a multi-label domain"
        );
    }

    #[test]
    fn a_non_remote_spelling_is_rejected() {
        for text in [
            "",
            ":",
            ":cat:",
            "cat",
            "@misskey.test",
            "cat@",
            ":cat@misskey.test",
            "cat@misskey.test:",
            ":cat@:",
            "cat@mis skey.test",
            "cat@misskey..test",
            "cat@.test",
            "cat@misskey.test.",
            "c.at@misskey.test",
            "cat@a@b",
            "cat@misskey.test:",
            "cat@misskey.test:80a",
            "cat@misskey.test:8:0",
            "🐱@misskey.test",
        ] {
            assert_eq!(remote_shortcode(text), None, "{text:?} is not remote");
        }
    }

    #[test]
    fn a_remote_emoji_is_named_shortcode_at_domain() {
        assert_eq!(
            display_name(":misskeycat:", Some("misskey.test")),
            "misskeycat@misskey.test",
            "Pleroma's name for a remote custom reaction"
        );
        assert_eq!(
            display_name(":cat:", None),
            "cat",
            "a local one is named by its bare shortcode, as Pleroma does"
        );
        assert_eq!(display_name("👍", None), "👍", "so does a unicode one");
        assert_eq!(
            display_name("👍", Some("misskey.test")),
            "👍",
            "a domain cannot rename text that is no shortcode"
        );
    }

    #[test]
    fn the_grouping_key_drops_every_selector_unless_a_joiner_is_there() {
        for (text, key) in [
            ("\u{2764}\u{FE0F}", "\u{2764}"),
            ("\u{2764}", "\u{2764}"),
            ("1\u{FE0F}\u{20E3}", "1\u{20E3}"),
            ("1\u{20E3}", "1\u{20E3}"),
            ("\u{1F44D}", "\u{1F44D}"),
            (":cat:", ":cat:"),
        ] {
            assert_eq!(canonical(text), key, "{text:?}");
        }
        for sequence in [
            "\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}",
            "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
            "\u{1F3F4}\u{200D}\u{2620}\u{FE0F}",
        ] {
            assert_eq!(canonical(sequence), sequence, "a ZWJ sequence is kept");
        }
    }

    #[test]
    fn the_qualified_form_restores_the_selector() {
        for (text, name) in [
            ("\u{2764}", "\u{2764}\u{FE0F}"),
            ("\u{2764}\u{FE0F}", "\u{2764}\u{FE0F}"),
            ("1\u{20E3}", "1\u{FE0F}\u{20E3}"),
            ("\u{1F44D}", "\u{1F44D}"),
            (
                "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
                "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
            ),
            (
                "\u{1F3F4}\u{200D}\u{2620}\u{FE0F}",
                "\u{1F3F4}\u{200D}\u{2620}\u{FE0F}",
            ),
            (":cat:", ":cat:"),
        ] {
            assert_eq!(qualified(text), name, "{text:?}");
        }
        assert_eq!(
            display_name("\u{2764}", None),
            "\u{2764}\u{FE0F}",
            "a stored heart is named with emoji presentation"
        );
    }

    #[test]
    fn a_bare_shortcode_is_a_word_of_shortcode_characters() {
        for text in ["cat", "blob-cat", "blobfox+1", "a_b", "Cat99", "2hu"] {
            assert_eq!(bare_shortcode(text), Some(text), "{text}");
        }
        for text in [
            "",
            ":cat:",
            ":cat",
            "cat:",
            "a b",
            "a.b",
            "cat@x.test",
            "🐱",
            "é",
            "::",
        ] {
            assert_eq!(bare_shortcode(text), None, "{text:?} is not a bare code");
        }
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
