//! The `media_attributes` arm of a status edit: new alt text and focal
//! points for the attachments the edited post wears.
//!
//! `plan` resolves the requested changes against the stored rows, before
//! the revision is recorded, so the edit knows whether media metadata alone
//! superseded a version; `write` stores them in the edit's transaction.

use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// The 422 body for an entry that names no attachment of the edited post.
///
/// Mastodon's bare changeset wording, which never says whether the id was
/// unknown, foreign or merely not on this post.
pub const UNKNOWN_ATTRIBUTE_MEDIA: &str = "Validation failed";

/// [`plan`]'s read: the stored metadata of every attachment named.
const STORED_SQL: &str = "SELECT id, description, blurhash, width, height, focus_x, focus_y
     FROM media_attachments
    WHERE id = ANY($1)";

/// [`write`]: one attachment's new metadata.
const WRITE_SQL: &str = "UPDATE media_attachments
     SET description = $2, blurhash = $3, width = $4, height = $5,
         focus_x = $6, focus_y = $7, updated_at = now()
   WHERE id = $1";

/// What an entry does to an attachment's focal point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FocusChange {
    /// Removes the focal point.
    Clear,
    /// Moves the focal point to `x`, `y`, each already inside `[-1, 1]`.
    Set {
        /// The horizontal axis.
        x: f32,
        /// The vertical axis.
        y: f32,
    },
}

/// One `media_attributes` entry, already validated by the service.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaAttributeChange {
    /// The attachment the entry edits.
    pub id: Uuid,
    /// The replacement alt text; `None` keeps the stored one.
    pub description: Option<String>,
    /// The focal point change; `None` keeps the stored one.
    pub focus: Option<FocusChange>,
}

/// What a remote edit's document states about one attachment it kept.
///
/// Unlike a [`MediaAttributeChange`], which says only what moves, this is
/// the whole state: an absent part clears the stored value, because the
/// remote's document is the truth about its own media.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaRefresh {
    /// The attachment the document restates.
    pub id: Uuid,
    /// The alt text.
    pub description: Option<String>,
    /// The blurhash placeholder.
    pub blurhash: Option<String>,
    /// The width in pixels.
    pub width: Option<i32>,
    /// The height in pixels.
    pub height: Option<i32>,
    /// The horizontal axis of the focal point.
    pub focus_x: Option<f32>,
    /// The vertical axis of the focal point.
    pub focus_y: Option<f32>,
}

/// The metadata of one attachment an edit may rewrite.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub(super) struct Metadata {
    /// The attachment.
    id: Uuid,
    /// Its alt text.
    description: Option<String>,
    /// Its blurhash placeholder.
    blurhash: Option<String>,
    /// Its width in pixels.
    width: Option<i32>,
    /// Its height in pixels.
    height: Option<i32>,
    /// The horizontal axis of its focal point.
    focus_x: Option<f32>,
    /// The vertical axis of its focal point.
    focus_y: Option<f32>,
}

impl Metadata {
    /// Replaces every part of this metadata with what `refresh` states.
    fn replace(&mut self, refresh: &MediaRefresh) {
        self.description.clone_from(&refresh.description);
        self.blurhash.clone_from(&refresh.blurhash);
        self.width = refresh.width;
        self.height = refresh.height;
        self.focus_x = refresh.focus_x;
        self.focus_y = refresh.focus_y;
    }

    /// Applies `change` over this metadata, present parts only.
    fn apply(&mut self, change: &MediaAttributeChange) {
        if let Some(text) = &change.description {
            self.description = Some(text.clone());
        }
        match change.focus {
            Some(FocusChange::Clear) => {
                self.focus_x = None;
                self.focus_y = None;
            }
            Some(FocusChange::Set { x, y }) => {
                self.focus_x = Some(x);
                self.focus_y = Some(y);
            }
            None => {}
        }
    }
}

/// The attachments whose metadata `changes` actually alter, in their new
/// state.
///
/// Every entry must name one of `allowed`, the attachments the post wears
/// once the edit lands; later entries for one id build on earlier ones. An
/// entry that restates what is stored alters nothing and is left out, so an
/// unchanged description records no revision. `refreshes` (a remote edit's
/// restated attachments) apply first, whole, then `changes` over them.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with [`UNKNOWN_ATTRIBUTE_MEDIA`] for an entry
/// outside `allowed`; database failures arrive as [`AppError::Database`].
pub(super) async fn plan(
    conn: &mut PgConnection,
    allowed: &[Uuid],
    changes: &[MediaAttributeChange],
    refreshes: &[MediaRefresh],
) -> AppResult<Vec<Metadata>> {
    if changes.is_empty() && refreshes.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = changes
        .iter()
        .map(|change| change.id)
        .chain(refreshes.iter().map(|refresh| refresh.id))
        .collect();
    if ids.iter().any(|id| !allowed.contains(id)) {
        return Err(AppError::Unprocessable(UNKNOWN_ATTRIBUTE_MEDIA.to_owned()));
    }
    let stored = sqlx::query_as::<_, Metadata>(STORED_SQL)
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;
    let mut working = stored.clone();
    for refresh in refreshes {
        row_of(&mut working, refresh.id)?.replace(refresh);
    }
    for change in changes {
        row_of(&mut working, change.id)?.apply(change);
    }
    Ok(working
        .into_iter()
        .filter(|row| !stored.contains(row))
        .collect())
}

/// The working row for `id`, or the unknown-attachment refusal.
fn row_of(rows: &mut [Metadata], id: Uuid) -> AppResult<&mut Metadata> {
    rows.iter_mut()
        .find(|row| row.id == id)
        .ok_or_else(|| AppError::Unprocessable(UNKNOWN_ATTRIBUTE_MEDIA.to_owned()))
}

/// Stores the metadata [`plan`] resolved.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`].
pub(super) async fn write(conn: &mut PgConnection, planned: &[Metadata]) -> AppResult<()> {
    for row in planned {
        sqlx::query(WRITE_SQL)
            .bind(row.id)
            .bind(row.description.as_deref())
            .bind(row.blurhash.as_deref())
            .bind(row.width)
            .bind(row.height)
            .bind(row.focus_x)
            .bind(row.focus_y)
            .execute(&mut *conn)
            .await
            .map(drop)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored row with a description and a focal point.
    fn stored() -> Metadata {
        Metadata {
            id: Uuid::now_v7(),
            description: Some("old".to_owned()),
            blurhash: Some("LEHV6nWB2yk8".to_owned()),
            width: Some(640),
            height: Some(480),
            focus_x: Some(0.5),
            focus_y: Some(-0.5),
        }
    }

    #[test]
    fn a_refresh_replaces_every_part_and_an_absent_one_clears() {
        let mut row = stored();
        let id = row.id;
        row.replace(&MediaRefresh {
            id,
            description: Some("new".to_owned()),
            blurhash: None,
            width: Some(800),
            height: None,
            focus_x: None,
            focus_y: Some(0.25),
        });
        assert_eq!(row.description.as_deref(), Some("new"), "the text replaced");
        assert_eq!(row.blurhash, None, "an unstated blurhash clears");
        assert_eq!((row.width, row.height), (Some(800), None), "dimensions");
        assert_eq!((row.focus_x, row.focus_y), (None, Some(0.25)), "the focus");
    }

    /// A change for `id` carrying only the parts given.
    fn change(
        id: Uuid,
        description: Option<&str>,
        focus: Option<FocusChange>,
    ) -> MediaAttributeChange {
        MediaAttributeChange {
            id,
            description: description.map(str::to_owned),
            focus,
        }
    }

    #[test]
    fn an_absent_part_keeps_the_stored_value() {
        let mut row = stored();
        let before = row.clone();
        row.apply(&change(row.id, None, None));
        assert_eq!(row, before, "nothing said, nothing moved");
        row.apply(&change(row.id, Some("new"), None));
        assert_eq!(row.description.as_deref(), Some("new"), "the text replaced");
        assert_eq!(row.focus_x, before.focus_x, "the focus stayed");
    }

    #[test]
    fn the_focus_sets_and_clears() {
        let mut row = stored();
        row.apply(&change(
            row.id,
            None,
            Some(FocusChange::Set { x: 1.0, y: 0.0 }),
        ));
        assert_eq!((row.focus_x, row.focus_y), (Some(1.0), Some(0.0)));
        row.apply(&change(row.id, None, Some(FocusChange::Clear)));
        assert_eq!((row.focus_x, row.focus_y), (None, None), "both axes go");
    }
}
