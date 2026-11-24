//! The `Emoji` entry of a `tag` array, read the one way notes and actors share.

use serde_json::Value;

use crate::{
    federation::{
        json::{as_array, first_string},
        note_parser::RemoteEmoji,
    },
    util::{emoji, url::is_http_url},
};

/// The longest emoji shortcode kept, in bytes.
///
/// The name is ASCII once it validates; Mastodon allows 2048 federated, a
/// picker has no use for that.
pub const MAX_SHORTCODE_BYTES: usize = 128;

/// Reads one `Emoji` tag as a [`RemoteEmoji`] filed under `domain`.
///
/// The emoji is the colon-stripped `name` and the first `http(s)` url under
/// `icon` (Pleroma's `fix_emoji` reads the same fields). A tag with neither
/// is skipped, and so is one whose name is no shortcode (`[A-Za-z0-9_+-]+`,
/// a superset of Pleroma's matcher) or is over [`MAX_SHORTCODE_BYTES`] long.
#[must_use]
pub fn parse_emoji_tag(entry: &Value, domain: Option<&str>) -> Option<RemoteEmoji> {
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
        domain: domain.map(str::to_owned),
    })
}
