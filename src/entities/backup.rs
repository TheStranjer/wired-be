//! The Pleroma `Backup` entity: one account's export request as
//! `/api/v1/pleroma/backups` renders it.
//!
//! The reference shape is Pleroma's `BackupView.render("backup.json", …)`,
//! the entry `render("index.json", %{backups: backups})` maps its rows
//! through (`lib/pleroma/web/pleroma_api/views/backup_view.ex`): exactly
//! the six keys `id`, `content_type`, `url`, `file_size`, `processed` and
//! `inserted_at`. `id` is Pleroma's flake id replaced by the row's `UUIDv7`
//! as lowercase hyphenated text (decision 4), and `inserted_at` is the
//! Mastodon millisecond timestamp of decision 5
//! ([`mastodon_time`]).
//!
//! `url` is the download address of the stored archive — but that address
//! is the uploader's to know, and an entity renders from data alone
//! (decision 3). So [`Backup::from_row`] takes the URL as a parameter and
//! [`crate::api::backups`] computes it from `state.uploader` before the
//! pure render: the finished file's public URL when the row is processed
//! and names a storage key, and the empty string for a request that has no
//! archive yet — Pleroma's own answer for such a row, since its `url`
//! virtual field renders `nil` as `""` here.

use serde::Serialize;

use crate::{models::backup::Backup as BackupRow, util::time::mastodon_time};

/// The Pleroma `Backup` entity.
///
/// Build one with [`Backup::from_row`]; its [`Serialize`] impl emits
/// `id`, `content_type`, `url`, `file_size`, `processed` and `inserted_at`.
#[derive(Debug, Serialize)]
pub struct Backup {
    /// The request id as the hyphenated lowercase `UUIDv7` string
    /// (decision 4).
    pub id: String,
    /// What the archive is served as; every archive is a zip.
    pub content_type: String,
    /// The archive's download URL, or the empty string while no file is
    /// stored behind the row.
    pub url: String,
    /// The archive's size in bytes; `0` until the job has written it.
    pub file_size: i64,
    /// Whether the archive has been built and stored.
    pub processed: bool,
    /// When the request was made, in the Mastodon millisecond format
    /// (decision 5).
    pub inserted_at: String,
}

impl Backup {
    /// Renders `row` as the entity, with `url` — the download address of
    /// the stored archive, `""` when there is none yet (see the
    /// [module docs](self) for who looks that up).
    #[must_use]
    pub fn from_row(row: &BackupRow, url: &str) -> Self {
        Self {
            id: row.id.to_string(),
            content_type: row.content_type.clone(),
            url: url.to_owned(),
            file_size: row.file_size,
            processed: row.processed,
            inserted_at: mastodon_time(&row.inserted_at),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use serde_json::Value;
    use uuid::Uuid;

    use super::*;
    use crate::test_fixtures::json_keys;

    /// The backup id, as a client sees it.
    const BACKUP_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// The download URL a stored archive gets.
    const ARCHIVE_URL: &str = "https://test.example/media/backups/archive-alice.zip";

    /// Every JSON key the entity must emit, sorted, for an exact key-set
    /// check.
    const EXPECTED_KEYS: [&str; 6] = [
        "content_type",
        "file_size",
        "id",
        "inserted_at",
        "processed",
        "url",
    ];

    /// A stored request: unprocessed until `processed` says otherwise.
    fn row(processed: bool) -> BackupRow {
        BackupRow {
            id: Uuid::parse_str(BACKUP_ID).expect("the fixture id is an id"),
            user_id: Uuid::now_v7(),
            file_name: "archive-alice-20261113072901.zip".to_owned(),
            file_size: if processed { 4_096 } else { 0 },
            processed,
            content_type: "application/zip".to_owned(),
            storage_key: processed.then(|| "backups/archive-alice.zip".to_owned()),
            inserted_at: Utc
                .with_ymd_and_hms(2026, 11, 13, 7, 29, 1)
                .single()
                .expect("the fixture stamp is a moment"),
            updated_at: Utc
                .with_ymd_and_hms(2026, 11, 13, 7, 29, 1)
                .single()
                .expect("the fixture stamp is a moment"),
        }
    }

    /// `row` rendered with download URL `url`; entities always serialize.
    fn render(row: &BackupRow, url: &str) -> Value {
        serde_json::to_value(Backup::from_row(row, url)).expect("an entity serializes")
    }

    #[test]
    fn an_unprocessed_request_renders_the_six_keys_with_no_file() {
        let value = render(&row(false), "");

        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exactly the keys");
        assert_eq!(value["id"], BACKUP_ID, "the id as hyphenated text");
        assert_eq!(value["content_type"], "application/zip");
        assert_eq!(value["url"], "", "a request has no download yet");
        assert_eq!(value["file_size"], 0, "and no bytes behind it");
        assert_eq!(value["processed"], false, "the flag reads false");
        assert_eq!(
            value["inserted_at"], "2026-11-13T07:29:01.000Z",
            "the stamp in the Mastodon millisecond format"
        );
    }

    #[test]
    fn a_processed_request_carries_the_url_it_was_handed() {
        let value = render(&row(true), ARCHIVE_URL);

        assert_eq!(json_keys(&value), EXPECTED_KEYS, "the same six keys");
        assert_eq!(value["url"], ARCHIVE_URL, "the stored archive's address");
        assert_eq!(value["file_size"], 4_096, "a number, as a size");
        assert_eq!(value["processed"], true, "and the flag reads true");
    }
}
