//! The write arms' tests: attachments, emojis and hashtags land beside the
//! status, a mirrored `Question` carries the counts the remote reported,
//! and a root of nothing mints its own conversation.

use serde_json::{Value, json};

use super::*;
use crate::{
    models::{custom_emoji, status::Visibility},
    services::ingest::{
        IngestOutcome, ingest_note,
        test_support::{app_state, author},
    },
};

#[tokio::test]
async fn attachments_emojis_and_hashtags_land_with_the_status() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;

    let outcome = ingest_note(&state, &bob, &satellites_object())
        .await
        .expect("the satellites ingest");
    let IngestOutcome::Created(row) = outcome else {
        panic!("the post writes, got {outcome:?}");
    };

    let media = media_attachment::for_statuses(&pool, &[row.id])
        .await
        .expect("the media reads")
        .remove(&row.id)
        .expect("the status carries media");
    assert_media_rows(&media);

    let emoji =
        custom_emoji::get_by_shortcodes(&pool, &[String::from("ralph")], Some("remote.example"))
            .await
            .expect("the emoji reads");
    assert_eq!(emoji.len(), 1, "the Emoji tag upserted its row");
    assert_eq!(
        emoji[0].domain.as_deref(),
        Some("remote.example"),
        "under the author's domain"
    );
    assert_eq!(
        emoji[0].image_url, "https://remote.example/emoji/ralph.png",
        "with the announced image"
    );

    let tags = sqlx::query_scalar::<_, String>(
        "SELECT t.name FROM hashtags t
               JOIN status_hashtags sh ON sh.hashtag_id = t.id
              WHERE sh.status_id = $1",
    )
    .bind(row.id)
    .fetch_all(&pool)
    .await
    .expect("the tags read");
    assert_eq!(tags, vec!["rust".to_owned()], "the Hashtag tag linked");
}

/// The note object carrying one `Hashtag`, one `Emoji` and two
/// `Document` attachments, the first with the full media column set.
fn satellites_object() -> Value {
    json!({
        "id": "https://remote.example/objects/ingest-satellites",
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>look</p>",
        "tag": [
            { "type": "Hashtag", "name": "#Rust" },
            {
                "type": "Emoji",
                "name": ":ralph:",
                "icon": { "type": "Image", "url": "https://remote.example/emoji/ralph.png" }
            },
        ],
        "attachment": [
            {
                "type": "Document",
                "mediaType": "image/png",
                "url": "https://remote.example/media/b.png",
                "name": "Second",
                "blurhash": "LEHV6nWB2yk8pyo0adR*.7kCMdnj",
                "width": 640,
                "height": 480
            },
            {
                "type": "Document",
                "mediaType": "image/jpeg",
                "url": "https://remote.example/media/a.jpg",
                "name": "First"
            },
        ],
    })
}

/// The attachment rows the ingest wrote: both documents, in document
/// order, unowned, with the stated url, alt text, placeholder and
/// dimensions kept.
fn assert_media_rows(media: &[media_attachment::MediaAttachment]) {
    assert_eq!(media.len(), 2, "both attachments land");
    assert_eq!(
        media[0].url, "https://remote.example/media/b.png",
        "in document order"
    );
    assert_eq!(
        media[0].remote_url.as_deref(),
        Some("https://remote.example/media/b.png"),
        "the document url is the remote url"
    );
    assert_eq!(media[0].user_id, None, "remote media has no uploader");
    assert_eq!(
        media[0].description.as_deref(),
        Some("Second"),
        "and its alt text"
    );
    assert_eq!(
        media[0].blurhash.as_deref(),
        Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj"),
        "and its placeholder"
    );
    assert_eq!(media[0].width, Some(640), "and its dimensions");
    assert_eq!(media[0].height, Some(480), "both");
}

#[tokio::test]
async fn a_question_lands_its_poll_with_the_reported_counts() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let object = json!({
        "id": "https://remote.example/objects/ingest-poll",
        "type": "Question",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>which?</p>",
        "oneOf": [
            { "type": "Note", "name": "Red", "replies": { "totalItems": 3 } },
            { "type": "Note", "name": "Blue", "replies": { "totalItems": 7 } }
        ],
        "votersCount": 10
    });

    let outcome = ingest_note(&state, &bob, &object)
        .await
        .expect("the question ingests");
    let IngestOutcome::Created(row) = outcome else {
        panic!("the poll post writes, got {outcome:?}");
    };

    let stand = poll::get_for_status(&pool, row.id)
        .await
        .expect("the poll reads")
        .expect("the status carries a poll");
    assert_eq!(stand.voters_count, 10, "the reported voter count");
    assert_eq!(stand.votes_count, 10, "and the votes total");
    let choices = poll::options(&pool, stand.id)
        .await
        .expect("the options read");
    let votes: Vec<i32> = choices.iter().map(|choice| choice.votes_count).collect();
    assert_eq!(votes, vec![3, 7], "per option, in document order");
}

#[tokio::test]
async fn a_thirty_option_question_is_ingested_not_dropped() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let options: Vec<Value> = (0..30)
        .map(|n| json!({ "type": "Note", "name": format!("opt{n}"), "replies": { "totalItems": n } }))
        .collect();
    let object = json!({
        "id": "https://remote.example/objects/ingest-big-poll",
        "type": "Question",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>many choices</p>",
        "oneOf": options
    });

    let outcome = ingest_note(&state, &bob, &object)
        .await
        .expect("a 30-option question ingests");
    let IngestOutcome::Created(row) = outcome else {
        panic!("the poll post writes, got {outcome:?}");
    };

    let stand = poll::get_for_status(&pool, row.id)
        .await
        .expect("the poll reads")
        .expect("the status carries a poll");
    let choices = poll::options(&pool, stand.id)
        .await
        .expect("the options read");
    assert_eq!(
        choices.len(),
        30,
        "every option lands, under the federated cap"
    );
    assert_eq!(choices[29].title, "opt29", "in document order");
}

#[tokio::test]
async fn a_direct_note_is_its_own_conversation_root() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let object = json!({
        "id": "https://remote.example/objects/ingest-dm",
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://test.example/users/someone"],
        "content": "<p>a direct note</p>",
    });

    let outcome = ingest_note(&state, &bob, &object)
        .await
        .expect("the direct note ingests");
    let IngestOutcome::Created(row) = outcome else {
        panic!("the post writes, got {outcome:?}");
    };

    assert_eq!(
        row.visibility,
        Visibility::Direct,
        "a non-public, non-followers addressing parses direct"
    );
    assert_eq!(
        row.conversation_id, row.id,
        "a root of nothing mints its own conversation"
    );
    assert!(
        row.in_reply_to_id.is_none() && row.in_reply_to_ap_id.is_none(),
        "and carries no reply reference of either kind"
    );
}

#[tokio::test]
async fn the_advertised_quote_policy_lands_with_the_status() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let policy = json!({"canQuote": {
        "automaticApproval": ["https://remote.example/users/bob/followers"],
        "manualApproval": [],
    }});
    let advertising = json!({
        "id": "https://remote.example/objects/ingest-policy",
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>with a policy</p>",
        "interactionPolicy": policy,
    });
    let silent = json!({
        "id": "https://remote.example/objects/ingest-no-policy",
        "type": "Note",
        "attributedTo": "https://remote.example/users/bob",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "content": "<p>without one</p>",
    });

    let IngestOutcome::Created(with) = ingest_note(&state, &bob, &advertising)
        .await
        .expect("the advertising note ingests")
    else {
        panic!("the advertising note writes");
    };
    let IngestOutcome::Created(without) = ingest_note(&state, &bob, &silent)
        .await
        .expect("the silent note ingests")
    else {
        panic!("the silent note writes");
    };

    assert_eq!(
        with.quote_policy.map(|stored| stored.0),
        Some(status::QuotePolicy {
            automatic: vec!["https://remote.example/users/bob/followers".to_owned()],
            manual: Vec::new(),
        }),
        "the canQuote lists are stored"
    );
    assert_eq!(
        without.quote_policy, None,
        "a note without interactionPolicy stores none"
    );
}

#[tokio::test]
async fn a_lost_uri_race_reports_raced_and_writes_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let object = satellites_object();
    let parsed = crate::federation::note_parser::parse_note(&object, &bob, chrono::Utc::now())
        .expect("the note parses");
    let new = build_new_status(&parsed, &ReplyLink::default(), None, &[]);

    let first = write_ingested(&state, &new, &parsed)
        .await
        .expect("the first write lands");
    let Written::Created(winner, _) = first else {
        panic!("the first write creates, got {first:?}");
    };
    let second = write_ingested(&state, &new, &parsed)
        .await
        .expect("a duplicate uri is no error");

    assert!(
        matches!(second, Written::Raced),
        "the second write loses the race, got {second:?}"
    );
    let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(&winner.uri)
        .fetch_one(&pool)
        .await
        .expect("the rows count");
    assert_eq!(rows, 1, "still one status for the uri");
    let media = media_attachment::for_statuses(&pool, &[winner.id])
        .await
        .expect("the media reads")
        .remove(&winner.id)
        .expect("the winner keeps its media");
    assert_eq!(media.len(), 2, "the loser added no media");
}

#[tokio::test]
async fn concurrent_ingests_of_one_object_agree_on_one_status() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let bob = author(&pool).await;
    let object = satellites_object();

    let (left, right) = tokio::join!(
        ingest_note(&state, &bob, &object),
        ingest_note(&state, &bob, &object),
    );
    let left_id = stored_id(left.expect("the first ingest does not error"));
    let right_id = stored_id(right.expect("the second ingest does not error"));

    assert_eq!(left_id, right_id, "both calls answer the same status");
    let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses")
        .fetch_one(&pool)
        .await
        .expect("the rows count");
    assert_eq!(rows, 1, "one row stored");
}

/// The id of the status an ingest outcome stored or found.
fn stored_id(outcome: IngestOutcome) -> Uuid {
    match outcome {
        IngestOutcome::Created(row) | IngestOutcome::Existing(row) => row.id,
        IngestOutcome::Ignored(reason) => panic!("ignored: {reason}"),
    }
}
