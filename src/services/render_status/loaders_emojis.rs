//! The custom-emoji loader of a page render: one `custom_emojis` read per
//! distinct author domain on the page, keyed by the shortcodes the rendered
//! texts actually use, and none at all when no text holds a `:shortcode:`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        custom_emoji::{self, CustomEmoji as EmojiRow},
        poll::PollOption as PollOptionRow,
        status::Status as StatusRow,
        user::User,
    },
    util::emoji::shortcodes_in,
};

/// The texts one rendered entry may use emojis in, and whose emojis they are.
///
/// It holds the key the answer is filed under, the author's domain (`None`
/// for a local author, whose emojis are the local pack ones) and the texts
/// to scan, in the order their emojis should list.
#[derive(Clone, Debug)]
pub struct EmojiText<'a> {
    /// What the answer is keyed by: a status id, or a revision id.
    pub key: Uuid,
    /// The author's `users.domain`; an emoji only counts under the domain
    /// of the account that wrote the text.
    pub domain: Option<&'a str>,
    /// The texts the entry renders: content, content warning, poll options.
    pub texts: Vec<&'a str>,
}

/// The [`EmojiText`] of `row` by `author`: its content, its content
/// warning and the titles of its poll options, in that order.
#[must_use]
pub fn row_texts<'a>(
    row: &'a StatusRow,
    author: &'a User,
    options: &'a [PollOptionRow],
) -> EmojiText<'a> {
    let titles = options.iter().map(|option| option.title.as_str());
    EmojiText {
        key: row.id,
        domain: author.domain.as_deref(),
        texts: [row.content.as_str(), row.spoiler_text.as_str()]
            .into_iter()
            .chain(titles)
            .collect(),
    }
}

/// The rows of a page that render an entity, each once: the page's own, the
/// reblogged `originals` nested in them and the `quoted` originals this
/// viewer may see (`visible`) — a hidden quote renders no text, so it asks
/// for no emoji.
pub(super) fn rendered_rows<'a>(
    rows: &[&'a StatusRow],
    originals: &'a HashMap<Uuid, StatusRow>,
    quoted: &'a HashMap<Uuid, StatusRow>,
    visible: &HashSet<Uuid>,
) -> Vec<&'a StatusRow> {
    let mut seen = HashSet::new();
    rows.iter()
        .copied()
        .chain(originals.values())
        .chain(quoted.values().filter(|row| visible.contains(&row.id)))
        .filter(|row| seen.insert(row.id))
        .collect()
}

/// [`load_emojis`] over the rendered `rows`: each row's texts are its
/// content, warning and poll option titles (`poll_options`, keyed by
/// status), filed under the author's domain (`users`, keyed by id). A row
/// whose author the batch does not know renders no entity and asks for
/// nothing.
pub(super) async fn load_row_emojis(
    pool: &PgPool,
    rows: &[&StatusRow],
    users: &HashMap<Uuid, User>,
    poll_options: &HashMap<Uuid, Vec<PollOptionRow>>,
) -> AppResult<HashMap<Uuid, Vec<EmojiRow>>> {
    let items: Vec<EmojiText<'_>> = rows
        .iter()
        .filter_map(|row| {
            let author = users.get(&row.user_id)?;
            let options: &[PollOptionRow] = poll_options.get(&row.id).map_or_default(Vec::as_slice);
            Some(row_texts(row, author, options))
        })
        .collect();
    load_emojis(pool, &items).await
}

/// The page's custom emojis, keyed by [`EmojiText::key`].
///
/// Each entry lists the enabled `custom_emojis` rows its texts name under
/// its author's domain, in order of first appearance; an entry using none
/// has no key.
///
/// One [`custom_emoji::get_by_shortcodes`] read per distinct domain, asked
/// for the union of that domain's shortcodes; no read when no text holds a
/// shortcode. A same-named emoji under another domain is never used, and
/// neither is a disabled one — the moderation switch hides it everywhere.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn load_emojis(
    pool: &PgPool,
    items: &[EmojiText<'_>],
) -> AppResult<HashMap<Uuid, Vec<EmojiRow>>> {
    let mut wanted: BTreeMap<Option<&str>, BTreeSet<&str>> = BTreeMap::new();
    for item in items {
        wanted
            .entry(item.domain)
            .or_default()
            .extend(item.texts.iter().flat_map(|text| shortcodes_in(text)));
    }
    let mut stored = Vec::with_capacity(wanted.len());
    for (domain, codes) in wanted {
        if codes.is_empty() {
            continue;
        }
        let codes: Vec<String> = codes.into_iter().map(str::to_owned).collect();
        let rows = custom_emoji::get_by_shortcodes(pool, &codes, domain).await?;
        let enabled: HashMap<String, EmojiRow> = rows
            .into_iter()
            .filter(|row| !row.disabled)
            .map(|row| (row.shortcode.clone(), row))
            .collect();
        stored.push((domain, enabled));
    }
    let stored: HashMap<Option<&str>, HashMap<String, EmojiRow>> = stored.into_iter().collect();
    Ok(items
        .iter()
        .filter_map(|item| {
            let found = emojis_of(item, &stored);
            (!found.is_empty()).then_some((item.key, found))
        })
        .collect())
}

/// The rows of `stored` that `item`'s texts use, each shortcode once, in
/// order of first appearance.
fn emojis_of<'a>(
    item: &EmojiText<'a>,
    stored: &HashMap<Option<&'a str>, HashMap<String, EmojiRow>>,
) -> Vec<EmojiRow> {
    let Some(by_code) = stored.get(&item.domain) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    item.texts
        .iter()
        .flat_map(|text| shortcodes_in(text))
        .filter(|code| seen.insert(*code))
        .filter_map(|code| by_code.get(code))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    /// A pool that was never connected and cannot be: any query through it
    /// fails, so a call that succeeds asked the database nothing.
    fn unreachable_pool() -> PgPool {
        PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .expect("a lazy pool builds without connecting")
    }

    #[tokio::test]
    async fn texts_without_shortcodes_ask_the_database_nothing() {
        let pool = unreachable_pool();
        let items = [
            EmojiText {
                key: Uuid::now_v7(),
                domain: None,
                texts: vec!["<p>plain</p>", "at 12:30", ""],
            },
            EmojiText {
                key: Uuid::now_v7(),
                domain: Some("remote.example"),
                texts: Vec::new(),
            },
        ];

        let loaded = load_emojis(&pool, &items).await;

        assert!(
            loaded
                .expect("no shortcode, no query, no failure")
                .is_empty(),
            "nothing was asked and nothing was found"
        );
    }

    #[tokio::test]
    async fn no_entries_ask_the_database_nothing() {
        let loaded = load_emojis(&unreachable_pool(), &[]).await;

        assert!(
            loaded.expect("an empty page is no query").is_empty(),
            "an empty page finds nothing"
        );
    }
}
