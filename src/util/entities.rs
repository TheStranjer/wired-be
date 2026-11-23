//! Mention and hashtag extraction and the HTML their links are built
//! from, following Pleroma (`lib/pleroma/formatter.ex`, and the link
//! builders in `lib/pleroma/web/common_api/utils.ex`).
//!
//! [`extract_mentions`] finds `@nick` and `@nick@domain.tld` references in
//! plain text and [`extract_hashtags`] finds `#tag` references; both dedupe
//! case-insensitively and return results in order of first appearance. The
//! HTML side has one builder per entity kind, [`mention_html`] and
//! [`hashtag_html`], and [`linkify_entities`], which walks already-escaped
//! text or HTML (the output of [`text_to_html`]) and replaces every entity it
//! can resolve, leaving the text inside existing anchors untouched.
//!
//! Nothing here touches the database or HTTP: mention resolution is a
//! callback, so the caller decides which [`MentionRef`]s are known and which
//! profile URL each of them gets.
//!
//! [`text_to_html`]: crate::util::format::text_to_html

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::{models::hashtag::normalize_name, util::sanitize::escape_html};

/// The mention pattern: a leading start-of-text, whitespace or one of `(`,
/// `[`, `{`, then `@` and a nickname of `[A-Za-z0-9_.-]` whose last character
/// is alphanumeric or `_`, then an optional `@domain` of `[A-Za-z0-9.-]` with
/// at least one dot and an alphanumeric last character — so sentence-final
/// `.` and `-` fall out of the match — optionally suffixed with `:port`
/// digits, which belong to the host the federation lookups key on. The lead
/// is a capture group (re-emitted by [`linkify_entities`]) because the
/// `regex` crate has no look-behind.
const MENTION_PATTERN: &str = r"(?<lead>^|[\s(\[{])@(?<nick>[A-Za-z0-9_.-]*[A-Za-z0-9_])(?:@(?<domain>(?:[A-Za-z0-9-]+\.)+[A-Za-z0-9-]*[A-Za-z0-9](?::[0-9]{1,5})?))?";

/// The hashtag pattern: a leading start-of-text or any character that is not
/// a letter or a number (so `a#b` and `1#b` do not match, while
/// whitespace- or punctuation-led tags do), then `#` and a word of letters,
/// numbers and underscores. Whether that word holds a non-digit is a check on
/// the capture in [`normalized_tag`], not part of the pattern.
const HASHTAG_PATTERN: &str = r"(?<lead>^|[^\p{L}\p{N}])#(?<tag>[\p{L}\p{N}_]+)";

/// The longest hashtag [`extract_hashtags`] and [`linkify_entities`] produce;
/// a longer word is cut to this many characters. Also the ceiling a stored
/// hashtag name may hold (`src/models/hashtag.rs` ignores longer names).
pub(crate) const MAX_HASHTAG_LEN: usize = 100;

/// The `http://` scheme text, as bytes, for [`has_http_scheme`].
const HTTP_SCHEME: &[u8] = b"http://";

/// The `https://` scheme text, as bytes, for [`has_http_scheme`].
const HTTPS_SCHEME: &[u8] = b"https://";

/// The close tag whose end bounds an anchor region in [`linkify_entities`].
const ANCHOR_CLOSE: &str = "</a>";

/// The compiled [`MENTION_PATTERN`], or `None` if the fixed pattern ever
/// failed to compile (it cannot; degradation then means "no mentions found"
/// rather than a panic, which the lint gate forbids outside tests).
static MENTION_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(MENTION_PATTERN).ok());

/// The mention pattern of [`linkify_entities`]: [`MENTION_PATTERN`] with one
/// extra lead — the `>` that closes an HTML tag. The linkifier runs on
/// rendered HTML, where a mention that opens the post sits right behind the
/// `<p>` even though [`extract_mentions`], reading the same words as plain
/// text, finds it at the start of the text; a literal `>` can never forge
/// that lead, because [`escape_html`](crate::util::sanitize::escape_html)
/// turns every `>` the source holds into `&gt;` before the HTML renders. So
/// a `>` at a match's start is always a tag's close, granting it keeps the
/// renderer's matches the extractor's: a leading mention is linked exactly
/// when it is stored.
const MENTION_LINK_PATTERN: &str = r"(?<lead>^|[\s(\[{>])@(?<nick>[A-Za-z0-9_.-]*[A-Za-z0-9_])(?:@(?<domain>(?:[A-Za-z0-9-]+\.)+[A-Za-z0-9-]*[A-Za-z0-9](?::[0-9]{1,5})?))?";

/// The compiled [`MENTION_LINK_PATTERN`], with the same `None` fallback as
/// [`MENTION_RE`].
static MENTION_LINK_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(MENTION_LINK_PATTERN).ok());

/// The compiled [`HASHTAG_PATTERN`], with the same `None` fallback as
/// [`MENTION_RE`].
static HASHTAG_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(HASHTAG_PATTERN).ok());

/// A `@nick` or `@nick@domain.tld` reference found in text, with the case it
/// was written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionRef {
    /// The local part: the characters between the `@` and a domain `@` (or
    /// the end of the reference).
    pub nickname: String,
    /// The `domain.tld` part of a remote reference, if there was one.
    pub domain: Option<String>,
}

/// The resolver [`linkify_entities`] asks about every mention it finds:
/// `Some(profile_url)` links the mention, `None` leaves it as plain text.
pub type MentionResolver<'a> = &'a dyn Fn(&MentionRef) -> Option<String>;

impl MentionRef {
    /// The full account this reference names: `nick`, or `nick@domain` for a
    /// remote one.
    #[must_use]
    pub fn acct(&self) -> String {
        let nickname = &self.nickname;
        self.domain
            .as_ref()
            .map_or_else(|| nickname.clone(), |domain| format!("{nickname}@{domain}"))
    }
}

/// Extracts every mention from plain text.
///
/// Matches follow the `MENTION_PATTERN` rules: a mention must start at the
/// beginning of the text or after whitespace or one of `(`, `[`, `{`, so
/// email addresses (`a@b.c`) are never mentions, and a mention whose lead is
/// still inside a bare `http(s)` URL (the scheme's URL bytes run up to the
/// `@`) is ignored. Results keep their written case, are deduplicated
/// case-insensitively by account, and come back in order of first appearance.
#[must_use]
pub fn extract_mentions(text: &str) -> Vec<MentionRef> {
    let Some(re) = MENTION_RE.as_ref() else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut found: Vec<MentionRef> = Vec::new();
    for caps in re.captures_iter(text) {
        let Some(nick) = caps.name("nick") else {
            continue;
        };
        if is_url_mention(text, nick.start()) {
            continue;
        }
        let mention = MentionRef {
            nickname: nick.as_str().to_owned(),
            domain: caps.name("domain").map(|m| m.as_str().to_owned()),
        };
        if seen.insert(mention.acct().to_lowercase()) {
            found.push(mention);
        }
    }
    found
}

/// Extracts every hashtag from plain text, lowercased.
///
/// A tag follows the `HASHTAG_PATTERN` rules: `#word` of letters, digits
/// and underscores, preceded by the start of the text or a character that is
/// not a letter or digit (so neither `a#b` nor the entity `&#123;` yields a
/// tag), holding at least one non-digit and at most 100 characters (a longer
/// word is cut). Results are deduplicated and returned in order of first
/// appearance.
#[must_use]
pub fn extract_hashtags(text: &str) -> Vec<String> {
    let Some(re) = HASHTAG_RE.as_ref() else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut found: Vec<String> = Vec::new();
    for caps in re.captures_iter(text) {
        let Some(word) = caps.name("tag") else {
            continue;
        };
        let Some(tag) = normalized_tag(word.as_str()) else {
            continue;
        };
        if seen.insert(tag.clone()) {
            found.push(tag);
        }
    }
    found
}

/// The `span`/`a` markup a resolved mention becomes, as Pleroma renders
/// it.
///
/// An `h-card` span around a `u-url mention` link whose visible text is `@`
/// plus the nickname in an inner span. No `data-user` attribute is emitted
/// and the `rel` is `ugc`. `profile_url` and `nickname` are HTML-escaped
/// into the markup; `nickname` is the local part of the mention only.
#[must_use]
pub fn mention_html(profile_url: &str, nickname: &str) -> String {
    format!(
        r#"<span class="h-card"><a class="u-url mention" href="{}" rel="ugc">@<span>{}</span></a></span>"#,
        escape_html(profile_url),
        escape_html(nickname)
    )
}

/// The `a` markup a hashtag becomes, as Pleroma renders it.
///
/// A `mention hashtag` link to `{base_url}/tags/{tag}` with `rel="tag ugc"`
/// and the tag in an inner span. All three insertions are HTML-escaped (a
/// tag found by [`extract_hashtags`] can hold none of the escaped characters
/// anyway).
#[must_use]
pub fn hashtag_html(base_url: &str, tag: &str) -> String {
    format!(
        r#"<a class="mention hashtag" href="{}/tags/{}" rel="tag ugc">#<span>{}</span></a>"#,
        escape_html(base_url),
        escape_html(tag),
        escape_html(tag)
    )
}

/// Links entities in already-escaped text or HTML (anything `text_to_html`
/// emits, or bare text).
///
/// Outside `<a …>…</a>` regions — which are passed through untouched, href
/// and link text included — every hashtag is replaced with [`hashtag_html`]
/// and every mention for which `mentions` returns a profile URL is replaced
/// with [`mention_html`] naming its local part; unknown mentions stay as
/// they were written. The match rules are those of [`extract_mentions`] and
/// [`extract_hashtags`]: in-URL mentions and digits-only tags are left
/// alone, and the character that led a match stays in the text. One mention
/// rule is the linkifier's own: on HTML input a tag's close leads a match
/// the way start-of-text leads it on plain input, so the mention that opens
/// a post links after its `<p>` too.
#[must_use]
pub fn linkify_entities(
    html_or_text: &str,
    mentions: MentionResolver<'_>,
    base_url: &str,
) -> String {
    let mut out = String::with_capacity(html_or_text.len());
    let mut rest = html_or_text;
    while let Some((start, end)) = first_anchor(rest) {
        let Some((before, tail)) = rest.split_at_checked(start) else {
            break;
        };
        let Some((anchor, after)) = tail.split_at_checked(end - start) else {
            break;
        };
        out.push_str(&linkify_text(before, mentions, base_url));
        out.push_str(anchor);
        rest = after;
    }
    out.push_str(&linkify_text(rest, mentions, base_url));
    out
}

/// Replaces hashtags then mentions in one non-anchor run. Hashtags go first
/// so the `#` they emit is never in front of the text the mention pass
/// scans; neither builder's markup can hold a match for the other pattern.
fn linkify_text(text: &str, mentions: MentionResolver<'_>, base_url: &str) -> String {
    replace_mentions(&replace_hashtags(text, base_url), mentions)
}

/// Rewrites every accepted hashtag match; rejected words (digits-only) and
/// anything the pattern does not match stay as written.
fn replace_hashtags(text: &str, base_url: &str) -> String {
    let Some(re) = HASHTAG_RE.as_ref() else {
        return text.to_owned();
    };
    re.replace_all(text, |caps: &Captures<'_>| {
        let tag = caps.name("tag").and_then(|w| normalized_tag(w.as_str()));
        tag.map_or_else(
            || whole_owned(caps),
            |tag| {
                format!(
                    "{}{}",
                    caps.name("lead").map_or("", |m| m.as_str()),
                    hashtag_html(base_url, &tag)
                )
            },
        )
    })
    .into_owned()
}

/// Rewrites every mention match the callback resolves; unknown mentions and
/// in-URL candidates stay as written. The lead set is
/// [`MENTION_LINK_PATTERN`]'s — [`MENTION_PATTERN`]'s plus a tag's close —
/// so the mention that opens the rendered text links just as the mention the
/// extractor found at the head of the source does.
fn replace_mentions(text: &str, mentions: MentionResolver<'_>) -> String {
    let Some(re) = MENTION_LINK_RE.as_ref() else {
        return text.to_owned();
    };
    re.replace_all(text, |caps: &Captures<'_>| {
        let Some(nick) = caps.name("nick") else {
            return whole_owned(caps);
        };
        if is_url_mention(text, nick.start()) {
            return whole_owned(caps);
        }
        let mention = MentionRef {
            nickname: nick.as_str().to_owned(),
            domain: caps.name("domain").map(|m| m.as_str().to_owned()),
        };
        match mentions(&mention) {
            Some(url) => format!(
                "{}{}",
                caps.name("lead").map_or("", |m| m.as_str()),
                mention_html(&url, &mention.nickname)
            ),
            None => whole_owned(caps),
        }
    })
    .into_owned()
}

/// The whole match's text, owned: what the replacements return for matches
/// they do not linkify.
fn whole_owned(caps: &Captures<'_>) -> String {
    caps.get(0).map_or(String::new(), |m| m.as_str().to_owned())
}

/// Turns a matched hashtag word into the tag it yields: the first
/// [`MAX_HASHTAG_LEN`] characters, normalized by
/// [`crate::models::hashtag::normalize_name`] (lowercased, and only when
/// at least one of the characters kept is not a digit).
fn normalized_tag(word: &str) -> Option<String> {
    let tag: String = word.chars().take(MAX_HASHTAG_LEN).collect();
    normalize_name(&tag)
}

/// Whether the `@` directly before the nickname starting at `nick_start` is
/// part of a bare URL rather than a mention.
fn is_url_mention(text: &str, nick_start: usize) -> bool {
    nick_start
        .checked_sub(1)
        .is_some_and(|at| inside_url(text, at))
}

/// Whether the `@` at byte `at` continues a bare URL: walking backwards from
/// it, the run of URL bytes (the set [`crate::util::format`] linkifies with)
/// starts on an `http(s)://` scheme. Then the `@` belongs to the URL, and
/// even a lead character the mention pattern allows (an unbalanced `(` or
/// `[`, say) does not make it a mention.
fn inside_url(text: &str, at: usize) -> bool {
    let bytes = text.as_bytes();
    let mut start = at;
    while let Some(prev) = start.checked_sub(1).and_then(|i| bytes.get(i)).copied() {
        if !is_url_byte(prev) {
            break;
        }
        start -= 1;
    }
    has_http_scheme(bytes.get(start..at).unwrap_or_default())
}

/// Whether a byte run begins with the case-insensitive `http(s)://` scheme.
fn has_http_scheme(candidate: &[u8]) -> bool {
    candidate
        .get(..HTTP_SCHEME.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(HTTP_SCHEME))
        || candidate
            .get(..HTTPS_SCHEME.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(HTTPS_SCHEME))
}

/// Whether byte `b` may continue a bare URL: the same RFC 3986 set
/// [`crate::util::format`]'s scanner uses, so whatever it would linkify
/// counts as a URL here too.
const fn is_url_byte(b: u8) -> bool {
    matches!(
        b,
        b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'%'
            | b':'
            | b'/'
            | b'?'
            | b'#'
            | b'['
            | b']'
            | b'@'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
    )
}

/// The byte range of the first `<a` tag through `</a>` in `text`: an `<a`
/// whose next byte closes the tag name or separates an attribute, up to the
/// end of the first `</a>` after it (or to the end of `text` when the anchor
/// is never closed).
fn first_anchor(text: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    loop {
        let start = from + text.get(from..)?.find("<a")?;
        if !is_anchor_tag(text, start) {
            from = start + 2;
            continue;
        }
        let close = text.get(start..).and_then(|tail| {
            tail.find(ANCHOR_CLOSE)
                .map(|rel| start + rel + ANCHOR_CLOSE.len())
        });
        return Some((start, close.unwrap_or(text.len())));
    }
}

/// Whether the `<a` at byte `lt` starts an `a` tag rather than a longer tag
/// name that merely begins with the same two bytes.
fn is_anchor_tag(text: &str, lt: usize) -> bool {
    text.as_bytes()
        .get(lt + 2)
        .is_some_and(|byte| *byte == b'>' || byte.is_ascii_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `MentionRef` for comparisons.
    fn mention(nickname: &str, domain: Option<&str>) -> MentionRef {
        MentionRef {
            nickname: nickname.to_owned(),
            domain: domain.map(str::to_owned),
        }
    }

    #[test]
    fn acct_joins_nickname_and_domain() {
        assert_eq!(mention("alice", None).acct(), "alice", "local acct");
        assert_eq!(
            mention("bob", Some("example.com")).acct(),
            "bob@example.com",
            "remote acct"
        );
    }

    #[test]
    fn extracts_local_and_remote_mentions_in_order() {
        assert_eq!(
            extract_mentions("Hello @alice and @bob@example.com!"),
            vec![mention("alice", None), mention("bob", Some("example.com"))],
            "both shapes, in order, the trailing ! excluded"
        );
        assert_eq!(
            extract_mentions("@a@b.c @d"),
            vec![mention("a", Some("b.c")), mention("d", None)],
            "a minimal dotted domain, and a bare nickname"
        );
    }

    #[test]
    fn mentions_need_a_valid_lead() {
        assert!(
            extract_mentions("email a@b.c and bob@example.org").is_empty(),
            "an @ inside a word is an email address, not a mention"
        );
        assert_eq!(
            extract_mentions("(@x) [@y] {@z} @w"),
            vec![
                mention("x", None),
                mention("y", None),
                mention("z", None),
                mention("w", None)
            ],
            "start, whitespace, (, [ and {{ may all lead a mention"
        );
        assert_eq!(
            extract_mentions("quote:@bob; @carl"),
            vec![mention("carl", None)],
            "a bare : or ; may not lead one"
        );
    }

    #[test]
    fn nicknames_end_on_a_word_character() {
        assert_eq!(
            extract_mentions("hi @bob."),
            vec![mention("bob", None)],
            "the sentence period stays out"
        );
        assert_eq!(
            extract_mentions("hi @bob-smith.2 @x_y"),
            vec![mention("bob-smith.2", None), mention("x_y", None)],
            "inner . and - are nickname characters"
        );
        assert_eq!(
            extract_mentions("trailing @bob- and @a."),
            vec![mention("bob", None), mention("a", None)],
            "a forbidden tail character drops out of the nickname"
        );
    }

    #[test]
    fn a_domain_may_carry_an_explicit_port() {
        assert_eq!(
            extract_mentions("hi @remote@127.0.0.1:34567!"),
            vec![mention("remote", Some("127.0.0.1:34567"))],
            "the port belongs to the domain; the trailing ! does not"
        );
        assert_eq!(
            extract_mentions("@u@example.com:8080 and @v@host.example:x"),
            vec![
                mention("u", Some("example.com:8080")),
                mention("v", Some("host.example")),
            ],
            "digits make a port, a letter does not — `:x` stays outside"
        );
        assert_eq!(
            extract_mentions("@u@example.com:"),
            vec![mention("u", Some("example.com"))],
            "a bare trailing colon is no port"
        );
    }

    #[test]
    fn domains_need_at_least_one_dot() {
        assert_eq!(
            extract_mentions("@u@examplecom"),
            vec![mention("u", None)],
            "no dot, no domain: the mention is just @u"
        );
        assert_eq!(
            extract_mentions("@u@a..b"),
            vec![mention("u", None)],
            "an empty label is not a domain either"
        );
        assert_eq!(
            extract_mentions("@u@ex.am.ple. @v@x-1.dev"),
            vec![
                mention("u", Some("ex.am.ple")),
                mention("v", Some("x-1.dev"))
            ],
            "many dots are fine, the trailing dot falls out, hyphens sit in labels"
        );
    }

    #[test]
    fn mentions_inside_urls_are_ignored() {
        assert!(
            extract_mentions("see https://example.com/@user and http://user@example.com/x")
                .is_empty(),
            "a URL's own @ has no allowed lead"
        );
        assert!(
            extract_mentions("https://example.com/[@user]").is_empty(),
            "even after an allowed [ the URL's byte run swallows the mention"
        );
        assert_eq!(
            extract_mentions("https://example.com/ @user"),
            vec![mention("user", None)],
            "once the URL run breaks at whitespace the mention is real"
        );
    }

    #[test]
    fn mentions_keep_case_dedupe_case_insensitively_and_in_order() {
        assert_eq!(
            extract_mentions("@ALICE @bob @alice @Bob@Example.COM @bob@example.com"),
            vec![
                mention("ALICE", None),
                mention("bob", None),
                mention("Bob", Some("Example.COM")),
            ],
            "the first spelling wins, dedupe is case-insensitive on the full acct"
        );
    }

    #[test]
    fn extracts_lowercased_deduped_hashtags() {
        assert_eq!(
            extract_hashtags("a #Foo #FOO #foo #bar"),
            vec!["foo", "bar"],
            "lowercased and deduped, in order of first appearance"
        );
    }

    #[test]
    fn hashtags_accept_unicode_and_underscore() {
        assert_eq!(
            extract_hashtags("#日本語 #café #Ёлка #snake_case #a1_"),
            vec!["日本語", "café", "ёлка", "snake_case", "a1_"],
            "letters are Unicode letters, digits and _ are word characters, tags lowercase"
        );
    }

    #[test]
    fn hashtags_need_a_valid_lead() {
        assert!(
            extract_hashtags("a#b and 1#two").is_empty(),
            "a letter or a digit before the # kills the match"
        );
        assert_eq!(
            extract_hashtags("(#a) [#b] {#c} #d&#e;"),
            vec!["a", "b", "c", "d", "e"],
            "start, whitespace and any non-alphanumeric punctuation may lead"
        );
    }

    #[test]
    fn digits_only_hashtags_are_rejected() {
        assert_eq!(
            extract_hashtags("#123 #12a &#123; #١٢٣_ #0"),
            vec!["12a", "١٢٣_"],
            "all-digit words are no tags (the entity &#123; included), one non-digit is enough"
        );
    }

    #[test]
    fn hashtags_are_cut_at_a_hundred_characters() {
        let long = format!("#{}", "a".repeat(105));
        let cut = "a".repeat(100);
        assert_eq!(
            extract_hashtags(&format!("{long} #tail")),
            vec![cut.as_str(), "tail"],
            "a longer word yields its first 100 characters"
        );
        let digits = format!("#{}a", "1".repeat(101));
        assert!(
            extract_hashtags(&digits).is_empty(),
            "the 100-character cut decides the digit-only check too"
        );
    }

    #[test]
    fn mention_html_has_the_pleroma_shape() {
        assert_eq!(
            mention_html("https://example.com/users/bob", "bob"),
            r#"<span class="h-card"><a class="u-url mention" href="https://example.com/users/bob" rel="ugc">@<span>bob</span></a></span>"#,
            "no data-user attribute, and the rel is ugc"
        );
        assert_eq!(
            mention_html("https://example.com/a?x=1&y=2", "<b>"),
            r#"<span class="h-card"><a class="u-url mention" href="https://example.com/a?x=1&amp;y=2" rel="ugc">@<span>&lt;b&gt;</span></a></span>"#,
            "url and nickname are HTML-escaped in"
        );
    }

    #[test]
    fn hashtag_html_has_the_pleroma_shape() {
        assert_eq!(
            hashtag_html("https://example.com", "rust"),
            r#"<a class="mention hashtag" href="https://example.com/tags/rust" rel="tag ugc">#<span>rust</span></a>"#,
            "the tag lands in the href and in the span"
        );
    }

    #[test]
    fn linkify_replaces_known_mentions_and_all_hashtags() {
        let resolver = &|m: &MentionRef| {
            m.domain.is_none().then(|| {
                let nick = &m.nickname;
                format!("https://example.com/users/{nick}")
            })
        };
        let input = "<p>Hi @alice, not @bob@remote.example, see #Rust</p>";
        assert_eq!(
            linkify_entities(input, resolver, "https://example.com"),
            format!(
                "<p>Hi {}, not @bob@remote.example, see {}</p>",
                mention_html("https://example.com/users/alice", "alice"),
                hashtag_html("https://example.com", "rust")
            ),
            "the known mention becomes its h-card, the unknown stays text, the tag links lowercased"
        );
    }

    #[test]
    fn linkify_links_a_leading_mention_behind_the_opening_tag() {
        let resolver = &|m: &MentionRef| {
            m.domain.is_none().then(|| {
                let nick = &m.nickname;
                format!("https://example.com/users/{nick}")
            })
        };
        assert_eq!(
            linkify_entities("<p>@bob, a word</p>", resolver, "https://example.com"),
            format!(
                "<p>{}, a word</p>",
                mention_html("https://example.com/users/bob", "bob")
            ),
            "the mention that opens the post links behind its <p>, the way \
             extract_mentions already counts it at the head of the text"
        );
        assert_eq!(
            linkify_entities("<p>a&gt;@bob</p>", resolver, "https://example.com"),
            "<p>a&gt;@bob</p>",
            "an escaped greater-than is no tag close and never leads a mention"
        );
    }

    #[test]
    fn linkify_leaves_anchor_text_alone() {
        let resolver = &|m: &MentionRef| {
            m.nickname.eq("alice").then(|| {
                let nick = &m.nickname;
                format!("https://example.com/users/{nick}")
            })
        };
        let input = r#"<p>Hi @alice <a href="https://x.example/@alice">@alice #Nope</a> #Yes</p>"#;
        assert_eq!(
            linkify_entities(input, resolver, "https://x.example"),
            format!(
                r#"<p>Hi {} <a href="https://x.example/@alice">@alice #Nope</a> {}</p>"#,
                mention_html("https://example.com/users/alice", "alice"),
                hashtag_html("https://x.example", "yes")
            ),
            "text and href inside an existing anchor are never touched"
        );
    }

    #[test]
    fn linkify_respects_the_match_rules() {
        let resolver = &|_: &MentionRef| Some("https://example.com/users/x".to_owned());
        let input = "<p>a@b.c https://e.com/@y https://e.com/[@y2] (#ok) #123 &#123;</p>";
        assert_eq!(
            linkify_entities(input, resolver, "https://example.com"),
            format!(
                "<p>a@b.c https://e.com/@y https://e.com/[@y2] ({}) #123 &#123;</p>",
                hashtag_html("https://example.com", "ok")
            ),
            "emails, in-URL mentions and digit-only tags pass through unchanged"
        );
    }

    #[test]
    fn linkify_keeps_unknown_mentions_as_text() {
        let resolver = &|_: &MentionRef| None::<String>;
        assert_eq!(
            linkify_entities("hi @who and #tag", resolver, "https://e.example"),
            format!("hi @who and {}", hashtag_html("https://e.example", "tag")),
            "nothing is linkified without anchors, unknown mentions stay plain text"
        );
    }
}
