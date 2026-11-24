//! The one reading of a boolean parameter's text.
//!
//! Every query-string flag and every form or JSON string flag goes through
//! [`truthy`], so `?local=yes` means the same thing wherever it is read.

/// The boolean a parameter's text spells, `None` when it spells neither.
///
/// Case- and whitespace-insensitive: `true`, `1`, `on`, `yes` and `t` are
/// `Some(true)`; `false`, `0`, `off`, `no` and `f` are `Some(false)`.
#[must_use]
pub fn truthy(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" | "t" => Some(true),
        "false" | "0" | "off" | "no" | "f" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_true_spellings_are_true_in_both_cases() {
        for text in ["true", "1", "on", "yes", "t"] {
            assert_eq!(truthy(text), Some(true), "`{text}`");
            assert_eq!(
                truthy(&text.to_ascii_uppercase()),
                Some(true),
                "`{text}` upper"
            );
        }
    }

    #[test]
    fn the_five_false_spellings_are_false_in_both_cases() {
        for text in ["false", "0", "off", "no", "f"] {
            assert_eq!(truthy(text), Some(false), "`{text}`");
            assert_eq!(
                truthy(&text.to_ascii_uppercase()),
                Some(false),
                "`{text}` upper"
            );
        }
    }

    #[test]
    fn mixed_case_reads_like_any_other_case() {
        assert_eq!(truthy("TrUe"), Some(true));
        assert_eq!(truthy("oFf"), Some(false));
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(truthy("  yes\t"), Some(true));
        assert_eq!(truthy("\n0 "), Some(false));
    }

    #[test]
    fn anything_else_is_neither() {
        for text in ["", "   ", "maybe", "2", "truee", "y", "n", "tru e"] {
            assert_eq!(truthy(text), None, "`{text}`");
        }
    }
}
