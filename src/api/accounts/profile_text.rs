//! The text of the profile write: the `fields_attributes` pair
//! (`stored_fields`), the display name (`sanitized_name`) and the note
//! render (`render_note`).
//!
//! The module also holds the `after_profile_updated` hook every
//! accepted profile write runs.

use std::collections::HashMap;

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{
    config::Config,
    entities::account::local_url,
    error::{AppError, AppResult},
    extract::index_map_to_vec,
    models::user::{self, User},
    services::federation_out,
    state::AppState,
    util::{
        entities::{MentionRef, extract_mentions, linkify_entities},
        format::text_to_html,
        sanitize::{sanitize_display_name, strip_html},
    },
};

/// The longest `fields_attributes` name accepted, in characters
/// (Pleroma's `[:instance, :account_field_name_length]` default, the
/// same cap Mastodon stores); longer is the 422 in [`check_field_len`]'s
/// wording.
const MAX_PROFILE_FIELD_NAME_CHARS: usize = 255;

/// The longest `fields_attributes` value accepted, in characters
/// (Mastodon's `account_field_value_length`); longer is the 422 in
/// [`check_field_len`]'s wording.
const MAX_PROFILE_FIELD_VALUE_CHARS: usize = 2047;

/// The 422 body `PATCH /api/v1/accounts/update_credentials` answers when
/// `fields_attributes` carries more entries than
/// [`max_profile_fields`](Config::max_profile_fields); this endpoint's
/// ticket pins the wording.
const TOO_MANY_FIELDS: &str = "Too many fields";

/// The `fields` and `raw_fields` jsonb a submitted `fields_attributes`
/// stores: [`index_map_to_vec`] folds the JSON array, index-object and
/// form-spelled bodies into one list of `{name, value}` entries (a key
/// neither holds reads as the empty string, so entries that are not
/// objects are empty ones), and an entry with both name and value empty
/// is dropped — Pleroma's `put_fields` filtering, widened to the whole
/// entry. What survives must be at most `max_fields` entries (the 422
/// [`TOO_MANY_FIELDS`]) with each name within
/// [`MAX_PROFILE_FIELD_NAME_CHARS`] and each value within
/// [`MAX_PROFILE_FIELD_VALUE_CHARS`] characters, counted on the raw text
/// like Pleroma's `valid_field?`. The stored pair is the raw entry in
/// `raw_fields` and, in `fields`, the name reduced to plain text by
/// [`strip_html`] and the value rendered by [`text_to_html`] — bare URLs
/// become links carrying the standard `rel`. An empty submission is an
/// empty pair: the metadata, cleared.
///
/// # Errors
///
/// [`AppError::Unprocessable`] for the count and both length limits.
pub(crate) fn stored_fields(submitted: Value, max_fields: usize) -> AppResult<(Value, Value)> {
    let mut entries = Vec::new();
    for entry in index_map_to_vec(submitted) {
        let name = field_text(&entry, "name");
        let value = field_text(&entry, "value");
        if name.is_empty() && value.is_empty() {
            continue;
        }
        entries.push((name, value));
    }
    if entries.len() > max_fields {
        return Err(AppError::Unprocessable(TOO_MANY_FIELDS.to_owned()));
    }
    let mut rendered = Vec::with_capacity(entries.len());
    let mut raw = Vec::with_capacity(entries.len());
    for (name, value) in entries {
        check_field_len("Field name", &name, MAX_PROFILE_FIELD_NAME_CHARS)?;
        check_field_len("Field value", &value, MAX_PROFILE_FIELD_VALUE_CHARS)?;
        rendered.push(json!({
            "name": strip_html(&name),
            "value": text_to_html(&value),
        }));
        raw.push(json!({"name": name, "value": value}));
    }
    Ok((Value::Array(rendered), Value::Array(raw)))
}

/// The string one submitted `fields_attributes` entry holds under `key`:
/// a missing key or a non-string value is the empty string, which is how
/// a form that spelled only `fields_attributes[1][name]` leaves the
/// value.
fn field_text(entry: &Value, key: &str) -> String {
    entry
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The 422 mirroring the name and bio messages when one profile field's
/// `text` is longer than `max_chars` characters (counted in characters,
/// not bytes).
fn check_field_len(what: &str, text: &str, max_chars: usize) -> AppResult<()> {
    if text.chars().count() > max_chars {
        return Err(AppError::Unprocessable(format!(
            "{what} is too long (maximum is {max_chars} characters)"
        )));
    }
    Ok(())
}

/// The display name to store: at most `max_chars` characters (in
/// characters, not bytes) of the submitted text — the limit is checked
/// before sanitizing, like Pleroma's changeset checks the parameter —
/// and then [`sanitize_display_name`]'s one line of plain text. An empty
/// string survives as empty.
pub(crate) fn sanitized_name(raw: &str, max_chars: usize) -> AppResult<String> {
    if raw.chars().count() > max_chars {
        return Err(AppError::Unprocessable(format!(
            "Name is too long (maximum is {max_chars} characters)"
        )));
    }
    Ok(sanitize_display_name(raw))
}

/// Renders one submitted note into the `bio` column: [`text_to_html`]
/// (escape, bare-URL links, paragraphs) and then [`linkify_entities`]
/// over its output, so hashtags link to `{base_url}/tags/{tag}` and
/// mentions naming a local user of this instance become their `h-card`
/// anchors — unknown and remote mentions stay the plain text they were
/// written as.
pub(crate) async fn render_note(state: &AppState, raw: &str) -> AppResult<String> {
    let html = text_to_html(raw);
    let urls = local_mention_urls(&state.db, &state.config, &html).await?;
    let resolver = |mention: &MentionRef| urls.get(&mention_key(mention)).cloned();
    Ok(linkify_entities(&html, &resolver, &state.config.base_url))
}

/// The profile URLs of every mention [`extract_mentions`] finds in
/// `text` that names a local user, keyed by [`mention_key`]. Lookups are
/// citext, so `@ALICE` finds `alice`; the [`local`](User::local) flag is
/// the guard that keeps a remote mirror out of the table whatever its
/// stored nickname looks like.
async fn local_mention_urls(
    db: &PgPool,
    config: &Config,
    text: &str,
) -> AppResult<HashMap<String, String>> {
    let mut urls = HashMap::new();
    for mention in extract_mentions(text) {
        if !names_own_domain(config, &mention) {
            continue;
        }
        if let Some(found) = user::get_by_nickname(db, &mention.nickname)
            .await?
            .filter(|found| found.local.get())
        {
            let _seen = urls.insert(mention_key(&mention), local_url(config, &found.nickname));
        }
    }
    Ok(urls)
}

/// The lookup key of one mention: the account it names, lowercased —
/// `nick` for a bare one, `nick@domain` when the reference spelled the
/// domain out (the case-fold
/// [`mention_html`](crate::util::entities::mention_html) never needs
/// because it renders the written spelling).
fn mention_key(mention: &MentionRef) -> String {
    mention
        .domain
        .as_ref()
        .map_or_else(
            || mention.nickname.clone(),
            |domain| format!("{}@{}", mention.nickname, domain),
        )
        .to_lowercase()
}

/// Whether a mention can name a local user at all: a bare `@nick`, or a
/// `@nick@domain` whose domain is this instance's own
/// ([`Config::domain`], compared case-insensitively). Any other domain
/// names a remote account, which this render never links.
fn names_own_domain(config: &Config, mention: &MentionRef) -> bool {
    mention
        .domain
        .as_deref()
        .is_none_or(|domain| domain.eq_ignore_ascii_case(config.domain()))
}

/// Post-update hook of [`update_credentials`], run once per accepted
/// profile write with the updated row: hands the change to
/// [`crate::services::federation_out`], which answers it with an `Update`
/// of the actor document — the actor is [`User::ap_id`], and the queueing
/// happens through `state` (decision 11).
///
/// The service debounces a burst of edits into one queued activity, so this
/// hook is cheap to fire from every accepted write; it never fails the
/// request.
pub(crate) async fn after_profile_updated(state: &AppState, user: &User) {
    federation_out::after_profile_updated(state, user).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `MentionRef` for the key and domain helpers.
    fn mention(nickname: &str, domain: Option<&str>) -> MentionRef {
        MentionRef {
            nickname: nickname.to_owned(),
            domain: domain.map(str::to_owned),
        }
    }

    #[test]
    fn display_name_limit_is_counted_in_characters() {
        assert_eq!(
            sanitized_name("abcdé", 5).unwrap(),
            "abcdé",
            "five characters fit, bytes are not counted"
        );
        let err = sanitized_name("abcdeé", 5).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Name is too long (maximum is 5 characters)",
            "the pinned wording carries the configured limit"
        );
    }

    #[test]
    fn display_name_is_reduced_to_one_line_of_text() {
        assert_eq!(
            sanitized_name("  <b>Bob</b><br>the Builder ", 50).unwrap(),
            "Bob the Builder",
            "markup and control characters collapse to spaces"
        );
        assert_eq!(
            sanitized_name("", 50).unwrap(),
            "",
            "the empty string is stored empty, not reset to the nickname"
        );
    }

    #[test]
    fn mention_keys_fold_case_across_domains() {
        assert_eq!(mention_key(&mention("Alice", None)), "alice", "a bare nick");
        assert_eq!(
            mention_key(&mention("Bob", Some("Remote.Example"))),
            "bob@remote.example",
            "the spelled-out domain joins the key, lowercased"
        );
    }

    #[test]
    fn only_bare_or_own_domain_mentions_can_be_local() {
        let config = Config::for_tests();
        assert!(
            names_own_domain(&config, &mention("alice", None)),
            "a bare nick may name a local user"
        );
        assert!(
            names_own_domain(&config, &mention("alice", Some("TEST.example"))),
            "and so may one spelled with this instance's own domain"
        );
        assert!(
            !names_own_domain(&config, &mention("bob", Some("elsewhere.example"))),
            "another domain never names a local user"
        );
    }
}
