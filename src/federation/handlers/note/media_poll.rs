//! The attachment and poll halves of an edit: the stored media compared and
//! rewritten, and the remote poll totals written over the mirrored ones.

use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

#[cfg(doc)]
use crate::models::status_edit;
use crate::{
    error::AppResult,
    federation::{
        dispatch::{FederationError, TransientExt as _},
        note_parser::{ParsedAttachment, ParsedPoll},
    },
    models::{
        poll::{self, PollOption, RemoteTotals},
        status_edit::media_attributes::MediaRefresh,
    },
    services::ingest,
    state::AppState,
    util::numbers::saturating_i32,
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
        .transient("the attachment rewrite failed")?;
    sqlx::query("DELETE FROM media_attachments WHERE id = ANY($1)")
        .bind(old_ids)
        .execute(&mut *tx)
        .await
        .map(drop)
        .transient("the detached attachment delete failed")?;
    ingest::insert_media(&mut tx, status_id, attachments).await?;
    tx.commit()
        .await
        .transient("the attachment rewrite commit failed")?;
    Ok(())
}

/// The poll arm of [`fold_edit`]: a `Question` whose update restates its
/// poll has the remote's numbers written over the mirrored ones — options,
/// voter count, total and deadline — and a `Question` that arrived without
/// a poll grows one exactly the create path would have grown it. A poll
/// whose option titles or choice mode changed is replaced outright (the
/// remote reset its votes), see [`poll::replace_remote_poll`]. A document
/// carrying no poll moves nothing here; [`remove_poll`] is the caller's
/// separate answer for a full edit of a non-`Question`.
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
        .transient("the stored poll read failed")?;
    if let Some(poll) = stored {
        let totals = remote_totals(poll_data);
        let held = poll::options(&state.db, poll.id)
            .await
            .transient("the stored options read failed")?;
        if options_differ(&held, poll.multiple, poll_data) {
            return poll::replace_remote_poll(&state.db, poll.id, poll_data.multiple, &totals)
                .await
                .transient("the poll replacement failed");
        }
        return poll::record_remote_totals(&state.db, poll.id, &totals)
            .await
            .transient("the remote poll totals failed");
    }
    let mut tx = state.db.begin().await.transient("the poll insert failed")?;
    ingest::insert_poll(&mut tx, status_id, Some(poll_data)).await?;
    tx.commit()
        .await
        .transient("the poll insert commit failed")?;
    Ok(())
}

/// Deletes the stored poll of a post whose edit no longer carries one: the
/// `Update` is not a `Question` (a `Note` without `oneOf`/`anyOf`).
pub(super) async fn remove_poll(
    state: &AppState,
    status_id: Uuid,
    question: Option<&ParsedPoll>,
) -> Result<(), FederationError> {
    if question.is_some() {
        return Ok(());
    }
    let removed = poll::delete_for_status(&state.db, status_id)
        .await
        .transient("the poll removal failed")?;
    if removed {
        debug!(status = %status_id, "an Update dropped the poll; deleted it");
    }
    Ok(())
}

/// Whether the document's options differ from the stored ones — the titles
/// compared in order — or its choice mode from `multiple`.
fn options_differ(held: &[PollOption], multiple: bool, poll_data: &ParsedPoll) -> bool {
    multiple != poll_data.multiple
        || held.len() != poll_data.options.len()
        || held
            .iter()
            .zip(&poll_data.options)
            .any(|(row, option)| row.title != option.title)
}

/// The remote's report as [`RemoteTotals`]: each option its stated count,
/// the poll its `votersCount` and the sum of the options, the deadline as
/// the document names it (`None` keeps the stored one — see
/// [`poll::record_remote_totals`]).
fn remote_totals(poll_data: &ParsedPoll) -> RemoteTotals {
    let option_counts: Vec<(String, i32)> = poll_data
        .options
        .iter()
        .map(|option| (option.title.clone(), saturating_i32(option.votes_count)))
        .collect();
    let votes_count = option_counts.iter().fold(0_i64, |sum, (_title, votes)| {
        sum.saturating_add(i64::from(*votes))
    });
    RemoteTotals {
        options: option_counts,
        voters_count: poll_data.voters_count.map_or(0, saturating_i32),
        votes_count: saturating_i32(votes_count),
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
            preview_url: None,
            duration: None,
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
        let history = revisions(&pool, status_id).await;
        assert_eq!(history.len(), 1, "a revision");
        assert_eq!(history[0].media_ids, vec![media_id], "the media it wore");
        assert_eq!(
            history[0].media_descriptions,
            Some(vec![Some("A".to_owned())]),
            "the revision keeps the alt text of its own version"
        );
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
        let history = revisions(&pool, status_id).await;
        assert_eq!(history.len(), 1, "a focal-point change is a revision");
        assert_eq!(
            (&history[0].media_focus_x, &history[0].media_focus_y),
            (&Some(vec![Some(0.0)]), &Some(vec![Some(0.0)])),
            "the revision keeps the focal point of its own version"
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

    /// A mirrored poll post at `uri` holding the fixture's `Tea`/`Coffee`
    /// poll, on which `voter` has voted for `Tea`: the status id and the poll.
    async fn mirrored_poll_voted_on(pool: &PgPool, uri: &str) -> (Uuid, Uuid, Uuid) {
        let author = author(pool).await;
        let row = mirrored(pool, &author, uri, "tea or coffee?").await;
        let mut tx = pool.begin().await.expect("the poll fixture opens");
        ingest::insert_poll(&mut tx, row.id, Some(&json_parsed_poll(18, end_time())))
            .await
            .expect("the poll fixture lands");
        tx.commit().await.expect("the poll fixture commits");
        let stand = poll::get_for_status(pool, row.id)
            .await
            .expect("the poll read")
            .expect("the poll is stored");
        let voter = crate::test_fixtures::local_user(pool, "poll_voter").await;
        let held = poll::options(pool, stand.id).await.expect("the options");
        let cast = poll::cast_vote(pool, stand.id, voter.id, &[held[0].id])
            .await
            .expect("the local vote casts");
        assert!(cast, "the fixture vote lands");
        (row.id, stand.id, voter.id)
    }

    /// The `Question` update of `uri` offering `titles` (one vote apiece),
    /// as `oneOf` or, when `multiple`, `anyOf`.
    fn question_with(uri: &str, titles: &[&str], multiple: bool) -> ActivityView {
        let entries: Vec<serde_json::Value> = titles
            .iter()
            .map(|title| {
                json!({
                    "type": "Note",
                    "name": title,
                    "replies": { "type": "Collection", "totalItems": 0 },
                })
            })
            .collect();
        let key = if multiple { "anyOf" } else { "oneOf" };
        view_of(
            "Update",
            note(
                uri,
                json!({
                    "type": "Question",
                    "updated": EDITED,
                    "votersCount": 0,
                    key: entries,
                }),
            ),
            None,
        )
    }

    #[tokio::test]
    async fn new_option_titles_replace_the_poll_and_reset_local_votes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let uri = "https://remote.example/users/bob/statuses/poll-new";
        let (status_id, poll_id, voter) = mirrored_poll_voted_on(&pool, uri).await;

        let outcome = update_note(
            state,
            question_with(uri, &["new1", "new2", "new3"], false),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the edit is folded");
        let stand = poll::get_for_status(&pool, status_id)
            .await
            .expect("the poll read")
            .expect("the poll is stored");
        assert_eq!(stand.id, poll_id, "the same poll row");
        let fresh = poll::options(&pool, poll_id).await.expect("the options");
        let titles: Vec<&str> = fresh.iter().map(|o| o.title.as_str()).collect();
        assert_eq!(titles, vec!["new1", "new2", "new3"], "the new options");
        assert!(
            fresh.iter().all(|o| o.votes_count == 0),
            "the remote's zero counts"
        );
        assert_eq!(stand.votes_count, 0, "the remote's total");
        assert!(
            !poll::has_voted(&pool, poll_id, voter)
                .await
                .expect("the ballot read"),
            "the local voter's ballot is gone"
        );
        assert!(
            poll::voted_option_positions(&pool, poll_id, voter)
                .await
                .expect("the positions read")
                .is_empty(),
            "so voted renders false and own_votes is empty"
        );

        let again = poll::cast_vote(&pool, poll_id, voter, &[fresh[1].id])
            .await
            .expect("the new vote casts");
        assert!(again, "the voter can vote on the new option");
    }

    #[tokio::test]
    async fn a_choice_mode_change_alone_updates_multiple() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let uri = "https://remote.example/users/bob/statuses/poll-mode";
        let (status_id, poll_id, voter) = mirrored_poll_voted_on(&pool, uri).await;

        let outcome = update_note(
            state,
            question_with(uri, &["Tea", "Coffee"], true),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the edit is folded");
        let stand = poll::get_for_status(&pool, status_id)
            .await
            .expect("the poll read")
            .expect("the poll is stored");
        assert!(stand.multiple, "single became multiple choice");
        assert!(
            !poll::has_voted(&pool, poll_id, voter)
                .await
                .expect("the ballot read"),
            "a changed poll starts over"
        );
    }

    #[tokio::test]
    async fn the_same_titles_keep_local_votes_and_refresh_counts_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let uri = "https://remote.example/users/bob/statuses/poll-same";
        let (_status_id, poll_id, voter) = mirrored_poll_voted_on(&pool, uri).await;

        let outcome = update_note(
            state,
            view_of("Update", question_update(uri), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the edit is folded");
        assert!(
            poll::has_voted(&pool, poll_id, voter)
                .await
                .expect("the ballot read"),
            "the ballot survives a counts-only update"
        );
    }

    #[tokio::test]
    async fn an_update_without_options_removes_the_poll() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let uri = "https://remote.example/users/bob/statuses/poll-gone";
        let (status_id, poll_id, _voter) = mirrored_poll_voted_on(&pool, uri).await;

        let outcome = update_note(
            state,
            view_of(
                "Update",
                note(
                    uri,
                    json!({ "updated": EDITED, "content": "<p>plain now</p>" }),
                ),
                None,
            ),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the edit is folded");
        assert!(
            poll::get_for_status(&pool, status_id)
                .await
                .expect("the poll read")
                .is_none(),
            "the poll is deleted"
        );
        assert!(
            poll::options(&pool, poll_id)
                .await
                .expect("the options read")
                .is_empty(),
            "with its options"
        );
    }
}
