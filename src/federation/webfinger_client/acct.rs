//! The handle rules: what an `acct` string may look like before any request
//! is made, and the prefix, nickname, host, IPv4 and port alphabets
//! [`split_acct`] checks it against.

use super::ACCT_PREFIX;

/// Splits a `WebFinger` handle into its `(nickname, domain)` parts, with the
/// domain lowercased.
///
/// Accepts `nick@domain` with any leading `@` and `acct:` (in either order,
/// case-insensitively for the scheme). The nickname must be non-empty and
/// made of the handle alphabet `[A-Za-z0-9._-]` (Pleroma's `webfinger`
/// regex, `[a-z0-9A-Z_\.-]+`); the domain must be a dotted hostname of ASCII
/// letters, digits and interior hyphens whose top-level label starts with a
/// letter, or a dotted IPv4 address, either one optionally suffixed with
/// `:port` (Mastodon's `Addressable` keeps the port in the domain likewise:
/// an instance on an unusual port, and the loopback peers the suites drive,
/// are addressable handles exactly like `nick@host.tld`). A single-label
/// host stays out, because the lookup builds the authority straight out of
/// the domain. The nickname is returned as given; the domain is lowercased.
#[must_use]
pub fn split_acct(acct: &str) -> Option<(String, String)> {
    let (nickname, domain) = strip_finger_prefixes(acct).split_once('@')?;
    if nickname.is_empty() || !nickname.chars().all(is_nickname_char) {
        return None;
    }
    let (host, port) = split_domain(domain);
    if !is_valid_host(host) || !is_valid_port(port) {
        return None;
    }
    Some((nickname.to_owned(), domain.to_ascii_lowercase()))
}

/// The `host` and `:port` parts of a requested domain: the port is the text
/// behind a `:` that follows a non-empty host; `None` carries no separator.
/// IPv6 bracket forms are out of this instance's federation vocabulary, so
/// a trailing colon-separated part is always read as a port.
fn split_domain(domain: &str) -> (&str, Option<&str>) {
    match domain.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => (host, Some(port)),
        _ => (domain, None),
    }
}

/// Whether `port` text names a usable TCP port: `None` means the scheme's
/// default, anything else one to five digits resolving into `1..=65535`.
/// `0`, non-digits and out-of-range numbers name nothing.
fn is_valid_port(port: Option<&str>) -> bool {
    port.is_none_or(|text| {
        !text.is_empty()
            && text.len() <= 5
            && text.chars().all(|c| c.is_ascii_digit())
            && text
                .parse::<u32>()
                .is_ok_and(|value| (1..=65535).contains(&value))
    })
}

/// `acct` without its leading `@` and `acct:` prefixes (each optional, in
/// either order). Crate-visible so [`crate::federation::fetch_actor`]'s
/// handle round trip can fold a requested `acct:` to the bare
/// `nick@domain` its mirrors store.
pub(crate) fn strip_finger_prefixes(acct: &str) -> &str {
    let without_at = acct.strip_prefix('@').unwrap_or(acct);
    let without_scheme = strip_prefix_ignore_case(without_at, ACCT_PREFIX).unwrap_or(without_at);
    without_scheme.strip_prefix('@').unwrap_or(without_scheme)
}

/// `value` with a case-insensitive ASCII `prefix` removed, `None` when
/// `value` does not start with it.
pub(super) fn strip_prefix_ignore_case<'value>(
    value: &'value str,
    prefix: &str,
) -> Option<&'value str> {
    let mut remaining = value;
    for expected in prefix.chars() {
        match remaining.chars().next() {
            Some(found) if found.eq_ignore_ascii_case(&expected) => {
                remaining = remaining.split_at(found.len_utf8()).1;
            }
            _ => return None,
        }
    }
    Some(remaining)
}

/// One character of the handle alphabet [`split_acct`] accepts in the
/// nickname: ASCII alphanumerics plus `.`, `_` and `-`.
const fn is_nickname_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Whether `host` is a hostname [`split_acct`] accepts: either a dotted
/// domain of at least two labels, each 1..=63 characters of ASCII letters,
/// digits and interior hyphens with the top-level label starting with a
/// letter, or a dotted IPv4 address — four decimal octets of at most three
/// digits each.
fn is_valid_host(host: &str) -> bool {
    is_valid_ipv4(host)
        || (host.contains('.')
            && host.split('.').all(is_valid_label)
            && host
                .split('.')
                .next_back()
                .is_some_and(|tld| tld.starts_with(|c: char| c.is_ascii_alphabetic())))
}

/// Whether `host` is a dotted IPv4 address: exactly four labels of one to
/// three ASCII digits each, every one of them at most `255`.
fn is_valid_ipv4(host: &str) -> bool {
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 3
                && label.chars().all(|c| c.is_ascii_digit())
                && label.parse::<u8>().is_ok()
        })
}

/// One DNS label as [`is_valid_host`] requires it for a domain name.
fn is_valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_acct_parses_valid_handles() {
        for (input, nickname, domain) in [
            ("alice@remote.example", "alice", "remote.example"),
            ("@alice@remote.example", "alice", "remote.example"),
            ("acct:alice@remote.example", "alice", "remote.example"),
            ("@acct:alice@remote.example", "alice", "remote.example"),
            ("acct:@alice@remote.example", "alice", "remote.example"),
            ("ACCT:alice@remote.example", "alice", "remote.example"),
            ("Alice.U-2@a1.example.io", "Alice.U-2", "a1.example.io"),
            ("alice@Remote.Example", "alice", "remote.example"),
            ("alice@remote.example:8080", "alice", "remote.example:8080"),
            ("alice@127.0.0.1:4000", "alice", "127.0.0.1:4000"),
            ("alice@127.0.0.1", "alice", "127.0.0.1"),
            ("alice@Remote.Example:80", "alice", "remote.example:80"),
        ] {
            assert_eq!(
                split_acct(input),
                Some((nickname.to_owned(), domain.to_owned())),
                "{input} is a valid handle"
            );
        }
    }

    #[test]
    fn split_acct_rejects_invalid_handles() {
        for input in [
            "",
            "@",
            "acct:",
            "alice",
            "alice@",
            "@remote.example",
            "acct:alice",
            "alice@remote",
            "alice@remote.example:",
            "alice@remote.example:0",
            "alice@remote.example:65536",
            "alice@remote.example:80x",
            "alice@remote.example:notaport",
            "alice@:8080",
            "alice@127.0.0.1:0",
            "alice@1.2.3",
            "alice@127.0.0.256:1",
            "alice@-bad.example",
            "alice@bad-.example",
            "alice@exam_ple.example",
            "ali ce@remote.example",
            "ali$ce@remote.example",
            "ali(ce@remote.example",
            "müller@remote.example",
            "alice@@remote.example",
            "alice@remote..example",
            "alice@.example",
            "alice@remote.",
            "alice@remote.1",
        ] {
            assert_eq!(split_acct(input), None, "{input} must be rejected");
        }
    }

    #[test]
    fn prefix_stripping_handles_case_and_order() {
        assert_eq!(
            strip_finger_prefixes("@acct:alice@x.example"),
            "alice@x.example"
        );
        assert_eq!(
            strip_finger_prefixes("Acct:alice@x.example"),
            "alice@x.example"
        );
        assert_eq!(
            strip_finger_prefixes("@@alice@x.example"),
            "alice@x.example",
            "each prefix position strips at most one layer"
        );
        assert_eq!(strip_finger_prefixes("alice@x.example"), "alice@x.example");
    }
}
