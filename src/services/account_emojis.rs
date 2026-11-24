//! The custom emojis of a page of accounts: one batch read for the page.
//!
//! An account's profile uses emojis in its display name, its note and the
//! names and values of its fields. [`AccountEmojis::load`] scans those texts
//! of every account on a page for `:shortcode:` tokens and looks them up with
//! the loader the status renders share ([`load_emojis`]): one
//! `custom_emojis` read per distinct account domain on the page (`None` for
//! the local accounts), and none at all when no text holds a shortcode. An
//! emoji counts only under the domain of the account whose text names it,
//! and a disabled one counts nowhere.
//!
//! [`AccountEmojis::fill`] then attaches the rows to the pure
//! [`Account`] render, so every endpoint that renders accounts costs the
//! same handful of queries whatever the page size.

use std::collections::HashMap;

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::render_status::{EmojiText, load_emojis};
use crate::{
    config::Config,
    entities::account::Account,
    error::AppResult,
    models::{custom_emoji::CustomEmoji as EmojiRow, user::User},
};

/// The custom emojis a page of accounts uses, keyed by account id.
#[derive(Clone, Debug, Default)]
pub struct AccountEmojis {
    /// The emoji rows each profile uses; an account using none has no entry.
    by_user: HashMap<Uuid, Vec<EmojiRow>>,
}

/// The profile texts of `user` that may hold `:shortcode:` tokens.
///
/// They are filed under the account's domain: the display name, the note,
/// and each profile field's name and value.
#[must_use]
pub fn profile_texts(user: &User) -> EmojiText<'_> {
    let mut texts = vec![user.name.as_str(), user.bio.as_str()];
    for entry in user.fields.as_array().map_or_default(Vec::as_slice) {
        for key in ["name", "value"] {
            texts.extend(entry.get(key).and_then(Value::as_str));
        }
    }
    EmojiText {
        key: user.id,
        domain: user.domain.as_deref(),
        texts,
    }
}

impl AccountEmojis {
    /// Loads the emojis `users` use, one read per distinct domain.
    ///
    /// # Errors
    ///
    /// Any database failure arrives as
    /// [`AppError::Database`](crate::error::AppError).
    pub async fn load(pool: &PgPool, users: &[&User]) -> AppResult<Self> {
        let items: Vec<EmojiText<'_>> = users.iter().map(|user| profile_texts(user)).collect();
        Ok(Self {
            by_user: load_emojis(pool, &items).await?,
        })
    }

    /// [`load`](Self::load) for a page held as owned rows.
    ///
    /// # Errors
    ///
    /// Exactly those of [`load`](Self::load).
    pub async fn load_all(pool: &PgPool, users: &[User]) -> AppResult<Self> {
        let refs: Vec<&User> = users.iter().collect();
        Self::load(pool, &refs).await
    }

    /// [`load`](Self::load) for the one account `user`.
    ///
    /// # Errors
    ///
    /// Exactly those of [`load`](Self::load).
    pub async fn load_one(pool: &PgPool, user: &User) -> AppResult<Self> {
        Self::load(pool, &[user]).await
    }

    /// The emoji rows `user`'s profile uses; empty for an account the page
    /// did not load or one using none.
    #[must_use]
    pub fn of(&self, user: &User) -> &[EmojiRow] {
        self.by_user.get(&user.id).map_or(&[], Vec::as_slice)
    }

    /// `account`, the render of `user`, with its
    /// [`emojis`](Account::emojis) filled from this page's rows.
    #[must_use]
    pub fn fill(&self, account: Account, user: &User, config: &Config) -> Account {
        account.with_emojis(self.of(user), config)
    }
}

#[cfg(test)]
mod tests;
