//! Tests for the answers to a `QuoteRequest` this instance sent: the grant,
//! the refusal and the revocation, and the answers that are ignored.

use serde_json::{Value, json};
use sqlx::PgPool;

use super::*;
use crate::{
    config::TEST_BASE_URL,
    federation::{
        handlers::{
            follow::{handle_accept, handle_reject},
            note::delete_note,
            test_support::{app_state, assert_handled, ignored, local_user},
        },
        publisher::KIND_PUBLISH,
    },
    models::{
        quote_request::NewQuoteRequest,
        status::{self, NewStatus, Status},
    },
    test_fixtures::remote_user,
};

/// The quoted author on `remote.example`.
const BOB: &str = "https://remote.example/users/bob";
/// The id the stored request was sent under.
const REQUEST: &str = "https://test.example/activities/quote-request-1";
/// The authorization the author grants.
const GRANT: &str = "https://remote.example/quote_authorizations/1";

/// Everything one test needs: the stored request, its quoting post and the
/// accounts around them.
struct Fixture {
    /// The quoted author, who answers.
    bob: User,
    /// The local quoting post.
    quoting: Status,
    /// The stored request.
    request: QuoteRequest,
}

/// A local post quoting a remote post of bob's, with its pending request.
async fn scenario(pool: &PgPool) -> Fixture {
    let alice = local_user(pool, "alice").await;
    let bob = remote_user(pool, "bob").await;
    let quoted = status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            uri: Some("https://remote.example/objects/quoted".to_owned()),
            user_id: bob.id,
            ..NewStatus::default()
        },
    )
    .await
    .expect("the quoted post inserts");
    let quoting = status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            quote_of_id: Some(quoted.id),
            ..NewStatus::local(&alice, "alice quoting")
        },
    )
    .await
    .expect("the quoting post inserts");
    let request = quote_request::insert(
        pool,
        &NewQuoteRequest {
            quoting_status_id: quoting.id,
            quoted_status_id: quoted.id,
            quoted_user_id: bob.id,
            request_uri: REQUEST,
        },
    )
    .await
    .expect("the request writes");
    Fixture {
        bob,
        quoting,
        request,
    }
}

/// The request as an answer embeds it.
fn embedded_request() -> Value {
    json!({
        "id": REQUEST,
        "type": "QuoteRequest",
        "actor": "https://test.example/users/alice",
        "object": "https://remote.example/objects/quoted",
        "instrument": "https://test.example/objects/quoting",
    })
}

/// An answer of `kind` by `actor` over `object`, with an optional `result`.
fn answer(kind: &str, actor: &str, object: &Value, result: Option<&str>) -> ActivityView {
    let mut document = json!({
        "id": format!("https://remote.example/activities/{kind}-1"),
        "type": kind,
        "actor": actor,
        "object": object,
    });
    if let Some(grant) = result {
        document["result"] = json!(grant);
    }
    ActivityView::parse(&document).expect("the fixture is a dispatchable activity")
}

/// The request as it stands now.
async fn stored(pool: &PgPool, fixture: &Fixture) -> QuoteRequest {
    quote_request::by_request_ap_id(pool, &fixture.request.request_ap_id)
        .await
        .expect("the request reads")
        .expect("the request is stored")
}

/// The activities queued for publication, oldest first.
async fn published(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT args -> 'activity' FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(KIND_PUBLISH)
    .fetch_all(pool)
    .await
    .expect("the publish jobs read")
}

/// An `Accept` that embeds the request grants it: the grant is stored and
/// one `Update` of the quoting note, now naming it, is queued.
#[tokio::test]
async fn an_accept_stores_the_grant_and_republishes_the_note() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let fixture = scenario(&pool).await;
    let view = answer("Accept", BOB, &embedded_request(), Some(GRANT));

    let outcome = handle_accept(app_state(&pool), view, fixture.bob.clone()).await;

    assert_handled(&outcome);
    let row = stored(&pool, &fixture).await;
    assert_eq!(row.state(), QuoteRequestState::Accepted, "the quote stands");
    assert_eq!(row.authorization_uri.as_deref(), Some(GRANT), "the grant");
    let updates = published(&pool).await;
    assert_eq!(updates.len(), 1, "one Update is queued");
    assert_eq!(updates[0]["type"], "Update", "an Update");
    assert_eq!(
        updates[0]["object"]["id"], fixture.quoting.uri,
        "of the quoting note"
    );
    assert_eq!(
        updates[0]["object"]["quoteAuthorization"], GRANT,
        "which now names the authorization"
    );
}

/// The request may be named by its id instead of embedded.
#[tokio::test]
async fn an_accept_may_name_the_request_by_id() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let fixture = scenario(&pool).await;
    let view = answer("Accept", BOB, &json!(REQUEST), Some(GRANT));

    let outcome = handle_accept(app_state(&pool), view, fixture.bob.clone()).await;

    assert_handled(&outcome);
    assert_eq!(
        stored(&pool, &fixture).await.state(),
        QuoteRequestState::Accepted,
        "the id finds the request"
    );
}

/// Only the quoted author may answer.
#[tokio::test]
async fn an_accept_from_another_account_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let fixture = scenario(&pool).await;
    let erin = remote_user(&pool, "erin").await;
    let view = answer(
        "Accept",
        "https://remote.example/users/erin",
        &embedded_request(),
        Some(GRANT),
    );

    let outcome = handle_accept(app_state(&pool), view, erin).await;

    assert_eq!(
        ignored(outcome),
        "an Accept of a quote request from an account that is not the quoted author",
        "erin has no say"
    );
    assert_eq!(
        stored(&pool, &fixture).await.state(),
        QuoteRequestState::Pending,
        "the request is untouched"
    );
    assert!(published(&pool).await.is_empty(), "and nothing is queued");
}

/// A grant has to name an authorization on the author's own host.
#[tokio::test]
async fn an_accept_without_a_usable_authorization_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let fixture = scenario(&pool).await;
    let foreign = "https://elsewhere.example/quote_authorizations/1";

    for result in [None, Some(foreign)] {
        let view = answer("Accept", BOB, &embedded_request(), result);
        let outcome = handle_accept(app_state(&pool), view, fixture.bob.clone()).await;
        assert_eq!(
            ignored(outcome),
            "an Accept of a quote request with no authorization on the quoted author's host",
            "{result:?} grants nothing"
        );
    }

    assert_eq!(
        stored(&pool, &fixture).await.state(),
        QuoteRequestState::Pending,
        "the request is untouched"
    );
    assert!(published(&pool).await.is_empty(), "and nothing is queued");
}

/// A redelivered `Accept` neither changes the row nor queues another
/// `Update`.
#[tokio::test]
async fn a_repeated_accept_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let fixture = scenario(&pool).await;
    let view = answer("Accept", BOB, &embedded_request(), Some(GRANT));
    let first = handle_accept(app_state(&pool), view.clone(), fixture.bob.clone()).await;
    assert_handled(&first);

    let again = handle_accept(app_state(&pool), view, fixture.bob.clone()).await;

    assert_eq!(
        ignored(again),
        "an Accept that restates the grant",
        "the restatement is a no-op"
    );
    assert_eq!(published(&pool).await.len(), 1, "one Update in all");
}

/// An embedded `QuoteRequest` this instance never sent is ignored, and an
/// `Accept` that is no quote answer keeps its follow route.
#[tokio::test]
async fn an_accept_of_something_else_falls_through() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let fixture = scenario(&pool).await;
    let stranger = json!({
        "id": "https://test.example/activities/never-sent",
        "type": "QuoteRequest",
        "actor": "https://test.example/users/alice",
        "object": "https://remote.example/objects/quoted",
    });

    let unknown = handle_accept(
        app_state(&pool),
        answer("Accept", BOB, &stranger, Some(GRANT)),
        fixture.bob.clone(),
    )
    .await;
    let follow = handle_accept(
        app_state(&pool),
        answer(
            "Accept",
            BOB,
            &json!("https://test.example/activities/some-follow"),
            None,
        ),
        fixture.bob.clone(),
    )
    .await;

    assert_eq!(
        ignored(unknown),
        "an Accept of a QuoteRequest this instance never sent",
        "a request nobody sent"
    );
    assert_eq!(
        ignored(follow),
        "an Accept naming no follow this instance stores",
        "the follow handshake still answers its own"
    );
}

/// A `Reject` marks the quote rejected and keeps the post; with no grant to
/// withdraw, nothing is re-published.
#[tokio::test]
async fn a_reject_marks_the_quote_rejected() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let fixture = scenario(&pool).await;
    let view = answer("Reject", BOB, &embedded_request(), None);

    let outcome = handle_reject(app_state(&pool), view.clone(), fixture.bob.clone()).await;

    assert_handled(&outcome);
    assert_eq!(
        stored(&pool, &fixture).await.state(),
        QuoteRequestState::Rejected,
        "the quote is rejected"
    );
    let post = status::get_by_id(&pool, fixture.quoting.id)
        .await
        .expect("the post reads");
    assert!(post.is_some(), "the quoting post stays");
    assert!(published(&pool).await.is_empty(), "nothing is re-published");
    let again = handle_reject(app_state(&pool), view, fixture.bob.clone()).await;
    assert_eq!(
        ignored(again),
        "an answer that restates the state the quote already has",
        "a repeated Reject is a no-op"
    );
}

/// Only the quoted author may refuse.
#[tokio::test]
async fn a_reject_from_another_account_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let fixture = scenario(&pool).await;
    let erin = remote_user(&pool, "erin").await;
    let view = answer(
        "Reject",
        "https://remote.example/users/erin",
        &embedded_request(),
        None,
    );

    let outcome = handle_reject(app_state(&pool), view, erin).await;

    assert_eq!(
        ignored(outcome),
        "a Reject of a quote request from an account that is not the quoted author",
        "erin has no say"
    );
    assert_eq!(
        stored(&pool, &fixture).await.state(),
        QuoteRequestState::Pending,
        "the request is untouched"
    );
}

/// A `Reject` of a granted quote takes the grant back: the note is
/// re-published without its authorization.
#[tokio::test]
async fn a_reject_after_a_grant_republishes_the_note_without_it() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let fixture = scenario(&pool).await;
    quote_request::accept(&pool, fixture.request.id, GRANT)
        .await
        .expect("the grant stores");
    let view = answer("Reject", BOB, &embedded_request(), None);

    let outcome = handle_reject(app_state(&pool), view, fixture.bob.clone()).await;

    assert_handled(&outcome);
    let updates = published(&pool).await;
    assert_eq!(updates.len(), 1, "one Update is queued");
    assert!(
        updates[0]["object"].get("quoteAuthorization").is_none(),
        "the note no longer names the grant"
    );
}

/// The `Delete` of the authorization revokes the quote and re-publishes the
/// note without it.
#[tokio::test]
async fn a_delete_of_the_authorization_revokes_the_quote() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let fixture = scenario(&pool).await;
    quote_request::accept(&pool, fixture.request.id, GRANT)
        .await
        .expect("the grant stores");
    let view = answer("Delete", BOB, &json!(GRANT), None);

    let outcome = delete_note(app_state(&pool), view.clone(), fixture.bob.clone()).await;

    assert_handled(&outcome);
    assert_eq!(
        stored(&pool, &fixture).await.state(),
        QuoteRequestState::Revoked,
        "the quote is revoked"
    );
    let updates = published(&pool).await;
    assert_eq!(updates.len(), 1, "one Update is queued");
    assert_eq!(updates[0]["type"], "Update", "an Update");
    assert!(
        updates[0]["object"].get("quoteAuthorization").is_none(),
        "the note no longer names the grant"
    );
    let again = delete_note(app_state(&pool), view, fixture.bob.clone()).await;
    assert_eq!(
        ignored(again),
        "an answer that restates the state the quote already has",
        "a repeated Delete is a no-op"
    );
}

/// Only the account that granted the authorization may withdraw it.
#[tokio::test]
async fn a_delete_of_the_authorization_by_another_account_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let fixture = scenario(&pool).await;
    quote_request::accept(&pool, fixture.request.id, GRANT)
        .await
        .expect("the grant stores");
    let erin = remote_user(&pool, "erin").await;
    let view = answer(
        "Delete",
        "https://remote.example/users/erin",
        &json!(GRANT),
        None,
    );

    let outcome = delete_note(app_state(&pool), view, erin).await;

    assert_eq!(
        ignored(outcome),
        "a Delete of a quote authorization by an account that did not grant it",
        "erin cannot withdraw bob's grant"
    );
    assert_eq!(
        stored(&pool, &fixture).await.state(),
        QuoteRequestState::Accepted,
        "the grant stands"
    );
}
