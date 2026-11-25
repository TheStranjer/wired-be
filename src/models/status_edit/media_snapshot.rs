//! The media half of a revision: the attachments a version wore and the
//! metadata each carried then.
//!
//! Mastodon stores `media_descriptions` per `StatusEdit` so a past version
//! renders its own alt text; a revision here snapshots the same, plus the
//! focal points, in arrays aligned element-for-element with `media_ids`.
//! A revision recorded before the snapshot columns existed holds `NULL`
//! there, and [`StatusEdit::dress`] leaves the attachment as it is stored
//! now for it.

use sqlx::PgConnection;
use uuid::Uuid;

use super::StatusEdit;
use crate::{error::AppResult, models::media_attachment::MediaAttachment};

/// [`load`]'s read: the status's attachments in render order (the same
/// `position, id` order the live render answers) with the metadata a
/// version shows.
const SNAPSHOT_SQL: &str = "SELECT id, description, focus_x, focus_y FROM media_attachments
     WHERE status_id = $1
  ORDER BY position, id";

/// One attachment row of [`SNAPSHOT_SQL`].
type SnapshotRow = (Uuid, Option<String>, Option<f32>, Option<f32>);

/// The attachments a status wears right now, as a revision stores them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct MediaSnapshot {
    /// The attachment ids, in render order.
    pub(super) ids: Vec<Uuid>,
    /// Each attachment's alt text, aligned with [`ids`](Self::ids).
    pub(super) descriptions: Vec<Option<String>>,
    /// Each attachment's focal point's horizontal axis, aligned likewise.
    pub(super) focus_x: Vec<Option<f32>>,
    /// Each attachment's focal point's vertical axis, aligned likewise.
    pub(super) focus_y: Vec<Option<f32>>,
}

impl MediaSnapshot {
    /// Splits the rows into the aligned arrays.
    fn from_rows(rows: Vec<SnapshotRow>) -> Self {
        let mut snapshot = Self::default();
        for (id, description, focus_x, focus_y) in rows {
            snapshot.ids.push(id);
            snapshot.descriptions.push(description);
            snapshot.focus_x.push(focus_x);
            snapshot.focus_y.push(focus_y);
        }
        snapshot
    }
}

/// Reads the snapshot of `status_id`'s attachments on `conn`.
///
/// # Errors
///
/// Any database failure arrives as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn load(conn: &mut PgConnection, status_id: Uuid) -> AppResult<MediaSnapshot> {
    let rows = sqlx::query_as::<_, SnapshotRow>(SNAPSHOT_SQL)
        .bind(status_id)
        .fetch_all(conn)
        .await?;
    Ok(MediaSnapshot::from_rows(rows))
}

impl StatusEdit {
    /// Restores onto `row` the metadata this version's attachment at
    /// `index` of [`media_ids`](Self::media_ids) carried.
    ///
    /// The alt text and the focal point come from the snapshot; a revision
    /// without one (recorded before the snapshot existed) leaves `row` as
    /// stored now.
    pub fn dress(&self, index: usize, row: &mut MediaAttachment) {
        if let Some(slot) = self
            .media_descriptions
            .as_ref()
            .and_then(|stored| stored.get(index))
        {
            row.description.clone_from(slot);
        }
        let focus_x = self
            .media_focus_x
            .as_ref()
            .and_then(|stored| stored.get(index));
        let focus_y = self
            .media_focus_y
            .as_ref()
            .and_then(|stored| stored.get(index));
        if let (Some(x), Some(y)) = (focus_x, focus_y) {
            row.focus_x = *x;
            row.focus_y = *y;
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    /// One axis of a focal-point snapshot, one slot per attachment.
    type Axis = Vec<Option<f32>>;

    /// A revision of `media` carrying the given snapshot columns.
    fn revision(
        descriptions: Option<Vec<Option<String>>>,
        focus: Option<(Axis, Axis)>,
    ) -> StatusEdit {
        let (media_focus_x, media_focus_y) = focus.unzip();
        StatusEdit {
            id: Uuid::nil(),
            status_id: Uuid::nil(),
            content: String::new(),
            raw_text: String::new(),
            spoiler_text: String::new(),
            sensitive: false,
            media_ids: vec![Uuid::nil()],
            created_at: Utc::now(),
            account_id: Uuid::nil(),
            media_descriptions: descriptions,
            media_focus_x,
            media_focus_y,
            poll_options: None,
        }
    }

    /// A stored attachment row wearing its current alt text and focal point.
    fn current() -> MediaAttachment {
        MediaAttachment {
            id: Uuid::nil(),
            user_id: None,
            status_id: None,
            position: 0,
            storage_key: None,
            remote_url: None,
            url: "https://test.example/media/x.png".to_owned(),
            preview_url: None,
            content_type: "image/png".to_owned(),
            description: Some("now".to_owned()),
            blurhash: None,
            width: None,
            height: None,
            preview_width: None,
            preview_height: None,
            duration: None,
            focus_x: Some(0.5),
            focus_y: Some(0.5),
            file_size: None,
            sensitive_hint: false,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn rows_split_into_aligned_arrays() {
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let snapshot = MediaSnapshot::from_rows(vec![
            (first, Some("a".to_owned()), Some(0.25), Some(-0.25)),
            (second, None, None, None),
        ]);
        assert_eq!(snapshot.ids, vec![first, second]);
        assert_eq!(snapshot.descriptions, vec![Some("a".to_owned()), None]);
        assert_eq!(snapshot.focus_x, vec![Some(0.25), None]);
        assert_eq!(snapshot.focus_y, vec![Some(-0.25), None]);
    }

    #[test]
    fn a_snapshot_restores_its_alt_text_and_focus() {
        let version = revision(
            Some(vec![Some("then".to_owned())]),
            Some((vec![Some(-1.0)], vec![None])),
        );
        let mut row = current();
        version.dress(0, &mut row);
        assert_eq!(row.description.as_deref(), Some("then"));
        assert_eq!((row.focus_x, row.focus_y), (Some(-1.0), None));
    }

    #[test]
    fn a_snapshot_without_alt_text_clears_the_current_one() {
        let version = revision(Some(vec![None]), None);
        let mut row = current();
        version.dress(0, &mut row);
        assert_eq!(row.description, None, "the version had no alt text");
        assert_eq!(
            (row.focus_x, row.focus_y),
            (Some(0.5), Some(0.5)),
            "focus kept"
        );
    }

    #[test]
    fn an_old_revision_leaves_the_stored_row_alone() {
        let version = revision(None, None);
        let mut row = current();
        version.dress(0, &mut row);
        assert_eq!(row.description.as_deref(), Some("now"));
        assert_eq!((row.focus_x, row.focus_y), (Some(0.5), Some(0.5)));
    }

    #[test]
    fn an_index_past_the_snapshot_leaves_the_row_alone() {
        let version = revision(Some(vec![Some("then".to_owned())]), None);
        let mut row = current();
        version.dress(3, &mut row);
        assert_eq!(row.description.as_deref(), Some("now"));
    }
}
