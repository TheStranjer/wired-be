//! The image and origin a delivered reaction's custom emoji is stored under.
//!
//! A reactor's `Emoji` tag is the reactor's own claim, so it never decides
//! what image an instance's emoji shows: only the signer's own emoji and
//! emoji this instance already holds are accepted, and no reaction ever
//! creates a `custom_emojis` row.

use super::Reaction;
use crate::{
    federation::dispatch::FederationError,
    models::{
        custom_emoji::{self, CustomEmoji},
        user::User,
    },
    state::AppState,
    util::emoji::shortcode as shortcode_of,
};

/// The image and origin a reaction row is written with.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct StoredEmoji {
    /// The emoji's image; `None` for a unicode reaction.
    pub(super) url: Option<String>,
    /// The instance the emoji belongs to; `None` for unicode and for an emoji of this instance.
    pub(super) domain: Option<String>,
}

/// Resolves what `reaction` is stored with; `None` refuses the reaction.
///
/// A unicode reaction carries no image. A custom emoji must claim either this
/// instance (the image is our stored one, the tag's URL is discarded, and an
/// unknown shortcode is refused) or an origin we hold a row for (that row's
/// image is used), or the signer's own domain (the tag's image is the
/// signer's word about its own emoji). Any other origin is refused.
pub(super) async fn resolve_emoji(
    state: &AppState,
    signer: &User,
    reaction: &Reaction,
) -> Result<Option<StoredEmoji>, FederationError> {
    let Some(code) = shortcode_of(&reaction.emoji) else {
        return Ok(Some(StoredEmoji {
            url: None,
            domain: None,
        }));
    };
    let Some(claimed) = reaction.origin.as_deref() else {
        return Ok(None);
    };
    if claimed.eq_ignore_ascii_case(state.config.domain()) {
        let local = stored_emoji(state, code, None)
            .await?
            .filter(|emoji| !emoji.disabled);
        return Ok(local.map(|emoji| StoredEmoji {
            url: Some(emoji.image_url),
            domain: None,
        }));
    }
    let own = signer
        .domain
        .as_deref()
        .is_some_and(|domain| domain.eq_ignore_ascii_case(claimed));
    let held = stored_emoji(state, code, Some(claimed)).await?;
    let url = match held {
        Some(emoji) => Some(emoji.image_url),
        None if own => reaction.url.clone(),
        None => None,
    };
    let Some(url) = url else {
        return Ok(None);
    };
    Ok(Some(StoredEmoji {
        url: Some(url),
        domain: Some(claimed.to_owned()),
    }))
}

/// The stored emoji `code` of `domain` (`None` = this instance), if any.
async fn stored_emoji(
    state: &AppState,
    code: &str,
    domain: Option<&str>,
) -> Result<Option<CustomEmoji>, FederationError> {
    let rows = custom_emoji::get_by_shortcodes(&state.db, &[code.to_owned()], domain)
        .await
        .map_err(|err| {
            FederationError::transient("could not look up the emoji of a reaction", &err)
        })?;
    Ok(rows.into_iter().next())
}
