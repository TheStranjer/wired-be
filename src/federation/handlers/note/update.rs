//! The `Update` fold: the authorship gate, the stale check and the edit of
//! the stored post, or the ingest of one this instance never stored.

use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use super::media_poll::{media_replacement_needed, refresh_poll, replace_media, stored_media};
#[cfg(doc)]
use super::update_note;
use crate::{
    api::statuses::after_status_edited,
    federation::{
        dispatch::{ActivityView, FederationError, Outcome},
        handlers::create,
        json::clamped_time,
        note_parser::{self, ParsedNote},
    },
    models::{
        status::{self, Status},
        status_edit::{self, EditParams},
        user::User,
    },
    services::ingest,
    state::AppState,
};

/// The body of [`update_note`]: the authorship gate, then the edit of the
/// stored post or the ingest of an unstored one.
pub(super) async fn apply_update(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(object) = view.object.embedded() else {
        debug!(
            object_id = view.object.id().unwrap_or_default(),
            "an Update whose object is only a reference; this instance does not fetch it yet"
        );
        return Ok(Outcome::Ignored("object not embedded"));
    };
    if !create::names_actor(object, &view.actor) {
        debug!(
            actor = %view.actor,
            "an Update whose object is attributed to someone else; discarding"
        );
        return Ok(Outcome::Ignored("attributedTo is not the activity actor"));
    }
    let Some(uri) = view.object.id() else {
        return Ok(Outcome::Ignored("an Update whose object names no id"));
    };
    let stored = status::get_by_uri(&state.db, uri)
        .await
        .map_err(|err| FederationError::transient("the status uri read failed", &err))?;
    match stored.filter(|row| row.user_id == signer.id && row.deleted_at.is_none()) {
        Some(row) => fold_edit(&state, object, &signer, &row).await,
        None => ingest_the_rest(&state, &signer, object).await,
    }
}

/// What an `Update` of an object this instance does not hold live (or does
/// not hold at all) becomes: a `Create`'s fold of the same document. The
/// ingest owns the answers — the tombstone's refusal, a row another account
/// authored restated as `Existing`, an unsolicited or deactivated one
/// ignored — and they ride out of this handler unchanged.
async fn ingest_the_rest(
    state: &AppState,
    signer: &User,
    object: &Value,
) -> Result<Outcome, FederationError> {
    match ingest::ingest_note(state, signer, object).await? {
        ingest::IngestOutcome::Created(row) => {
            debug!(
                status = %row.id,
                "an Update brought a status this instance never stored"
            );
            Ok(Outcome::Handled)
        }
        ingest::IngestOutcome::Existing(row) => {
            debug!(
                status = %row.id,
                "an Update restated a status this instance does not edit from here"
            );
            Ok(Outcome::Handled)
        }
        ingest::IngestOutcome::Ignored(reason) => Ok(Outcome::Ignored(reason)),
    }
}

/// The edit fold of [`apply_update`], in the order the ticket fixes: the
/// stale gate, the parse, the versioned write, the attachments, the poll,
/// and the edit route's own hook — each step the same code the local edit
/// runs, on state the document restates.
async fn fold_edit(
    state: &AppState,
    object: &Value,
    signer: &User,
    stored: &Status,
) -> Result<Outcome, FederationError> {
    let now = Utc::now();
    let recency = recency(object, stored, now);
    if recency == Ordering::Less {
        return Ok(ignore_stale(stored));
    }
    let parsed = note_parser::parse_note(object, signer, now)
        .map_err(|err| FederationError::permanent("the updated object is not parseable", &err))?;
    if recency == Ordering::Equal && restates(&parsed, stored) {
        return Ok(ignore_stale(stored));
    }
    let mut mentioned = ingest::resolve_mentions(state, &parsed.mention_hrefs).await?;
    ingest::merge_accounts(
        &mut mentioned,
        ingest::resolve_local_audience(state, &parsed.audience).await?,
    );
    let media = stored_media(&state.db, stored.id)
        .await
        .map_err(|err| FederationError::transient("the attachment read failed", &err))?;
    let media_changed = media_replacement_needed(&media, &parsed.attachments);
    let params = edit_params(
        &parsed,
        stored,
        &mentioned,
        media_changed,
        update_time(object, now),
    );
    let Some(updated) = status_edit::update_status(&state.db, stored.id, params)
        .await
        .map_err(|err| FederationError::transient("the status edit failed", &err))?
    else {
        return Ok(Outcome::Ignored("the status was deleted under its edit"));
    };
    if media_changed {
        let old_ids: Vec<Uuid> = media.into_iter().map(|(id, _url)| id).collect();
        replace_media(state, stored.id, &old_ids, &parsed.attachments).await?;
    }
    refresh_poll(state, stored.id, parsed.poll.as_ref()).await?;
    after_status_edited(state, stored).await;
    debug!(
        status = %updated.id,
        "an Update folded a remote status edit"
    );
    Ok(Outcome::Handled)
}

/// The new state [`fold_edit`] writes: the document's restated fields —
/// content, source text, warning, sensitive flag, mentions and hashtags —
/// beside the two the ticket leaves the row's own: the language tag an
/// edit's document is not asked to restate, and everything the update's
/// parse cannot see. `media_changed` is the [module doc](self)'s attachment
/// replacement: an empty submitted set, which [`status_edit::update_status`]
/// reads as "detach everything the post wears now" — the rows the remote
/// named are nobody's uploads, so the fold rewrites them itself beside the
/// versioned write. `edited_at` is the author's own [`update_time`], the
/// stamp the row wears so the next stale check compares like with like.
fn edit_params(
    parsed: &ParsedNote,
    stored: &Status,
    mentioned: &[User],
    media_changed: bool,
    edited_at: Option<DateTime<Utc>>,
) -> EditParams {
    EditParams {
        content: parsed.status.content.clone(),
        raw_text: parsed.status.raw_text.clone(),
        content_type: parsed.status.content_type.clone(),
        spoiler_text: parsed.status.spoiler_text.clone(),
        sensitive: parsed.status.sensitive,
        language: stored.language.clone(),
        mentioned_user_ids: mentioned.iter().map(|account| account.id).collect(),
        hashtags: parsed.hashtags.clone(),
        media_ids: media_changed.then(Vec::new),
        edited_at,
    }
}

/// The answer of a [`fold_edit`] that found the `Update` stale.
fn ignore_stale(stored: &Status) -> Outcome {
    debug!(
        status = %stored.id,
        "an Update no newer than the stored version; ignoring"
    );
    Outcome::Ignored("an Update no newer than the stored version")
}

/// The stale gate of [`fold_edit`], as an ordering of the update's own
/// moment (see [`update_time`]) against the version this instance holds.
///
/// Remote edits are stamped with the author's clock, so both sides are the
/// same clock. `Less` is an edit the fold already made or an older peer's
/// restatement: it writes nothing. `Equal` is stale only when the content
/// is identical too ([`restates`]), since `updated` has one-second
/// precision on some servers. `Greater`, or no `updated` to compare, edits.
fn recency(object: &Value, stored: &Status, now: DateTime<Utc>) -> Ordering {
    update_time(object, now).map_or(Ordering::Greater, |incoming| {
        incoming.cmp(&stored.edited_at.unwrap_or(stored.inserted_at))
    })
}

/// Whether `parsed` restates the version `stored` already holds.
fn restates(parsed: &ParsedNote, stored: &Status) -> bool {
    parsed.status.content == stored.content
        && parsed.status.raw_text == stored.raw_text
        && parsed.status.spoiler_text == stored.spoiler_text
        && parsed.status.sensitive == stored.sensitive
}

/// The moment an `Update` claims its document was last changed: its
/// `updated`, clamped by [`clamped_time`] the way the note parse clamps
/// creation times — a stamp from a runaway clock answers `now`, still the
/// freshest the fold can honestly call it. `None` for a document without a
/// readable `updated`: its `published` says when the post was made, not when
/// it changed, so there is nothing to compare and the fold trusts the
/// sender and edits.
fn update_time(object: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    clamped_time(object.get("updated"), now)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::{
        app_state, author, author_again, links_for, live_content, mirrored, note, other, revisions,
        rows_for_uri, view_of,
    };
    use super::super::update_note;
    use super::*;

    /// The stored row for `uri`.
    async fn stored_row(pool: &sqlx::PgPool, uri: &str) -> Status {
        status::get_by_uri(pool, uri)
            .await
            .expect("the uri read")
            .expect("the post is stored")
    }

    /// An `Update` of `uri` to `text` carrying the author's `updated` stamp.
    fn edit_at(uri: &str, text: &str, updated: Option<&str>) -> Value {
        let mut extra = json!({ "content": format!("<p>{text}</p>") });
        if let Some(stamp) = updated {
            extra["updated"] = json!(stamp);
        }
        note(uri, extra)
    }

    #[tokio::test]
    async fn a_later_edit_older_than_local_now_still_applies_with_the_authors_time() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/skew-1";
        drop(mirrored(&pool, &author, uri, "first words").await);
        let first = Utc::now() - chrono::Duration::seconds(60);
        let second = Utc::now() - chrono::Duration::seconds(55);
        let stamp = |time: DateTime<Utc>| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        for (text, time) in [("edit a", first), ("edit b", second)] {
            let outcome = update_note(
                state.clone(),
                view_of("Update", edit_at(uri, text, Some(&stamp(time))), None),
                author_again(&pool).await,
            )
            .await
            .expect("the fold completes");
            assert_eq!(outcome, Outcome::Handled, "{text} is applied");
        }

        let row = stored_row(&pool, uri).await;
        assert_eq!(row.content, "<p>edit b</p>", "the newer edit won");
        assert_eq!(
            row.edited_at.map(|at| at.timestamp_millis()),
            Some(second.timestamp_millis()),
            "and `edited_at` is the author's time, not the receipt time"
        );
        assert_eq!(revisions(&pool, row.id).await.len(), 2, "both are kept");
    }

    #[tokio::test]
    async fn an_older_edit_delivered_late_is_still_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/late-1";
        drop(mirrored(&pool, &author, uri, "first words").await);
        let newer = "2024-05-01T12:00:05.000Z";
        let older = "2024-05-01T12:00:00.000Z";

        let first = update_note(
            state.clone(),
            view_of("Update", edit_at(uri, "edit b", Some(newer)), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");
        assert_eq!(first, Outcome::Handled, "the newer edit applies");
        let late = update_note(
            state,
            view_of("Update", edit_at(uri, "edit a", Some(older)), None),
            author,
        )
        .await
        .expect("the fold completes");

        assert_eq!(
            late,
            Outcome::Ignored("an Update no newer than the stored version"),
            "the older edit arrived second"
        );
        assert_eq!(
            live_content(&pool, uri).await.as_deref(),
            Some("<p>edit b</p>")
        );
    }

    #[tokio::test]
    async fn an_equal_stamp_applies_only_when_the_content_differs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/tie-1";
        drop(mirrored(&pool, &author, uri, "first words").await);
        let stamp = Some("2024-05-01T12:00:00.000Z");

        let outcomes = [
            ("edit a", Outcome::Handled),
            ("edit b", Outcome::Handled),
            (
                "edit b",
                Outcome::Ignored("an Update no newer than the stored version"),
            ),
        ];
        for (text, expected) in outcomes {
            let outcome = update_note(
                state.clone(),
                view_of("Update", edit_at(uri, text, stamp), None),
                author_again(&pool).await,
            )
            .await
            .expect("the fold completes");
            assert_eq!(outcome, expected, "{text} at the same second");
        }
        assert_eq!(
            live_content(&pool, uri).await.as_deref(),
            Some("<p>edit b</p>")
        );
    }

    #[tokio::test]
    async fn an_update_without_updated_and_new_content_applies() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/no-stamp-1";
        drop(mirrored(&pool, &author, uri, "first words").await);

        let outcome = update_note(
            state,
            view_of("Update", edit_at(uri, "fresh words", None), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "nothing to compare, so it edits");
        assert_eq!(
            live_content(&pool, uri).await.as_deref(),
            Some("<p>fresh words</p>")
        );
    }

    #[tokio::test]
    async fn an_edit_rewrites_the_post_and_records_its_previous_version() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let mentioned = other(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/edit-1";
        let row = mirrored(&pool, &author, uri, "first words").await;

        let outcome = update_note(
            state,
            view_of(
                "Update",
                note(
                    uri,
                    json!({
                        "content": "<p>second words</p>",
                        "updated": "2024-05-01T12:00:00.000Z",
                        "tag": [
                            { "type": "Hashtag", "href": "https://remote.example/tags/rust",
                              "name": "#Rust" },
                            { "type": "Mention", "href": mentioned.ap_id, "name": "@erin" },
                        ],
                        "attachment": [{
                            "type": "Document",
                            "mediaType": "image/png",
                            "url": "https://remote.example/media/edit-a.png",
                        }],
                    }),
                ),
                None,
            ),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the edit is folded");
        let edited = status::get_by_id(&pool, row.id)
            .await
            .expect("the row read")
            .expect("the post is still live");
        assert_eq!(edited.content, "<p>second words</p>", "the text is new");
        assert!(
            edited.edited_at.is_some(),
            "and the edit is stamped on the row"
        );

        let history = revisions(&pool, row.id).await;
        assert_eq!(history.len(), 1, "the superseded version is one row");
        assert_eq!(history[0].content, "<p>first words</p>");

        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT h.name FROM status_hashtags sh JOIN hashtags h ON h.id = sh.hashtag_id \
              WHERE sh.status_id = $1",
        )
        .bind(row.id)
        .fetch_all(&pool)
        .await
        .expect("the tag links read");
        assert_eq!(tags, ["rust"], "the new hashtag is linked");
        assert_eq!(
            links_for(
                &pool,
                "SELECT count(*) FROM status_mentions WHERE status_id = $1",
                row.id
            )
            .await,
            1,
            "and the resolved mention"
        );
        assert_eq!(
            links_for(
                &pool,
                "SELECT count(*) FROM media_attachments WHERE status_id = $1",
                row.id
            )
            .await,
            1,
            "and the attachment the update named"
        );
    }

    #[tokio::test]
    async fn an_edit_links_a_local_user_addressed_without_a_mention_tag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let alice = crate::models::user::insert(
            &pool,
            &crate::models::user::NewUser::local(
                "alice",
                &crate::models::user::local_ap_id(&state.config.base_url, "alice"),
            ),
        )
        .await
        .expect("the local account inserts");
        let uri = "https://remote.example/users/bob/statuses/edit-audience";
        let row = mirrored(&pool, &author, uri, "first words").await;

        let outcome = update_note(
            state,
            view_of(
                "Update",
                note(
                    uri,
                    json!({
                        "content": "<p>second words</p>",
                        "updated": "2024-05-01T12:00:00.000Z",
                        "to": [alice.ap_id],
                        "tag": [],
                    }),
                ),
                None,
            ),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the edit is folded");
        let mentioned: Vec<Uuid> =
            sqlx::query_scalar("SELECT user_id FROM status_mentions WHERE status_id = $1")
                .bind(row.id)
                .fetch_all(&pool)
                .await
                .expect("the mention rows read");
        assert_eq!(
            mentioned,
            vec![alice.id],
            "the addressed local user is linked"
        );
    }

    #[tokio::test]
    async fn an_update_no_newer_than_the_stored_version_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/stale-1";
        drop(mirrored(&pool, &author, uri, "first words").await);

        let fresh = note(
            uri,
            json!({
                "content": "<p>second words</p>",
                "updated": "2024-05-01T12:00:00.000Z",
            }),
        );
        let first = update_note(
            state.clone(),
            view_of("Update", fresh, None),
            author_again(&pool).await,
        )
        .await
        .expect("the first edit folds");
        assert_eq!(first, Outcome::Handled, "the fresh edit is applied");

        let outdated = note(
            uri,
            json!({
                "content": "<p>third words</p>",
                "updated": "2024-05-01T11:00:00.000Z",
            }),
        );
        let outcome = update_note(state, view_of("Update", outdated, None), author)
            .await
            .expect("the second fold completes");

        assert_eq!(
            outcome,
            Outcome::Ignored("an Update no newer than the stored version"),
            "an older update describes an older edit"
        );
        assert_eq!(
            live_content(&pool, uri).await.as_deref(),
            Some("<p>second words</p>"),
            "and rewrote nothing"
        );
    }

    #[tokio::test]
    async fn an_update_for_an_unknown_status_ingests_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/never-created";

        let outcome = update_note(
            state.clone(),
            view_of("Update", note(uri, json!({})), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the post lands");
        assert_eq!(
            live_content(&pool, uri).await.as_deref(),
            Some("<p>first words</p>"),
            "as the document describes it"
        );

        let again = update_note(state, view_of("Update", note(uri, json!({})), None), author)
            .await
            .expect("the second fold completes");
        assert_eq!(
            again,
            Outcome::Handled,
            "an Update with no `updated` has nothing to compare, so it is folded"
        );
        assert_eq!(
            revisions(&pool, stored_row(&pool, uri).await.id)
                .await
                .len(),
            0,
            "but a restatement of the same text supersedes no version"
        );
        assert_eq!(
            rows_for_uri(&pool, uri).await,
            1,
            "and the post is stored exactly once"
        );
    }

    #[tokio::test]
    async fn an_update_attributed_to_somebody_else_never_edits() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/not-my-edit";
        drop(mirrored(&pool, &author, uri, "first words").await);

        let stolen = json!({
            "id": uri,
            "type": "Note",
            "attributedTo": "https://remote.example/users/erin",
            "content": "<p>overwritten</p>",
            "updated": "2024-05-03T10:00:00.000Z",
        });
        let outcome = update_note(state, view_of("Update", stolen, None), author)
            .await
            .expect("the fold completes");

        assert_eq!(
            outcome,
            Outcome::Ignored("attributedTo is not the activity actor"),
            "the sender may only edit its own objects"
        );
        assert_eq!(
            live_content(&pool, uri).await.as_deref(),
            Some("<p>first words</p>"),
            "and rewrote nothing"
        );
    }
}
