//! The `status_edits` table: one row per superseded version of a status.
//!
//! Pleroma keeps an edit's history inside the object, as the `state` list
//! `Pleroma.Object.Updater.make_update_object_data`
//! (`lib/pleroma/object/updater.ex`) prepends on every `Edit`; here each
//! version is a row of its own (`docs/ARCHITECTURE.md` decision 6): the
//! **current** version lives nowhere in this table — the `statuses` row is
//! the current version, and `GET /api/v1/statuses/{id}/history` renders it
//! as its last element. So a row of `status_edits` is always a revision a
//! later edit superseded: the content, source text, warning, sensitive flag
//! and media ids the post carried at the time, plus the account that wrote
//! it and the moment that version was authored
//! ([`record_revision`]'s timestamp rule).
//!
//! The write is [`update_status`]: one transaction that re-reads the live
//! row under a lock, snapshots it as a revision **only when the edit
//! actually changed one of the versioned fields** (content, warning,
//! sensitive flag or media), applies the new state and stamps
//! `edited_at`, and replaces the post's mention, hashtag and media links.
//! The resolution of *what* the new state is — merging present-only request
//! parameters over the stored row, and rendering the new text — belongs to
//! [`crate::services::create_status::edit_status`], which shares the
//! create path's checks; this module applies what it is handed.
//!
//! Deleting a status takes its history with it through the foreign key's
//! cascade; a soft delete leaves both the tombstone and its revisions, and
//! every read here hides them the way [`crate::models::status`] hides
//! tombstones.

use chrono::{DateTime, Utc};
use sqlx::{AssertSqlSafe, PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{hashtag, media_attachment::claimable_predicate, mention, status::Status},
    util::id::new_id,
};

pub mod media_attributes;

/// The live row [`update_status`] edits, locked: `FOR UPDATE` serializes two
/// concurrent edits of one status, so each transaction snapshots exactly
/// the state the previous one left.
const LIVE_FOR_UPDATE_SQL: &str = "SELECT * FROM statuses
     WHERE id = $1 AND deleted_at IS NULL
       FOR UPDATE";

/// [`record_revision`]: one snapshot row. The id is a fresh `UUIDv7`
/// (decision 4) and every value comes off the status row as it is right
/// now; `created_at` is when *that version* was authored — its last edit
/// time, or its creation before any edit — and `account_id` its author.
const RECORD_SQL: &str = "INSERT INTO status_edits (
     id, status_id, content, raw_text, spoiler_text, sensitive,
     media_ids, created_at, account_id
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9
)";

/// The media ids to snapshot with a revision: the status's attachments in
/// render order (the same `position, id` order
/// [`media_attachment::for_statuses`](crate::models::media_attachment::for_statuses)
/// answers), so the history entry names its media the way the live row
/// would have.
const MEDIA_IDS_SQL: &str = "SELECT id FROM media_attachments
     WHERE status_id = $1
  ORDER BY position, id";

/// [`list_for_status`]: one status's superseded versions, oldest authored
/// first (the `(status_id, created_at)` index serves the walk); the id
/// breaks a same-instant tie, and `UUIDv7` ids tie in mint order
/// (decision 4).
const LIST_SQL: &str = "SELECT * FROM status_edits
     WHERE status_id = $1
  ORDER BY created_at, id";

/// [`update_status`]'s write: every column the edit resolves, the `edited_at`
/// stamp moved only when a revision was recorded (an edit of nothing has
/// nothing to mark) — to the author's own time when [`EditParams::edited_at`]
/// names one, `now()` otherwise — and `RETURNING *` answers the row as it
/// now stands for the entity render.
const APPLY_SQL: &str = "UPDATE statuses
     SET content = $2,
         raw_text = $3,
         content_type = $4,
         spoiler_text = $5,
         sensitive = $6,
         language = $7,
         edited_at = CASE WHEN $8 THEN COALESCE($9::timestamptz, now()) ELSE edited_at END,
         updated_at = now()
   WHERE id = $1 AND deleted_at IS NULL
RETURNING *";

/// [`update_status`]'s media arm, step one, as SQL text: the usable part of
/// the submitted ids.
///
/// Those are the rows [`claimable_predicate`] lets the author claim for this
/// status (their own, held by no *other* status and reserved by no pending
/// schedule), deduplicated to their first position and answered in the
/// order the client sent them. Anything else (an unknown id, another
/// account's upload, a row already posted elsewhere, an upload a schedule
/// reserves) names nothing and drops out, Pleroma's
/// `CommonAPI.Utils.attachments_from_ids` rejecting the nils the same way.
fn media_plan_sql() -> String {
    format!(
        "WITH requested AS (
    SELECT id, min(ord) AS ord
      FROM unnest($2::uuid[]) WITH ORDINALITY AS t(id, ord)
     GROUP BY id
)
SELECT m.id FROM requested
     JOIN media_attachments m ON m.id = requested.id
    WHERE {}
ORDER BY requested.ord",
        claimable_predicate("$3", "$1", "NULL")
    )
}

/// [`update_status`]'s media arm, step two: the attachments the replacement
/// leaves behind are freed (their rows stay, unposted, exactly like an
/// upload that was never posted).
const MEDIA_DETACH_SQL: &str = "UPDATE media_attachments
     SET status_id = NULL, position = 0, updated_at = now()
   WHERE status_id = $1 AND NOT (id = ANY($2))";

/// [`update_status`]'s media arm, step three: the surviving ids, in the
/// order [`media_plan_sql`] resolved them, take positions from zero. The
/// ordinality is one-based; the column counts from zero.
const MEDIA_ATTACH_SQL: &str = "UPDATE media_attachments m
     SET status_id = $1, position = v.position, updated_at = now()
    FROM (
        SELECT id, (ord - 1)::int AS position
          FROM unnest($2::uuid[]) WITH ORDINALITY AS t(id, ord)
    ) v
    WHERE m.id = v.id";

/// One row of `status_edits`: one superseded version of a status.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct StatusEdit {
    /// `UUIDv7` primary key, minted by [`record_revision`] (decision 4).
    pub id: Uuid,
    /// The status this version belongs to.
    pub status_id: Uuid,
    /// The content that version carried, as sanitized HTML.
    pub content: String,
    /// The source text that version carried.
    pub raw_text: String,
    /// The content warning that version carried; empty means there was none.
    pub spoiler_text: String,
    /// Whether that version folded behind its warning.
    pub sensitive: bool,
    /// That version's attachment ids, in the order they rendered.
    pub media_ids: Vec<Uuid>,
    /// When that version was authored: the status's creation time for the
    /// first revision, the previous edit's stamp for every later one.
    pub created_at: DateTime<Utc>,
    /// The account that wrote that version (the status's author at the
    /// time of the snapshot).
    pub account_id: Uuid,
}

/// The whole new state one edit writes onto a status.
///
/// Every field carries the value the post wears after this edit — the
/// submitted one where the body named it, the stored one where it did not —
/// already resolved by [`crate::services::create_status::edit_status`].
#[derive(Clone, Debug)]
pub struct EditParams {
    /// The new content, as sanitized HTML.
    pub content: String,
    /// The new source text.
    pub raw_text: String,
    /// The `MIME` type [`raw_text`](Self::raw_text) is written in.
    pub content_type: String,
    /// The new content warning; empty for none.
    pub spoiler_text: String,
    /// Whether the new content folds behind the warning.
    pub sensitive: bool,
    /// The new language tag; `None` clears it.
    pub language: Option<String>,
    /// The accounts the new text @-mentions, in mention order:
    /// [`update_status`] replaces the post's whole link set with these
    /// (see [`crate::models::mention`]).
    pub mentioned_user_ids: Vec<Uuid>,
    /// The hashtags the new text carries (see [`crate::models::hashtag`]),
    /// likewise replacing the post's whole tag set.
    pub hashtags: Vec<String>,
    /// The media replacement: `None` leaves the post's attachments exactly
    /// as they are; `Some(ids)` names the whole new set, in order —
    /// [`update_status`] filters it down to the ids the author may attach
    /// (their own, unposted or already on this post), puts those on the
    /// post in that order, and frees the attachments it leaves out.
    pub media_ids: Option<Vec<Uuid>>,
    /// The moment the author made this edit, for an edit a remote author
    /// made: `None` stamps the local receipt time (`now()`), which is what a
    /// local edit is; `Some(time)` stamps the author's clock instead, so a
    /// later remote edit compares against the same clock.
    pub edited_at: Option<DateTime<Utc>>,
    /// New alt text and focal points for attachments the post wears once
    /// the edit lands; an entry naming any other attachment fails the
    /// whole edit with [`media_attributes::UNKNOWN_ATTRIBUTE_MEDIA`], and one that alters
    /// something counts as a change to the version.
    pub media_attributes: Vec<media_attributes::MediaAttributeChange>,
    /// What a remote edit's document restates about attachments it kept:
    /// each entry replaces the whole stored metadata (alt text, blurhash,
    /// dimensions, focal point) of an attachment the post wears once the
    /// edit lands, and one that alters something counts as a change to the
    /// version, like [`media_attributes`](Self::media_attributes). Local
    /// edits leave it empty.
    pub media_refresh: Vec<media_attributes::MediaRefresh>,
}

/// Snapshots the current state of `status` as a revision row on `conn`.
///
/// The caller opens the transaction (in this crate, always
/// [`update_status`], which holds a lock on the row), so the snapshot and
/// the write that supersedes it commit together. The revision's
/// `created_at` is when the snapshotted version was authored — the row's
/// `edited_at` if it had been edited before, its `inserted_at` until. The
/// media ids come off `media_attachments` on the same connection: the
/// attachments the post carries right now, in render order.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn record_revision(conn: &mut PgConnection, status: &Status) -> AppResult<()> {
    let media_ids = media_ids_of(conn, status.id).await?;
    sqlx::query(RECORD_SQL)
        .bind(new_id())
        .bind(status.id)
        .bind(&status.content)
        .bind(&status.raw_text)
        .bind(&status.spoiler_text)
        .bind(status.sensitive)
        .bind(&media_ids)
        .bind(status.edited_at.unwrap_or(status.inserted_at))
        .bind(status.user_id)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

/// The superseded versions of `status_id`, oldest authored first.
///
/// This is the order `GET /api/v1/statuses/{id}/history` answers. The
/// current version is not one of them — the live [`Status`] row is — and a
/// status nobody edited answers the empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn list_for_status(pool: &PgPool, status_id: Uuid) -> AppResult<Vec<StatusEdit>> {
    Ok(sqlx::query_as::<_, StatusEdit>(LIST_SQL)
        .bind(status_id)
        .fetch_all(pool)
        .await?)
}

/// Applies `params` to status `status_id` in one transaction.
///
/// The state the post wore before this write is [`record_revision`]'d —
/// only when the edit actually changed a versioned field (content, source
/// text, warning, sensitive flag or media), since a revision of an
/// unchanged post would put the same version in the history twice. The new
/// state is then written with `edited_at = now()` beside a recorded
/// revision, and the post's mention, hashtag and media links are replaced
/// with what `params` names.
///
/// An edit that changed none of the versioned fields still writes the row
/// (a `language` or `content_type`-only edit is a real change the entity
/// must carry), it just records no revision and moves no `edited_at` stamp.
/// A live row the whole transaction cannot find — deleted between the
/// endpoint's gate and this lock — is the `None` answer, and nothing is
/// written for it.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); the rollback leaves the
/// status, its revisions and its links exactly as they were, so a status
/// and the revision that shadows it are written together or not at all.
pub async fn update_status(
    pool: &PgPool,
    status_id: Uuid,
    params: EditParams,
) -> AppResult<Option<Status>> {
    let mut tx = pool.begin().await?;
    let Some(row) = live_row(&mut tx, status_id).await? else {
        return Ok(None);
    };
    let current_media = media_ids_of(&mut tx, status_id).await?;
    let new_media = match &params.media_ids {
        Some(submitted) => Some(resolve_media(&mut tx, status_id, row.user_id, submitted).await?),
        None => None,
    };
    let final_media = new_media.as_deref().unwrap_or(&current_media);
    let attributes = media_attributes::plan(
        &mut tx,
        final_media,
        &params.media_attributes,
        &params.media_refresh,
    )
    .await?;
    let changed = edit_changes_a_version(
        &row,
        &params,
        new_media.as_ref(),
        &current_media,
        !attributes.is_empty(),
    );
    if changed {
        record_revision(&mut tx, &row).await?;
    }
    let Some(updated) = apply_edit(&mut tx, status_id, &params, changed).await? else {
        return Ok(None);
    };
    mention::replace_conn(&mut tx, status_id, &params.mentioned_user_ids).await?;
    hashtag::replace_names_conn(&mut tx, status_id, &params.hashtags).await?;
    if let Some(kept) = &new_media {
        replace_media(&mut tx, status_id, kept).await?;
    }
    media_attributes::write(&mut tx, &attributes).await?;
    tx.commit().await?;
    Ok(Some(updated))
}

/// Whether this edit supersedes a version and owes a revision row: one of
/// the fields a [`StatusEdit`] stores — content, source text, warning,
/// sensitive flag, media set — actually differs from what the post carries
/// now. `new_media` is `None` when the edit says nothing about media, and
/// the current set is [`media_ids_of`]'s answer for the post as it stands.
/// `attributes_changed` is whether a `media_attributes` entry alters the
/// description or focal point of an attachment: alt text is part of what a
/// version shows.
fn edit_changes_a_version(
    row: &Status,
    params: &EditParams,
    new_media: Option<&Vec<Uuid>>,
    current_media: &[Uuid],
    attributes_changed: bool,
) -> bool {
    let media_changed =
        attributes_changed || new_media.is_some_and(|kept| kept.as_slice() != current_media);
    row.content != params.content
        || row.raw_text != params.raw_text
        || row.spoiler_text != params.spoiler_text
        || row.sensitive != params.sensitive
        || media_changed
}

/// Reads the live row `status_id` under the transaction's row lock (see
/// [`LIVE_FOR_UPDATE_SQL`]); `None` for a tombstone or a gone row.
async fn live_row(conn: &mut PgConnection, status_id: Uuid) -> AppResult<Option<Status>> {
    Ok(sqlx::query_as::<_, Status>(LIVE_FOR_UPDATE_SQL)
        .bind(status_id)
        .fetch_optional(&mut *conn)
        .await?)
}

/// The attachment ids `status_id` carries right now, in render order — the
/// snapshot's media and the replacement's `from`.
async fn media_ids_of(conn: &mut PgConnection, status_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query_as::<_, (Uuid,)>(MEDIA_IDS_SQL)
        .bind(status_id)
        .fetch_all(conn)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Filters `submitted` down to the ids this edit may attach to `status_id`
/// (see [`media_plan_sql`]), in the order the client sent them. The
/// surviving list is both the new media set and what the post wears once
/// [`replace_media`] has run.
async fn resolve_media(
    conn: &mut PgConnection,
    status_id: Uuid,
    user_id: Uuid,
    submitted: &[Uuid],
) -> AppResult<Vec<Uuid>> {
    // The statement is assembled from constants only; every value is bound.
    let rows = sqlx::query_as::<_, (Uuid,)>(AssertSqlSafe(media_plan_sql()))
        .bind(status_id)
        .bind(submitted.to_vec())
        .bind(user_id)
        .fetch_all(conn)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Writes the resolved state onto the status (see [`APPLY_SQL`]), answering
/// the row as it now stands; `None` only for a row the lock found live a
/// statement ago and that vanished since — unreachable under this
/// transaction's lock, and the endpoint's 404 if it ever happens.
async fn apply_edit(
    conn: &mut PgConnection,
    status_id: Uuid,
    params: &EditParams,
    changed: bool,
) -> AppResult<Option<Status>> {
    Ok(sqlx::query_as::<_, Status>(APPLY_SQL)
        .bind(status_id)
        .bind(&params.content)
        .bind(&params.raw_text)
        .bind(&params.content_type)
        .bind(&params.spoiler_text)
        .bind(params.sensitive)
        .bind(params.language.as_deref())
        .bind(changed)
        .bind(params.edited_at)
        .fetch_optional(&mut *conn)
        .await?)
}

/// Makes `kept` the post's whole attachment set: every attachment of
/// `status_id` outside the list is freed, and the list takes positions from
/// zero in order (see [`MEDIA_DETACH_SQL`] and [`MEDIA_ATTACH_SQL`]).
async fn replace_media(conn: &mut PgConnection, status_id: Uuid, kept: &[Uuid]) -> AppResult<()> {
    let kept = kept.to_vec();
    sqlx::query(MEDIA_DETACH_SQL)
        .bind(status_id)
        .bind(&kept)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    sqlx::query(MEDIA_ATTACH_SQL)
        .bind(status_id)
        .bind(kept)
        .execute(&mut *conn)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            media_attachment::{self, NewMediaAttachment},
            status::{self, NewStatus, Visibility},
        },
        test_fixtures::local_user as user,
    };

    /// The instance origin the fixtures post under.
    const BASE: &str = TEST_BASE_URL;

    /// `author`'s live local post of `text`, public, with no entities.
    async fn post(pool: &PgPool, author: &crate::models::user::User, text: &str) -> Status {
        status::insert(pool, BASE, &NewStatus::local(author, text))
            .await
            .expect("the fixture post inserts")
    }

    /// An unposted upload by `author`: the shape `media_ids` on an edit names.
    async fn upload(pool: &PgPool, author: &crate::models::user::User) -> Uuid {
        media_attachment::insert(
            pool,
            &NewMediaAttachment {
                user_id: Some(author.id),
                status_id: None,
                url: format!("{BASE}/media/fixture.png"),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("the fixture upload inserts")
        .id
    }

    /// Upload `id` attached to `status` at `position` — a row the post
    /// already carries.
    async fn attach(
        pool: &PgPool,
        author: &crate::models::user::User,
        status: &Status,
        position: i32,
    ) -> Uuid {
        media_attachment::insert(
            pool,
            &NewMediaAttachment {
                user_id: Some(author.id),
                status_id: Some(status.id),
                position,
                url: format!("{BASE}/media/fixture.png"),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("the fixture attachment inserts")
        .id
    }

    /// The stored attachment of `status_id`, in render order, for an
    /// expectation about what the post wears now.
    async fn stored_media(pool: &PgPool, status_id: Uuid) -> Vec<Uuid> {
        let rows = sqlx::query_as::<_, (Uuid,)>(
            "SELECT id FROM media_attachments WHERE status_id = $1 ORDER BY position, id",
        )
        .bind(status_id)
        .fetch_all(pool)
        .await
        .expect("the attachment rows read");
        rows.into_iter().map(|(id,)| id).collect()
    }

    /// Everything new: the plain edit of `text`, touching nothing else.
    fn edit_to(text: &str, content: &str) -> EditParams {
        EditParams {
            content: content.to_owned(),
            raw_text: text.to_owned(),
            content_type: status::DEFAULT_CONTENT_TYPE.to_owned(),
            spoiler_text: String::new(),
            sensitive: false,
            language: None,
            mentioned_user_ids: Vec::new(),
            hashtags: Vec::new(),
            media_ids: None,
            edited_at: None,
            media_attributes: Vec::new(),
            media_refresh: Vec::new(),
        }
    }

    /// The stored revision rows of `status_id`, oldest first, straight from
    /// the table.
    async fn revisions(pool: &PgPool, status_id: Uuid) -> Vec<StatusEdit> {
        list_for_status(pool, status_id)
            .await
            .expect("the revisions read")
    }

    #[tokio::test]
    async fn an_edit_snapshots_the_previous_version_and_stamps_the_edit() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "editor").await;
        let row = post(&pool, &alice, "first words").await;

        let updated = update_status(
            &pool,
            row.id,
            edit_to("second words", "<p>second words</p>"),
        )
        .await
        .expect("the edit writes")
        .expect("the status is live");

        assert_eq!(
            updated.raw_text, "second words",
            "the row carries the new text"
        );
        assert_eq!(updated.content, "<p>second words</p>");
        assert!(
            updated.edited_at.is_some(),
            "a recorded revision stamps `edited_at`"
        );

        let history = revisions(&pool, row.id).await;
        assert_eq!(history.len(), 1, "the superseded version is one row");
        let revision = &history[0];
        assert_eq!(revision.status_id, row.id);
        assert_eq!(revision.content, row.content, "the snapshot is the old row");
        assert_eq!(revision.raw_text, "first words");
        assert_eq!(revision.account_id, alice.id, "and names its author");
        assert_eq!(
            revision.created_at, row.inserted_at,
            "the first version was authored when the post was"
        );
        assert!(revision.media_ids.is_empty(), "it carried no media");
    }

    #[tokio::test]
    async fn a_local_edit_stamps_the_receipt_time() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "local-stamp").await;
        let row = post(&pool, &alice, "first words").await;
        let before = Utc::now() - chrono::Duration::seconds(5);

        let updated = update_status(&pool, row.id, edit_to("second", "<p>second</p>"))
            .await
            .expect("the edit writes")
            .expect("the status is live");

        let stamped = updated.edited_at.expect("the edit is stamped");
        assert!(
            stamped >= before && stamped <= Utc::now() + chrono::Duration::seconds(5),
            "a local edit is stamped with the clock of this server"
        );
    }

    #[tokio::test]
    async fn an_edit_naming_the_authors_time_stamps_that_time() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "remote-stamp").await;
        let row = post(&pool, &alice, "first words").await;
        let authored = Utc::now() - chrono::Duration::hours(3);

        let mut params = edit_to("second", "<p>second</p>");
        params.edited_at = Some(authored);
        let updated = update_status(&pool, row.id, params)
            .await
            .expect("the edit writes")
            .expect("the status is live");

        assert_eq!(
            updated.edited_at.map(|at| at.timestamp_micros()),
            Some(authored.timestamp_micros()),
            "the row wears the author's moment, not the receipt time"
        );
    }

    #[tokio::test]
    async fn successive_edits_order_their_revisions_oldest_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "twice-edited").await;
        let row = post(&pool, &alice, "v1").await;

        let after_one = update_status(&pool, row.id, edit_to("v2", "<p>v2</p>"))
            .await
            .expect("the first edit writes")
            .expect("the status is live");
        let after_two = update_status(&pool, row.id, edit_to("v3", "<p>v3</p>"))
            .await
            .expect("the second edit writes")
            .expect("the status is live");

        let history = revisions(&pool, row.id).await;
        assert_eq!(history.len(), 2, "two superseded versions");
        assert_eq!(history[0].raw_text, "v1", "the original comes first");
        assert_eq!(history[1].raw_text, "v2", "then the first edit's version");
        assert!(
            history[0].created_at <= history[1].created_at,
            "authored times walk forward"
        );
        assert_eq!(
            history[1].created_at,
            after_one.edited_at.expect("the first edit stamped"),
            "the second snapshot names the first edit's time as its authoring"
        );
        assert!(
            after_two.edited_at.expect("the second edit stamped")
                >= after_one.edited_at.expect("the first edit stamped"),
            "and the live row carries the latest stamp"
        );
    }

    #[tokio::test]
    async fn an_edit_of_nothing_records_no_revision_and_marks_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "restater").await;
        let row = post(&pool, &alice, "same words").await;

        let updated = update_status(&pool, row.id, edit_to("same words", &row.content))
            .await
            .expect("the restatement writes")
            .expect("the status is live");

        assert!(
            updated.edited_at.is_none(),
            "nothing superseded, so nothing is stamped"
        );
        assert!(
            revisions(&pool, row.id).await.is_empty(),
            "and no revision row was written"
        );
    }

    #[tokio::test]
    async fn a_language_only_edit_writes_through_without_a_revision() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "tagged").await;
        let row = post(&pool, &alice, "bonjour").await;

        let mut params = edit_to("bonjour", &row.content);
        params.language = Some("fr".to_owned());
        let updated = update_status(&pool, row.id, params)
            .await
            .expect("the edit writes")
            .expect("the status is live");

        assert_eq!(
            updated.language.as_deref(),
            Some("fr"),
            "the tag is written"
        );
        assert!(
            updated.edited_at.is_none(),
            "a tag is no version, so nothing is recorded or stamped"
        );
        assert!(
            revisions(&pool, row.id).await.is_empty(),
            "a tag is no version, so nothing is recorded"
        );
    }

    #[tokio::test]
    async fn editing_media_replaces_the_posts_whole_attachment_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "media-editor").await;
        let row = post(&pool, &alice, "with pictures").await;
        let old = attach(&pool, &alice, &row, 0).await;
        let kept = upload(&pool, &alice).await;
        let newcomer = upload(&pool, &alice).await;

        let mut params = edit_to("with pictures", &row.content);
        params.media_ids = Some(vec![newcomer, kept, old]);
        let edited = update_status(&pool, row.id, params)
            .await
            .expect("the media edit writes");
        assert!(edited.is_some(), "the live post took the edit");

        assert_eq!(
            stored_media(&pool, row.id).await,
            vec![newcomer, kept, old],
            "the set is the submitted one, in the submitted order, and a \
             row already on the post keeps its place when asked for again"
        );

        let history = revisions(&pool, row.id).await;
        assert_eq!(
            history.len(),
            1,
            "media changed, so a version was superseded"
        );
        assert_eq!(
            history[0].media_ids,
            vec![old],
            "and the snapshot names the attachments the post wore before"
        );
    }

    #[tokio::test]
    async fn media_the_author_may_not_attach_drops_out_of_the_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "picker").await;
        let bob = user(&pool, "other-picker").await;
        let row = post(&pool, &alice, "mine only").await;
        let mine = upload(&pool, &alice).await;
        let theirs = upload(&pool, &bob).await;
        let elsewhere = post(&pool, &alice, "another post").await;
        let posted_elsewhere = attach(&pool, &alice, &elsewhere, 0).await;

        let mut params = edit_to("mine only", &row.content);
        params.media_ids = Some(vec![mine, theirs, posted_elsewhere, Uuid::now_v7()]);
        let edited = update_status(&pool, row.id, params)
            .await
            .expect("the media edit writes");
        assert!(edited.is_some(), "the live post took the edit");

        assert_eq!(
            stored_media(&pool, row.id).await,
            vec![mine],
            "a foreign upload, one another post claims and an unknown id \
             all name nothing"
        );
    }

    #[tokio::test]
    async fn clearing_media_frees_every_attachment_of_the_post() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "unposter").await;
        let row = post(&pool, &alice, "text only after all").await;
        let one = attach(&pool, &alice, &row, 0).await;
        let two = attach(&pool, &alice, &row, 1).await;

        let mut params = edit_to("text only after all", &row.content);
        params.media_ids = Some(Vec::new());
        let edited = update_status(&pool, row.id, params)
            .await
            .expect("the media edit writes");
        assert!(edited.is_some(), "the live post took the edit");

        assert!(
            stored_media(&pool, row.id).await.is_empty(),
            "the post wears nothing"
        );
        let freed: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
            "SELECT id FROM media_attachments WHERE status_id IS NULL ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("the unposted rows read")
        .into_iter()
        .map(|(id,)| id)
        .collect();
        assert!(
            freed.contains(&one) && freed.contains(&two),
            "and the freed rows stay in the table, unposted"
        );
    }

    #[tokio::test]
    async fn an_edit_replaces_the_posts_mentions_and_hashtags() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "relabeler").await;
        let bob = user(&pool, "mentioned-bob").await;
        let row = status::insert(
            &pool,
            BASE,
            &NewStatus {
                mentioned_user_ids: vec![bob.id],
                hashtags: vec!["old".to_owned()],
                ..NewStatus::local(&alice, "hi @bob #old")
            },
        )
        .await
        .expect("the fixture post inserts");

        let mut params = edit_to("bye #new", "<p>bye #new</p>");
        params.hashtags = vec!["new".to_owned()];
        let edited = update_status(&pool, row.id, params)
            .await
            .expect("the edit writes");
        assert!(edited.is_some(), "the live post took the edit");

        let linked = sqlx::query_as::<_, (Uuid,)>(
            "SELECT user_id FROM status_mentions WHERE status_id = $1",
        )
        .bind(row.id)
        .fetch_optional(&pool)
        .await
        .expect("the mention links read");
        assert!(linked.is_none(), "the dropped mention is unlinked");

        let tagged: Vec<String> = sqlx::query_as::<_, (String,)>(
            "SELECT h.name FROM status_hashtags sh JOIN hashtags h ON h.id = sh.hashtag_id \
              WHERE sh.status_id = $1",
        )
        .bind(row.id)
        .fetch_all(&pool)
        .await
        .expect("the tag links read")
        .into_iter()
        .map(|(name,)| name)
        .collect();
        assert_eq!(
            tagged,
            vec!["new"],
            "the old tag is gone and the new one is in"
        );
    }

    #[tokio::test]
    async fn a_gone_or_tombstoned_status_is_edited_by_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "ghost-writer").await;
        let row = post(&pool, &alice, "doomed").await;
        let tombstone = status::soft_delete(&pool, row.id)
            .await
            .expect("the fixture delete runs")
            .expect("the fixture status was live");
        assert!(
            tombstone.deleted_at.is_some(),
            "the fixture row is a tombstone now"
        );

        assert!(
            update_status(&pool, row.id, edit_to("changed", "<p>changed</p>"))
                .await
                .expect("the write runs")
                .is_none(),
            "a tombstone is not editable"
        );
        assert!(
            update_status(&pool, Uuid::now_v7(), edit_to("changed", "<p>changed</p>"))
                .await
                .expect("the write runs")
                .is_none(),
            "and neither is an unknown id"
        );
    }

    #[tokio::test]
    async fn record_revision_snapshots_the_row_it_is_handed() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "single-snapshot").await;
        let row = status::insert(
            &pool,
            BASE,
            &NewStatus {
                spoiler_text: "careful".to_owned(),
                sensitive: true,
                visibility: Visibility::Unlisted,
                ..NewStatus::local(&alice, "raw body")
            },
        )
        .await
        .expect("the fixture post inserts");
        let picture = attach(&pool, &alice, &row, 0).await;
        let mut tx = pool.begin().await.expect("the transaction opens");

        record_revision(&mut tx, &row)
            .await
            .expect("the revision records");
        tx.commit().await.expect("the revision commits");

        let history = revisions(&pool, row.id).await;
        assert_eq!(history.len(), 1);
        let revision = &history[0];
        assert_eq!(revision.content, row.content);
        assert_eq!(revision.raw_text, "raw body");
        assert_eq!(
            revision.spoiler_text, "careful",
            "warning and flag snapshot too"
        );
        assert!(revision.sensitive);
        assert_eq!(revision.media_ids, vec![picture], "with the media it wore");
    }
}
