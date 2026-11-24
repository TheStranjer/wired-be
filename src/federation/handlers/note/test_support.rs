//! Fixtures the inbound Note handler tests share: the delivering actor,
//! accounts, mirrored posts and the `Update` and `Question` documents.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

pub(super) use crate::federation::handlers::test_support::app_state;
use crate::{
    config::TEST_BASE_URL,
    federation::{
        dispatch::ActivityView,
        note_parser::{self, ParsedPoll},
    },
    models::{
        status::{self, NewStatus, Status},
        status_edit,
        user::{self, Flag, NewUser, User},
    },
    util::time::parse_time,
};

/// The actor every fixture here is delivered by, and the author of
/// every fixture post.
pub(super) const BOB: &str = "https://remote.example/users/bob";

/// The stored mirror of the fixture author.
pub(super) async fn author(pool: &PgPool) -> User {
    user::insert(
        pool,
        &NewUser {
            nickname: "bob@remote.example".to_owned(),
            ap_id: BOB.to_owned(),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the author fixture inserts")
}

/// The author account re-read from storage: the handlers take their
/// signer by value, so every delivery in a test needs a copy of its own.
pub(super) async fn author_again(pool: &PgPool) -> User {
    user::get_by_ap_id(pool, BOB)
        .await
        .expect("the author read")
        .expect("the author is stored")
}

/// A second remote account on the same host, so a `Delete` aimed at
/// somebody else's post survives containment's host rule.
pub(super) async fn other(pool: &PgPool) -> User {
    user::insert(
        pool,
        &NewUser {
            nickname: "erin@remote.example".to_owned(),
            ap_id: "https://remote.example/users/erin".to_owned(),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the other account fixture inserts")
}

/// The stored mirror of a note-family `Update`/`Delete` of `object`,
/// delivered by [`BOB`] — or by `actor` where a test names one.
pub(super) fn view_of(kind: &str, object: Value, actor: Option<&str>) -> ActivityView {
    let mut envelope = json!({
        "id": format!("https://remote.example/activities/{kind}-{}", Uuid::now_v7()),
        "type": kind,
        "actor": actor.unwrap_or(BOB),
    });
    envelope["object"] = object;
    ActivityView::parse(&envelope).expect("the fixture is a dispatchable activity")
}

/// A `Note`-family document under `uri`, attributed to [`BOB`], with
/// every extra field `extra` merges over it.
pub(super) fn note(uri: &str, extra: Value) -> Value {
    let mut object = json!({
        "id": uri,
        "type": "Note",
        "attributedTo": BOB,
        "content": "<p>first words</p>",
        "published": "2024-05-01T10:00:00.000Z",
    });
    json_extend(&mut object, extra);
    object
}

/// Merges `extra`'s fields over `target`, dropping what they displace.
pub(super) fn json_extend(target: &mut Value, extra: Value) {
    let (Some(map), Value::Object(additions)) = (target.as_object_mut(), extra) else {
        panic!("both documents are JSON objects");
    };
    for (key, value) in additions {
        let previous = map.insert(key, value);
        drop(previous);
    }
}

/// The instant the fixture notes are published, and the anchor every
/// mirrored row is stamped with — a fixed past moment so a fixture
/// `updated` in 2024 postdates it without depending on the wall clock.
pub(super) fn published() -> DateTime<Utc> {
    parse_time("2024-05-01T10:00:00.000Z").expect("the fixture time parses")
}

/// The mirrored note `uri` as this instance stores it after a `Create`:
/// `author`'s remote row of `text`, authored at [`published`].
pub(super) async fn mirrored(pool: &PgPool, author: &User, uri: &str, text: &str) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            uri: Some(uri.to_owned()),
            content: format!("<p>{text}</p>"),
            raw_text: text.to_owned(),
            user_id: author.id,
            inserted_at: Some(published()),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the mirrored note inserts")
}

/// How many live statuses `uri` names, with their content.
pub(super) async fn live_content(pool: &PgPool, uri: &str) -> Option<String> {
    let row = status::get_by_uri(pool, uri).await.expect("the uri read");
    row.filter(|status| status.deleted_at.is_none())
        .map(|status| status.content)
}

/// How many `statuses` rows carry object uri `uri`, tombstone included.
pub(super) async fn rows_for_uri(pool: &PgPool, uri: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(uri)
        .fetch_one(pool)
        .await
        .expect("the row count reads")
}

/// How many rows of `table` hang off the status `id` — an expectation
/// about a link set the edit rewrote.
pub(super) async fn links_for(pool: &PgPool, sql: &'static str, id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|err| panic!("the link count should read: {err}"))
}

/// The superseded versions of `status_id`, oldest first.
pub(super) async fn revisions(pool: &PgPool, status_id: Uuid) -> Vec<status_edit::StatusEdit> {
    status_edit::list_for_status(pool, status_id)
        .await
        .expect("the revisions read")
}

/// The further deadline `question_update` names, as an instant.
pub(super) fn end_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2024-06-02T00:00:00Z")
        .expect("the deadline is an instant")
        .with_timezone(&Utc)
}

/// The `Question` update: the same poll titles as the fixture, with
/// the remote's new totals and a further deadline.
pub(super) fn question_update(uri: &str) -> Value {
    json!({
        "id": uri,
        "type": "Question",
        "attributedTo": BOB,
        "content": "<p>tea or coffee?</p>",
        "published": "2024-05-01T10:00:00.000Z",
        "updated": "2024-05-02T10:00:00.000Z",
        "endTime": "2024-06-02T00:00:00Z",
        "votersCount": 42,
        "oneOf": [
            {
                "type": "Note",
                "name": "Tea",
                "replies": { "type": "Collection", "totalItems": 30 },
            },
            {
                "type": "Note",
                "name": "Coffee",
                "replies": { "type": "Collection", "totalItems": 15 },
            },
        ],
    })
}

/// The ingest-side poll fixture: the options `Tea` and `Coffee` with
/// `voters` counted, closing at `until`.
pub(super) fn json_parsed_poll(voters: i64, until: DateTime<Utc>) -> ParsedPoll {
    ParsedPoll {
        options: vec![
            note_parser::ParsedPollOption {
                title: "Tea".to_owned(),
                votes_count: 10,
            },
            note_parser::ParsedPollOption {
                title: "Coffee".to_owned(),
                votes_count: 8,
            },
        ],
        multiple: false,
        expires_at: Some(until),
        voters_count: Some(voters),
    }
}
