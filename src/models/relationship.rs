//! Batch loading of the [`Relationship`] entity from the tables that answer
//! its flags.
//!
//! [`Relationship::load_many`] answers the relationship flags of one viewer
//! to many targets in a fixed, bounded set of queries: every source of flags
//! is one statement, executed once per call whatever the number of targets —
//! either a `WHERE … = ANY(…)` batch over the requested ids or, for a source
//! whose whole set is smaller than the question (the endorsements and the
//! mutes), one read of the viewer's own set, of which membership then answers
//! every target — so there is no N+1 anywhere in this module. The
//! `FlagSources` struct mirrors that contract — one field per source — and
//! `flag_sources` is the single place the set runs.
//!
//! The `follows` table (owned by [`crate::models::follow`]) supplies the
//! flags this ticket computes: `following`, `requested` and
//! `showing_reblogs` / `notifying` from the viewer's own follow rows, and
//! `followed_by` from the accepted incoming rows. The `account_notes`
//! table (owned by [`crate::models::account_note`]) supplies `note`: the
//! viewer's stored comment, the empty string where they have none. The
//! `user_domain_blocks` table (owned by [`crate::models::domain_block`])
//! supplies `domain_blocking`: true for a remote target whose host the
//! viewer has blocked, never for a local one. The `endorsements` table
//! (owned by [`crate::models::endorsement`]) supplies `endorsed`: one read
//! of the viewer's whole endorsed set, so a target pinned by the viewer
//! lights the flag. The `blocks` table (owned by
//! [`crate::models::block`]) supplies `blocking` and `blocked_by`: one
//! statement over both directions of the pair, so a target the viewer
//! blocked lights the first flag and a target who blocked the viewer the
//! second. The `mutes` table (owned by [`crate::models::mute`]) supplies
//! `muting` and `muting_notifications`: the muter's two flat sets, read
//! through the model that owns them, so a target the viewer mutes lights the
//! first flag and one whose notifications are hidden as well the second —
//! and a mute whose duration has run out is in neither set, which is how
//! every read of that table treats it. Every other flag of the entity is a
//! documented default until its own ticket lands; each one then adds exactly
//! one field to `FlagSources`, one line (one query) to `flag_sources` and one
//! flag line to `assemble`.

use sqlx::PgPool;
use uuid::Uuid;

use self::{assemble::assemble, sources::flag_sources};
use crate::{
    entities::{
        account::JsonBool,
        relationship::{Relationship, RelationshipMap},
    },
    error::{AppError, AppResult},
    models::user::User,
};

mod assemble;
#[cfg(test)]
mod block_mute_tests;
#[cfg(test)]
mod flag_tests;
#[cfg(test)]
mod follow_tests;
#[cfg(test)]
mod load_tests;
mod sources;
#[cfg(test)]
mod test_support;

/// The single inherent implementation of the entity: the constructor that
/// holds its defaults and the batch loader that computes the rest. It
/// lives here rather than beside the struct because every query belongs
/// to the models layer (decision 3), and the crate denies splitting a
/// type's inherent blocks across modules (`multiple_inherent_impl`).
impl Relationship {
    /// The relationship of a viewer with nothing recorded about
    /// `target_id`: every flag `false` — except
    /// [`showing_reblogs`](Self::showing_reblogs), which only a stored
    /// hiding can make `false` — and an empty
    /// [`note`](Self::note). [`load_many`](Self::load_many) starts from
    /// this and overwrites exactly the flags its sources computed.
    #[must_use]
    pub fn unconnected(target_id: Uuid) -> Self {
        Self {
            id: target_id.to_string(),
            following: JsonBool(false),
            requested: JsonBool(false),
            endorsed: JsonBool(false),
            followed_by: JsonBool(false),
            blocking: JsonBool(false),
            blocked_by: JsonBool(false),
            muting: JsonBool(false),
            muting_notifications: JsonBool(false),
            notifying: JsonBool(false),
            showing_reblogs: JsonBool(true),
            domain_blocking: JsonBool(false),
            note: String::new(),
            subscribing: JsonBool(false),
        }
    }

    /// Loads the relationship of `viewer_id` to each of `target_ids`, one
    /// entity per requested id, in the requested order (a repeated id is
    /// answered once per occurrence).
    ///
    /// The whole answer costs one fixed set of queries — see
    /// `flag_sources` — never a query per target. Ids that name no user
    /// are not an error: a relationship is just the (default) flags, and
    /// deciding what an unknown target means for a caller (skipping it,
    /// 404ing) is the endpoint's job, not the loader's. An empty `target_ids`
    /// answers an empty `Vec` without touching the database.
    ///
    /// # Errors
    ///
    /// Any database failure (connection, encoding) arrives as
    /// [`AppError::Database`](crate::error::AppError).
    pub async fn load_many(
        pool: &PgPool,
        viewer_id: Uuid,
        target_ids: &[Uuid],
    ) -> AppResult<Vec<Self>> {
        if target_ids.is_empty() {
            return Ok(Vec::new());
        }
        let sources = flag_sources(pool, viewer_id, target_ids).await?;
        Ok(target_ids
            .iter()
            .map(|&target_id| assemble(target_id, &sources))
            .collect())
    }

    /// Loads the relationship of `viewer_id` to the one account `target_id`:
    /// [`load_many`](Self::load_many) over a single target.
    ///
    /// # Errors
    ///
    /// Any database failure arrives as
    /// [`AppError::Database`](crate::error::AppError); the loader answers one
    /// row per requested id, so [`AppError::Internal`](crate::error::AppError)
    /// is only the guard for a loader that skipped its only target.
    pub async fn load_one(pool: &PgPool, viewer_id: Uuid, target_id: Uuid) -> AppResult<Self> {
        let loaded = Self::load_many(pool, viewer_id, &[target_id]).await?;
        loaded.into_iter().next().ok_or_else(|| {
            AppError::Internal("the relationship loader skipped its only target".to_owned())
        })
    }

    /// Loads `viewer`'s relationship to every one of `targets` as a
    /// [`RelationshipMap`] keyed by target id, the preload shape
    /// [`Account::from_user_for`](crate::entities::account::Account::from_user_for)
    /// reads. `None` for an anonymous viewer, who needs no query at all.
    ///
    /// # Errors
    ///
    /// Any database failure arrives as
    /// [`AppError::Database`](crate::error::AppError).
    pub async fn load_map(
        pool: &PgPool,
        viewer: Option<&User>,
        targets: &[Uuid],
    ) -> AppResult<Option<RelationshipMap>> {
        let Some(who) = viewer else {
            return Ok(None);
        };
        let loaded = Self::load_many(pool, who.id, targets).await?;
        Ok(Some(
            targets
                .iter()
                .copied()
                .zip(loaded)
                .collect::<RelationshipMap>(),
        ))
    }
}
