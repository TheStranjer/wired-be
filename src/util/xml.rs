//! Hand-rolled XML helpers for the small documents this API serves.
//!
//! The XRD documents of [`crate::api::well_known`] are fixed, shallow trees,
//! so no XML library is warranted; what is needed is escaping of the
//! database-sourced values that go into them. [`xml_escape`] implements the
//! five predefined XML entities (`&amp;`, `&lt;`, `&gt;`, `&quot;`,
//! `&apos;`), valid in both element text and attribute values.

/// `value` with every XML special character replaced by its predefined entity.
///
/// The result can be interpolated into an element text node or a quoted
/// attribute value as-is. The five entities are `&amp;`, `&lt;`, `&gt;`,
/// `&quot;` and `&apos;`; everything else, including other Unicode, is
/// copied through.
#[must_use]
pub fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_copied_through() {
        for value in [
            "",
            "alice",
            "acct:alice@test.example",
            "https://test.example/users/alice",
            "héllo wörld — ✓",
        ] {
            assert_eq!(xml_escape(value), value, "{value:?} needs no escaping");
        }
    }

    #[test]
    fn every_special_character_becomes_its_entity() {
        assert_eq!(xml_escape("&"), "&amp;", "ampersand");
        assert_eq!(xml_escape("<"), "&lt;", "less-than");
        assert_eq!(xml_escape(">"), "&gt;", "greater-than");
        assert_eq!(xml_escape("\""), "&quot;", "double quote");
        assert_eq!(xml_escape("'"), "&apos;", "single quote");
    }

    #[test]
    fn mixed_text_escapes_every_occurrence_without_double_escaping() {
        assert_eq!(
            xml_escape("a<b>&c\"d'e"),
            "a&lt;b&gt;&amp;c&quot;d&apos;e",
            "each character is replaced once"
        );
        assert_eq!(
            xml_escape("<a href=\"x\">&amp;</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;amp;&lt;/a&gt;",
            "text that already looks like entities is escaped as literal text"
        );
    }
}
