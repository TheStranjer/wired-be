//! The attachment and poll halves of an edit: the stored media compared and
//! rewritten, and the remote poll totals written over the mirrored ones.

use sqlx::PgPool;
use uuid::Uuid;

use super::whole;
#[cfg(doc)]
use crate::models::status_edit;
use crate::{
    error::AppResult,
    federation::{
        dispatch::FederationError,
        note_parser::{ParsedAttachment, ParsedPoll},
    },
    models::{
        poll::{self, RemoteTotals},
        status_edit::media_attributes::MediaRefresh,
    },
    services::ingest,
    state::AppState,
};

/// One stored attachment of a post: its id (what a replacement deletes after
/// the versioned write detaches it), the url [`plan_media`] compares, and the
/// metadata a refresh may rewrite.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub(super) struct StoredMedia {
    /// The row's id.
    pub(super) id: Uuid,
    /// The url the row serves.
    url: String,
    /// The alt text.
    description: Option<String>,
    /// The blurhash placeholder.
    blurhash: Option<String>,
    /// The width in pixels.
    width: Option<i32>,
    /// The height in pixels.
    height: Option<i32>,
    /// The horizontal axis of the focal point.
    focus_x: Option<f32>,
    /// The vertical axis of the focal point.
    focus_y: Option<f32>,
}

impl StoredMedia {
    /// The metadata this row holds, as the refresh that would restate it.
    fn state(&self) -> MediaRefresh {
        MediaRefresh {
            id: self.id,
            description: self.description.clone(),
            blurhash: self.blurhash.clone(),
            width: self.width,
            height: self.height,
            focus_x: self.focus_x,
            focus_y: self.focus_y,
        }
    }
}

/// What the document's attachments ask of the post's stored ones.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum MediaPlan {
    /// The same media with the same metadata: nothing moves.
    Keep,
    /// The same urls in the same order, but metadata (alt text, focal point,
    /// blurhash, dimensions) differs on these rows, which are updated in
    /// place.
    Refresh(Vec<MediaRefresh>),
    /// The count or a url differs: the stored rows go and the document's
    /// set lands fresh.
    Replace,
}

impl MediaPlan {
    /// Whether the stored rows are deleted and the document's set inserted.
    pub(super) const fn replaces(&self) -> bool {
        matches!(self, Self::Replace)
    }

    /// The in-place metadata rewrites of a [`MediaPlan::Refresh`]; empty for
    /// the other plans.
    pub(super) const fn refreshes(&self) -> &[MediaRefresh] {
        match self {
            Self::Refresh(rows) => rows.as_slice(),
            Self::Keep | Self::Replace => &[],
        }
    }

    /// Whether the plan moves any attachment.
    pub(super) const fn changes_media(&self) -> bool {
        !matches!(self, Self::Keep)
    }
}

/// The post's attachments as stored, in render order, with the metadata
/// [`plan_media`] compares.
pub(super) async fn stored_media(pool: &PgPool, status_id: Uuid) -> AppResult<Vec<StoredMedia>> {
    let rows = sqlx::query_as::<_, StoredMedia>(
        "SELECT id, url, description, blurhash, width, height, focus_x, focus_y
           FROM media_attachments
          WHERE status_id = $1 ORDER BY position, id",
    )
    .bind(status_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// How the document's attachments differ from the post's stored ones: the
/// urls compared in order, so a changed count or url replaces the set, and
/// the same urls with different metadata refresh those rows in place. An
/// unchanged restatement moves nothing — and, because
/// [`status_edit::update_status`] only ever snapshots a version it sees
/// changing, records no revision for it either.
pub(super) fn plan_media(stored: &[StoredMedia], fresh: &[ParsedAttachment]) -> MediaPlan {
    if stored.len() != fresh.len()
        || stored
            .iter()
            .zip(fresh)
            .any(|(row, media)| row.url != media.url)
    {
        return MediaPlan::Replace;
    }
    let refreshes: Vec<MediaRefresh> = stored
        .iter()
        .zip(fresh)
        .map(|(row, media)| (row.state(), restated(row.id, media)))
        .filter(|(held, wanted)| held != wanted)
        .map(|(_held, wanted)| wanted)
        .collect();
    if refreshes.is_empty() {
        MediaPlan::Keep
    } else {
        MediaPlan::Refresh(refreshes)
    }
}

/// The metadata `media` states, as a refresh of the stored row `id`.
fn restated(id: Uuid, media: &ParsedAttachment) -> MediaRefresh {
    MediaRefresh {
        id,
        description: media.description.clone(),
        blurhash: media.blurhash.clone(),
        width: media.width,
        height: media.height,
        focus_x: media.focus_x,
        focus_y: media.focus_y,
    }
}

/// The attachment rewrite beside the versioned write: the detached rows of
/// a remote post are not uploads anybody may re-attach, so they go, and the
/// document's fresh set lands through the create path's own
/// [`ingest::insert_media`] — same row shape, same remote-url semantics,
/// whichever activity brought the media.
pub(super) async fn replace_media(
    state: &AppState,
    status_id: Uuid,
    old_ids: &[Uuid],
    attachments: &[ParsedAttachment],
) -> Result<(), FederationError> {
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|err| FederationError::transient("the attachment rewrite failed", &err))?;
    sqlx::query("DELETE FROM media_attachments WHERE id = ANY($1)")
        .bind(old_ids)
        .execute(&mut *tx)
        .await
        .map(drop)
        .map_err(|err| FederationError::transient("the detached attachment delete failed", &err))?;
    ingest::insert_media(&mut tx, status_id, attachments).await?;
    tx.commit()
        .await
        .map_err(|err| FederationError::transient("the attachment rewrite commit failed", &err))?;
    Ok(())
}

/// The poll arm of [`fold_edit`]: a `Question` whose update restates its
/// poll has the remote's numbers written over the mirrored ones — options,
/// voter count, total and deadline — and a `Question` that arrived without
/// a poll grows one exactly the create path would have grown it. A document
/// carrying no poll moves nothing; the stored poll's own votes, cast here,
/// are nobody's to overwrite from there.
pub(super) async fn refresh_poll(
    state: &AppState,
    status_id: Uuid,
    question: Option<&ParsedPoll>,
) -> Result<(), FederationError> {
    let Some(poll_data) = question else {
        return Ok(());
    };
    let stored = poll::get_for_status(&state.db, status_id)
        .await
        .map_err(|err| FederationError::transient("the stored poll read failed", &err))?;
    if let Some(poll) = stored {
        let totals = remote_totals(poll_data);
        return poll::record_remote_totals(&state.db, poll.id, &totals)
            .await
            .map_err(|err| FederationError::transient("the remote poll totals failed", &err));
    }
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|err| FederationError::transient("the poll insert failed", &err))?;
    ingest::insert_poll(&mut tx, status_id, Some(poll_data)).await?;
    tx.commit()
        .await
        .map_err(|err| FederationError::transient("the poll insert commit failed", &err))?;
    Ok(())
}

/// The remote's report as [`RemoteTotals`]: each option its stated count,
/// the poll its `votersCount` and the sum of the options, the deadline as
/// the document names it (`None` keeps the stored one — see
/// [`poll::record_remote_totals`]).
fn remote_totals(poll_data: &ParsedPoll) -> RemoteTotals {
    let option_counts: Vec<(String, i32)> = poll_data
        .options
        .iter()
        .map(|option| (option.title.clone(), whole(option.votes_count)))
        .collect();
    let votes_count = option_counts.iter().fold(0_i64, |sum, (_title, votes)| {
        sum.saturating_add(i64::from(*votes))
    });
    RemoteTotals {
        options: option_counts,
        voters_count: poll_data.voters_count.map_or(0, whole),
        votes_count: whole(votes_count),
        expires_at: poll_data.expires_at,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use serde_json::json;

    use super::super::test_support::{
        app_state, author, author_again, end_time, json_parsed_poll, mirrored, note,
        question_update, revisions, view_of,
    };
    use super::super::update_note;
    use super::*;
    use crate::federation::dispatch::{ActivityView, Outcome};

    /// The url of the one image every media fixture here carries.
    const IMAGE: &str = "https://remote.example/media/alt.png";

    /// The edit stamp of every media `Update`, after the fixtures' publish time.
    const EDITED: &str = "2024-05-01T12:00:00.000Z";

    /// The fixture image at `url` as a parsed attachment with `description`
    /// and the focal point `focus`.
    fn parsed_image(url: &str, description: &str, focus: (f32, f32)) -> ParsedAttachment {
        ParsedAttachment {
            url: url.to_owned(),
            content_type: "image/png".to_owned(),
            description: Some(description.to_owned()),
            blurhash: None,
            width: None,
            height: None,
            focus_x: Some(focus.0),
            focus_y: Some(focus.1),
        }
    }

    /// The `Update` of `uri` restating its one image at `url` as `name`
    /// with the focal point `focus`; the text is the fixture's own.
    fn image_update(uri: &str, url: &str, name: &str, focus: [f32; 2]) -> ActivityView {
        view_of(
            "Update",
            note(
                uri,
                json!({
                    "updated": EDITED,
                    "attachment": [{
                        "type": "Document",
                        "mediaType": "image/png",
                        "url": url,
                        "name": name,
                        "focalPoint": focus,
                    }],
                }),
            ),
            None,
        )
    }

    /// A mirrored post at `uri` wearing `image`: its id and the image row's.
    async fn mirrored_with_image(
        pool: &PgPool,
        uri: &str,
        image: &ParsedAttachment,
    ) -> (Uuid, Uuid) {
        let author = author(pool).await;
        let row = mirrored(pool, &author, uri, "first words").await;
        let mut tx = pool.begin().await.expect("the media fixture opens");
        ingest::insert_media(&mut tx, row.id, std::slice::from_ref(image))
            .await
            .expect("the media fixture lands");
        tx.commit().await.expect("the media fixture commits");
        let media = stored_media(pool, row.id).await.expect("the media read");
        let first = media.first().expect("the fixture image is stored");
        (row.id, first.id)
    }

    #[test]
    fn plan_media_tells_keep_refresh_and_replace_apart() {
        let id = Uuid::now_v7();
        let stored = vec![StoredMedia {
            id,
            url: IMAGE.to_owned(),
            description: Some("A".to_owned()),
            blurhash: None,
            width: None,
            height: None,
            focus_x: Some(0.0),
            focus_y: Some(0.0),
        }];

        let same = parsed_image(IMAGE, "A", (0.0, 0.0));
        assert_eq!(plan_media(&stored, &[same]), MediaPlan::Keep, "restated");

        let mut reworded = parsed_image(IMAGE, "B", (0.0, 0.0));
        reworded.blurhash = Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj".to_owned());
        let MediaPlan::Refresh(rows) = plan_media(&stored, &[reworded]) else {
            panic!("the same url with new metadata refreshes");
        };
        assert_eq!(rows.len(), 1, "the one differing row");
        assert_eq!(rows[0].id, id, "keeps the stored row");
        assert_eq!(rows[0].description.as_deref(), Some("B"), "the new alt");

        let moved = parsed_image("https://remote.example/media/other.png", "A", (0.0, 0.0));
        assert_eq!(plan_media(&stored, &[moved]), MediaPlan::Replace, "url");
        assert_eq!(plan_media(&stored, &[]), MediaPlan::Replace, "count");
    }

    #[tokio::test]
    async fn an_update_with_new_alt_text_rewrites_the_row_in_place() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let uri = "https://remote.example/users/bob/statuses/alt-1";
        let (status_id, media_id) =
            mirrored_with_image(&pool, uri, &parsed_image(IMAGE, "A", (0.0, 0.0))).await;

        let outcome = update_note(
            state,
            image_update(uri, IMAGE, "B", [0.0, 0.0]),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the edit is folded");
        let media = stored_media(&pool, status_id)
            .await
            .expect("the media read");
        assert_eq!(media.len(), 1, "still one attachment");
        assert_eq!(media[0].id, media_id, "the same row, not a new one");
        assert_eq!(media[0].description.as_deref(), Some("B"), "the new alt");
        assert_eq!(revisions(&pool, status_id).await.len(), 1, "a revision");
    }

    #[tokio::test]
    async fn an_update_moving_only_the_focal_point_rewrites_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let uri = "https://remote.example/users/bob/statuses/focus-1";
        let (status_id, media_id) =
            mirrored_with_image(&pool, uri, &parsed_image(IMAGE, "A", (0.0, 0.0))).await;

        let outcome = update_note(
            state,
            image_update(uri, IMAGE, "A", [0.5, -0.5]),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the edit is folded");
        let media = stored_media(&pool, status_id)
            .await
            .expect("the media read");
        assert_eq!(media[0].id, media_id, "the same row");
        assert_eq!(media[0].description.as_deref(), Some("A"), "alt untouched");
        assert_eq!(
            (media[0].focus_x, media[0].focus_y),
            (Some(0.5), Some(-0.5)),
            "the focal point moved"
        );
    }

    #[tokio::test]
    async fn an_update_naming_a_new_url_still_replaces_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let uri = "https://remote.example/users/bob/statuses/url-1";
        let (status_id, media_id) =
            mirrored_with_image(&pool, uri, &parsed_image(IMAGE, "A", (0.0, 0.0))).await;
        let other = "https://remote.example/media/other.png";

        let outcome = update_note(
            state,
            image_update(uri, other, "A", [0.0, 0.0]),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the edit is folded");
        let media = stored_media(&pool, status_id)
            .await
            .expect("the media read");
        assert_eq!(media.len(), 1, "one attachment after the swap");
        assert_eq!(media[0].url, other, "the new url");
        assert_ne!(media[0].id, media_id, "a fresh row replaced the old");
    }

    #[tokio::test]
    async fn a_question_update_moves_the_poll_totals() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/poll-1";
        let row = mirrored(&pool, &author, uri, "tea or coffee?").await;
        let mut tx = pool.begin().await.expect("the poll fixture opens");
        ingest::insert_poll(
            &mut tx,
            row.id,
            Some(&json_parsed_poll(
                20,
                DateTime::parse_from_rfc3339("2024-06-01T00:00:00Z")
                    .expect("the deadline is an instant")
                    .with_timezone(&Utc),
            )),
        )
        .await
        .expect("the poll fixture lands");
        tx.commit().await.expect("the poll fixture commits");

        let outcome = update_note(
            state,
            view_of("Update", question_update(uri), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the totals are folded");
        let stand = poll::get_for_status(&pool, row.id)
            .await
            .expect("the poll read")
            .expect("the post still carries its poll");
        assert_eq!(stand.voters_count, 42, "the remote's voter count wins");
        assert_eq!(stand.votes_count, 45, "and its vote total");
        let options = poll::options(&pool, stand.id)
            .await
            .expect("the options read");
        assert_eq!(
            options.iter().map(|o| o.votes_count).collect::<Vec<_>>(),
            vec![30, 15],
            "option by option, matched by title"
        );
        assert_eq!(
            stand.expires_at,
            Some(end_time()),
            "and the deadline the document named overwrote the stored one"
        );
    }

    #[tokio::test]
    async fn a_question_update_grows_the_poll_it_first_carries() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/poll-late";
        let row = mirrored(&pool, &author, uri, "a plain question").await;

        let grow = update_note(state, view_of("Update", question_update(uri), None), author)
            .await
            .expect("the fold completes");
        assert_eq!(grow, Outcome::Handled, "the poll-bearing update folds");

        let stand = poll::get_for_status(&pool, row.id)
            .await
            .expect("the poll read")
            .expect("the update's poll landed");
        assert_eq!(stand.voters_count, 42, "as a mirrored create's would");
        assert_eq!(poll::options(&pool, stand.id).await.unwrap().len(), 2);
    }
}
