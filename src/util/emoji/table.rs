//! The set of emoji a reaction may be: every fully-qualified sequence of
//! Unicode's `emoji-test.txt`, the way Pleroma's `Pleroma.Emoji` builds its
//! list from the copy it ships (`lib/pleroma/emoji-test.txt`, Emoji 15.0).
//!
//! The data lives in `table/*.txt`: the `fully-qualified` lines of that file,
//! verbatim and split by first code point so that no file outgrows the length
//! gate. Each line reads `<code points> ; fully-qualified # <emoji> E<ver>
//! <name>`, and the one thing this module takes from it is the `<emoji>`
//! column. Nothing is generated at build time: refreshing the table is a
//! matter of re-extracting those lines for a newer Unicode release.
//!
//! The lookup is [`is_emoji`]: a sequence is an emoji when it is in the table,
//! when it is one with its variation selectors (U+FE0F) left out — Pleroma's
//! `emoji_qualification_map`, which lets a client send the unqualified `❤`
//! for `❤️` — or when it is a lone regional indicator, which Pleroma also
//! accepts.

use std::sync::LazyLock;

/// The U+FE0F variation selector that makes a sequence's base emoji-presented.
const VARIATION_SELECTOR: char = '\u{FE0F}';

/// The files of `emoji-test.txt` lines, in no particular order.
const SOURCES: [&str; 8] = [
    include_str!("table/a.txt"),
    include_str!("table/b.txt"),
    include_str!("table/c.txt"),
    include_str!("table/d.txt"),
    include_str!("table/e.txt"),
    include_str!("table/f.txt"),
    include_str!("table/g.txt"),
    include_str!("table/h.txt"),
];

/// The lookup structure built once from [`SOURCES`].
struct Table {
    /// Every fully-qualified sequence, sorted for binary search.
    qualified: Vec<&'static str>,
    /// The same sequences with every U+FE0F removed, sorted for binary search.
    bare: Vec<String>,
}

/// The table, parsed on first use.
static TABLE: LazyLock<Table> = LazyLock::new(build);

/// The emoji column of one `emoji-test.txt` line: the text after the `# ` of
/// the comment up to the next space, `None` for a line without one.
fn sequence_of(line: &'static str) -> Option<&'static str> {
    let (_, comment) = line.split_once("# ")?;
    comment.split(' ').next().filter(|emoji| !emoji.is_empty())
}

/// `sequence` with every U+FE0F left out.
fn without_selectors(sequence: &str) -> String {
    sequence
        .chars()
        .filter(|&ch| ch != VARIATION_SELECTOR)
        .collect()
}

/// Parses [`SOURCES`] into the sorted lookup vectors.
fn build() -> Table {
    let mut qualified: Vec<&'static str> = SOURCES
        .iter()
        .flat_map(|source| source.lines())
        .filter_map(sequence_of)
        .collect();
    qualified.sort_unstable();
    qualified.dedup();
    let mut bare: Vec<String> = qualified
        .iter()
        .map(|sequence| without_selectors(sequence))
        .collect();
    bare.sort_unstable();
    bare.dedup();
    Table { qualified, bare }
}

/// Whether `ch` is one of the 26 regional indicators U+1F1E6–U+1F1FF.
const fn is_regional_indicator(ch: char) -> bool {
    matches!(ch, '\u{1F1E6}'..='\u{1F1FF}')
}

/// Whether `sequence` is an emoji: a fully-qualified entry of the table, an
/// entry with its variation selectors dropped, or a lone regional indicator.
///
/// The text is taken as it stands: it is the caller's job to have cut it to
/// one grapheme cluster, and a sequence of several emoji is not in the table.
#[must_use]
pub(super) fn is_emoji(sequence: &str) -> bool {
    let table = &*TABLE;
    if table.qualified.binary_search(&sequence).is_ok() {
        return true;
    }
    let mut chars = sequence.chars();
    if let (Some(only), None) = (chars.next(), chars.next())
        && is_regional_indicator(only)
    {
        return true;
    }
    let bare = without_selectors(sequence);
    !bare.is_empty() && table.bare.binary_search(&bare).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_holds_every_fully_qualified_sequence() {
        let table = &*TABLE;
        assert!(
            table.qualified.len() > 3600,
            "the table has {} sequences",
            table.qualified.len()
        );
        assert!(
            table
                .qualified
                .windows(2)
                .all(|pair| matches!(pair, [a, b] if a < b)),
            "the table is sorted without duplicates"
        );
        assert!(
            table
                .bare
                .windows(2)
                .all(|pair| matches!(pair, [a, b] if a < b)),
            "the bare spellings are sorted without duplicates"
        );
    }

    #[test]
    fn sequence_of_reads_the_emoji_column() {
        assert_eq!(
            sequence_of("1F600 ; fully-qualified # \u{1F600} E1.0 grinning face"),
            Some("\u{1F600}"),
            "the column after the comment marker"
        );
        assert_eq!(
            sequence_of("0023 FE0F 20E3 ; fully-qualified # #\u{FE0F}\u{20E3} E0.6 keycap: #"),
            Some("#\u{FE0F}\u{20E3}"),
            "a keycap's own # does not confuse the comment marker"
        );
        assert_eq!(
            sequence_of("no comment here"),
            None,
            "a line without a comment has no emoji"
        );
        assert_eq!(
            sequence_of("1F600 ; fully-qualified # "),
            None,
            "nor does one with an empty comment"
        );
    }

    #[test]
    fn emoji_outside_the_old_blocks_are_in_the_table() {
        for sequence in [
            "\u{2B50}",
            "\u{2B1B}",
            "\u{231B}",
            "\u{23F0}",
            "\u{203C}\u{FE0F}",
            "\u{A9}\u{FE0F}",
            "\u{2122}\u{FE0F}",
            "\u{3030}\u{FE0F}",
            "\u{3297}\u{FE0F}",
            "\u{1F004}",
            "\u{1F0CF}",
            "\u{1F170}\u{FE0F}",
            "\u{1F201}",
            "\u{1F7E5}",
            "\u{1F7E2}",
        ] {
            assert!(is_emoji(sequence), "{sequence:?} is an emoji");
        }
    }

    #[test]
    fn an_unqualified_spelling_is_accepted() {
        assert!(is_emoji("\u{2764}"), "❤ without its selector");
        assert!(is_emoji("\u{2764}\u{FE0F}"), "❤ with it");
        assert!(is_emoji("#\u{20E3}"), "a keycap without its selector");
    }

    #[test]
    fn every_regional_indicator_alone_is_an_emoji() {
        for ch in '\u{1F1E6}'..='\u{1F1FF}' {
            assert!(is_emoji(&ch.to_string()), "{ch:?} is an emoji");
        }
        assert!(is_emoji("🇯🇵"), "a flag pair is too");
        assert!(!is_emoji("\u{1F1E5}"), "the code point before the block");
        assert!(!is_emoji("\u{1F200}"), "and the one after it");
    }

    #[test]
    fn text_that_is_not_in_the_table_is_not_an_emoji() {
        for sequence in [
            "",
            "x",
            "a",
            "0",
            "#",
            "ab",
            "\u{FE0F}",
            "a\u{FE0F}",
            "a\u{1F3FB}",
            "\u{20E3}",
            "👍👍",
            "\u{200D}",
        ] {
            assert!(!is_emoji(sequence), "{sequence:?} is not an emoji");
        }
    }
}
