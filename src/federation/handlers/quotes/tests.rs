//! Tests for the inbound `QuoteRequest`: the grant and its answer, the
//! refusals by visibility and standing, and the requests that are ignored.

use serde_json::{Value, json};
use sqlx::PgPool;

use super::*;
use crate::{
    config::TEST_BASE_URL,
    federation::{
        delivery::KIND_DELIVER,
        handlers::test_support::{accepted_follow, app_state, assert_handled, ignored, local_user},
        vocab::AS_PUBLIC,
    },
    models::{
        block,
        status::{NewStatus, QuoteApprovalPolicy, Visibility},
    },
    test_fixtures::federating_remote,
};

/// The quoting account, on `remote.example`.
const BOB: &str = "https://remote.example/users/bob";
/// The quoting post's object id.
const QUOTING: &str = "https://remote.example/objects/bob-quote";

/// A remote mirror of `nickname` on `remote.example` that can be delivered
/// to.
async fn remote(pool: &PgPool, nickname: &str) -> User {
    federating_remote(pool, nickname).await
}

/// A local post of `author` seen as `visibility`.
async fn post(pool: &PgPool, author: &User, visibility: Visibility) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility,
            ..NewStatus::local(author, "quote me")
        },
    )
    .await
    .expect("the quoted post inserts")
}

/// A post of `author` stored as if it had been mirrored from its peer.
async fn mirrored(pool: &PgPool, author: &User, uri: &str) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            uri: Some(uri.to_owned()),
            user_id: author.id,
            ..NewStatus::default()
        },
    )
    .await
    .expect("the mirrored post inserts")
}

/// The quoting note of `author` as the request embeds it; `quoted` is the
/// post it quotes, left out when `None`.
fn quoting_note(author: &str, quoted: Option<&str>) -> Value {
    let mut note = json!({
        "id": QUOTING,
        "type": "Note",
        "attributedTo": author,
        "to": [AS_PUBLIC],
        "content": "<p>bob quoting</p>",
    });
    if let Some(uri) = quoted {
        note["quote"] = json!(uri);
    }
    note
}

/// A `QuoteRequest` `n` by bob for `object`, carrying `instrument`.
fn request(n: &str, object: &str, instrument: &Value) -> ActivityView {
    ActivityView::parse(&json!({
        "id": format!("https://remote.example/activities/{n}"),
        "type": "QuoteRequest",
        "actor": BOB,
        "object": object,
        "instrument": instrument,
    }))
    .expect("the fixture is a dispatchable activity")
}

/// The documents delivered so far, oldest first.
async fn deliveries(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT args -> 'body' FROM jobs WHERE kind = $1 ORDER BY inserted_at, id",
    )
    .bind(KIND_DELIVER)
    .fetch_all(pool)
    .await
    .expect("the delivery jobs read")
}

/// How many grants are stored.
async fn grants(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM quote_authorizations")
        .fetch_one(pool)
        .await
        .expect("the grant count reads")
}

/// A public note with an embedded instrument is granted: the grant is
/// stored, the quoting post is ingested and linked, and one `Accept` whose
/// `result` names the grant's document is queued for the quoter.
#[tokio::test]
async fn a_public_note_is_granted_with_an_embedded_instrument() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote(&pool, "bob").await;
    let quoted = post(&pool, &alice, Visibility::Public).await;

    let outcome = handle_quote_request(
        app_state(&pool),
        request("q1", &quoted.uri, &quoting_note(BOB, Some(&quoted.uri))),
        bob,
    )
    .await;

    assert_handled(&outcome);
    assert_eq!(grants(&pool).await, 1, "one grant is stored");
    let stored = status::get_by_uri(&pool, QUOTING)
        .await
        .expect("the quoting read runs")
        .expect("the quoting post was ingested");
    assert_eq!(
        stored.quote_of_id,
        Some(quoted.id),
        "and it quotes the local post"
    );
    let sent = deliveries(&pool).await;
    assert_eq!(sent.len(), 1, "one answer is queued");
    let grant_id = sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM quote_authorizations")
        .fetch_one(&pool)
        .await
        .expect("the grant id reads");
    assert_eq!(sent[0]["type"], "Accept", "the answer grants");
    assert_eq!(sent[0]["actor"], alice.ap_id, "the quoted author answers");
    assert_eq!(
        sent[0]["result"],
        format!("{TEST_BASE_URL}/quote_authorizations/{grant_id}"),
        "the result is the grant's document"
    );
    assert_eq!(
        sent[0]["object"]["id"], "https://remote.example/activities/q1",
        "and the request rides inside"
    );
}

/// A stored quoting post that has not named its quote is linked by the
/// grant, and the quoted post counts it once.
#[tokio::test]
async fn a_grant_links_a_stored_quoting_post_that_names_no_quote() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote(&pool, "bob").await;
    let quoted = post(&pool, &alice, Visibility::Unlisted).await;
    let quoting = mirrored(&pool, &bob, QUOTING).await;

    let outcome = handle_quote_request(
        app_state(&pool),
        request("q1", &quoted.uri, &json!(QUOTING)),
        bob,
    )
    .await;

    assert_handled(&outcome);
    let linked = status::get_by_id(&pool, quoting.id)
        .await
        .expect("the read runs")
        .expect("the quoting post is stored");
    assert_eq!(linked.quote_of_id, Some(quoted.id), "the quote is linked");
    let counted = status::get_by_id(&pool, quoted.id)
        .await
        .expect("the read runs")
        .expect("the quoted post is stored");
    assert_eq!(counted.quotes_count, 1, "and counted once");
}

/// A redelivered request restates the one grant; every answer names it.
#[tokio::test]
async fn a_redelivered_request_restates_the_grant() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote(&pool, "bob").await;
    let quoted = post(&pool, &alice, Visibility::Public).await;
    let note = quoting_note(BOB, None);

    for n in ["q1", "q2"] {
        let outcome = handle_quote_request(
            app_state(&pool),
            request(n, &quoted.uri, &note),
            bob.clone(),
        )
        .await;
        assert_handled(&outcome);
    }

    assert_eq!(grants(&pool).await, 1, "one grant for the post pair");
    let sent = deliveries(&pool).await;
    assert_eq!(sent.len(), 2, "each request is answered");
    assert_eq!(
        sent[0]["result"], sent[1]["result"],
        "with the same authorization"
    );
}

/// A private post is quotable by the author's accepted followers only: a
/// stranger is rejected and stores nothing, a follower is granted.
#[tokio::test]
async fn a_private_post_is_granted_to_followers_only() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote(&pool, "bob").await;
    let quoted = post(&pool, &alice, Visibility::Private).await;
    let note = quoting_note(BOB, None);

    let stranger = handle_quote_request(
        app_state(&pool),
        request("q1", &quoted.uri, &note),
        bob.clone(),
    )
    .await;
    assert_handled(&stranger);
    assert_eq!(grants(&pool).await, 0, "a stranger is granted nothing");
    assert_eq!(
        deliveries(&pool).await[0]["type"],
        "Reject",
        "and is told so"
    );

    accepted_follow(&pool, &bob, &alice).await;
    let follower =
        handle_quote_request(app_state(&pool), request("q2", &quoted.uri, &note), bob).await;
    assert_handled(&follower);
    assert_eq!(grants(&pool).await, 1, "a follower is granted");
    assert_eq!(
        deliveries(&pool).await[1]["type"],
        "Accept",
        "and is told so"
    );
}

/// A public post of `author` under the author's own quote `policy`.
async fn post_under(pool: &PgPool, author: &User, policy: QuoteApprovalPolicy) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            quote_approval_policy: Some(policy),
            ..NewStatus::local(author, "quote me, or not")
        },
    )
    .await
    .expect("the quoted post inserts")
}

/// A public post whose author chose `nobody` is refused with a `Reject`
/// and stores no grant.
#[tokio::test]
async fn a_nobody_policy_rejects_even_a_follower() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote(&pool, "bob").await;
    accepted_follow(&pool, &bob, &alice).await;
    let quoted = post_under(&pool, &alice, QuoteApprovalPolicy::Nobody).await;

    let outcome = handle_quote_request(
        app_state(&pool),
        request("q1", &quoted.uri, &quoting_note(BOB, None)),
        bob,
    )
    .await;

    assert_handled(&outcome);
    assert_eq!(grants(&pool).await, 0, "nothing is granted");
    assert_eq!(
        deliveries(&pool).await[0]["type"],
        "Reject",
        "and the quoter is told so"
    );
}

/// A public post whose author chose `followers` is granted to an accepted
/// follower and refused to a stranger.
#[tokio::test]
async fn a_followers_policy_grants_followers_and_rejects_strangers() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote(&pool, "bob").await;
    let quoted = post_under(&pool, &alice, QuoteApprovalPolicy::Followers).await;
    let note = quoting_note(BOB, None);

    let stranger = handle_quote_request(
        app_state(&pool),
        request("q1", &quoted.uri, &note),
        bob.clone(),
    )
    .await;
    assert_handled(&stranger);
    assert_eq!(grants(&pool).await, 0, "a stranger is granted nothing");
    assert_eq!(deliveries(&pool).await[0]["type"], "Reject", "and is told");

    accepted_follow(&pool, &bob, &alice).await;
    let follower =
        handle_quote_request(app_state(&pool), request("q2", &quoted.uri, &note), bob).await;
    assert_handled(&follower);
    assert_eq!(grants(&pool).await, 1, "a follower is granted");
    assert_eq!(deliveries(&pool).await[1]["type"], "Accept", "and is told");
}

/// A direct post, and a post of an author who blocks the quoter, are
/// refused with a `Reject` and no grant.
#[tokio::test]
async fn direct_posts_and_blocked_quoters_are_rejected() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote(&pool, "bob").await;
    let direct = post(&pool, &alice, Visibility::Direct).await;
    let public = post(&pool, &alice, Visibility::Public).await;
    let note = quoting_note(BOB, None);

    let first = handle_quote_request(
        app_state(&pool),
        request("q1", &direct.uri, &note),
        bob.clone(),
    )
    .await;
    assert_handled(&first);
    let _block = block::block(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .expect("the block writes");
    let second =
        handle_quote_request(app_state(&pool), request("q2", &public.uri, &note), bob).await;
    assert_handled(&second);

    assert_eq!(grants(&pool).await, 0, "neither request is granted");
    let sent = deliveries(&pool).await;
    assert_eq!(sent.len(), 2, "both are answered");
    assert!(
        sent.iter().all(|document| document["type"] == "Reject"),
        "with a refusal"
    );
}

/// A grant that was withdrawn is not restated: the same request is
/// rejected from then on.
#[tokio::test]
async fn a_revoked_grant_is_rejected() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote(&pool, "bob").await;
    let quoted = post(&pool, &alice, Visibility::Public).await;
    let note = quoting_note(BOB, None);
    let first = handle_quote_request(
        app_state(&pool),
        request("q1", &quoted.uri, &note),
        bob.clone(),
    )
    .await;
    assert_handled(&first);
    let grant_id = sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM quote_authorizations")
        .fetch_one(&pool)
        .await
        .expect("the grant id reads");
    let revoked = quote_authorization::revoke(&pool, grant_id)
        .await
        .expect("the revoke runs");
    assert!(revoked, "the grant stood");

    let again =
        handle_quote_request(app_state(&pool), request("q2", &quoted.uri, &note), bob).await;

    assert_handled(&again);
    assert_eq!(
        deliveries(&pool).await[1]["type"],
        "Reject",
        "the repeat is refused"
    );
}

/// An instrument that is not the signer's own post — another author, another
/// host, a bare uri naming somebody else's stored post, nothing at all — is
/// ignored: no grant and no answer.
#[tokio::test]
async fn an_instrument_that_is_not_the_signers_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote(&pool, "bob").await;
    let erin = remote(&pool, "erin").await;
    let quoted = post(&pool, &alice, Visibility::Public).await;
    let _theirs = mirrored(&pool, &erin, QUOTING).await;
    let foreign_host = json!({
        "id": "https://evil.example/objects/bob-quote",
        "type": "Note",
        "attributedTo": BOB,
        "to": [AS_PUBLIC],
        "content": "<p>not mine</p>",
    });
    let cases = [
        ("someone else's note", quoting_note(&erin.ap_id, None)),
        ("another host", foreign_host),
        ("a stored post of someone else", json!(QUOTING)),
        ("a document with no id", json!({"type": "Note"})),
        ("a number", json!(7)),
    ];

    for (n, (what, instrument)) in cases.iter().enumerate() {
        let outcome = handle_quote_request(
            app_state(&pool),
            request(&format!("q{n}"), &quoted.uri, instrument),
            bob.clone(),
        )
        .await;
        assert!(
            ignored(outcome).contains("instrument"),
            "{what} is ignored for its instrument"
        );
    }

    assert_eq!(grants(&pool).await, 0, "nothing is granted");
    assert!(deliveries(&pool).await.is_empty(), "and nothing is sent");
}

/// An object that is not a live post of this instance is ignored: a remote
/// post, an unknown local one, and a deleted one.
#[tokio::test]
async fn an_object_that_is_not_a_live_local_post_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote(&pool, "bob").await;
    let gone = post(&pool, &alice, Visibility::Public).await;
    let _tombstone = status::soft_delete(&pool, gone.id)
        .await
        .expect("the delete runs");
    let theirs = mirrored(&pool, &bob, "https://remote.example/objects/theirs").await;
    let note = quoting_note(BOB, None);

    for (n, object) in [
        theirs.uri.clone(),
        format!("{TEST_BASE_URL}/objects/not-stored"),
        gone.uri.clone(),
    ]
    .iter()
    .enumerate()
    {
        let outcome = handle_quote_request(
            app_state(&pool),
            request(&format!("q{n}"), object, &note),
            bob.clone(),
        )
        .await;
        assert!(
            ignored(outcome).contains("object"),
            "{object} is no live local status"
        );
    }

    assert_eq!(grants(&pool).await, 0, "nothing is granted");
    assert!(deliveries(&pool).await.is_empty(), "and nothing is sent");
}

/// A request signed by an account this instance owns folds nothing.
#[tokio::test]
async fn a_locally_signed_request_is_ignored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let carol = local_user(&pool, "carol").await;
    let quoted = post(&pool, &alice, Visibility::Public).await;

    let outcome = handle_quote_request(
        app_state(&pool),
        request("q1", &quoted.uri, &quoting_note(BOB, None)),
        carol,
    )
    .await;

    assert!(
        ignored(outcome).contains("owns"),
        "the door is for remote peers"
    );
    assert_eq!(grants(&pool).await, 0, "nothing is granted");
}

/// The live registry routes a delivered `QuoteRequest` to this handler:
/// the dispatcher's own entry point queues the grant's `Accept`.
#[tokio::test]
async fn the_dispatcher_routes_a_delivered_request_here() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local_user(&pool, "alice").await;
    let bob = remote(&pool, "bob").await;
    let quoted = post(&pool, &alice, Visibility::Public).await;
    let activity = json!({
        "id": "https://remote.example/activities/routed",
        "type": "QuoteRequest",
        "actor": BOB,
        "object": quoted.uri,
        "instrument": quoting_note(BOB, Some(&quoted.uri)),
    });
    let args = json!({"activity": activity, "signer_id": bob.id, "inbox_owner": null});

    let outcome = crate::federation::dispatch::handle_incoming(app_state(&pool), args).await;

    assert!(outcome.is_ok(), "the delivery completes");
    assert_eq!(grants(&pool).await, 1, "the handler stored the grant");
    assert_eq!(deliveries(&pool).await.len(), 1, "and answered the quoter");
}
