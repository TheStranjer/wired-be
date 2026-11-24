//! The parameter trimming the routes read their requests through.

/// `value` trimmed, or the empty string when the request carried none.
fn trimmed(value: Option<&str>) -> &str {
    value.map_or("", str::trim)
}

/// The parameter `value` when the request carried it non-blank after
/// trimming — the presence rule of every required parameter here.
pub(super) fn required(value: Option<&str>) -> Option<&str> {
    let text = trimmed(value);
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_parameters_are_present_and_non_blank() {
        assert_eq!(required(None), None, "an absent parameter");
        assert_eq!(required(Some("")), None, "an empty one");
        assert_eq!(required(Some("   ")), None, "a blank one");
        assert_eq!(
            required(Some(" https://x.example/a ")),
            Some("https://x.example/a"),
            "a value is trimmed"
        );
        assert_eq!(trimmed(None), "", "and an absent value reads as empty");
        assert_eq!(
            trimmed(Some(" https://x.example/a ")),
            "https://x.example/a"
        );
    }
}
