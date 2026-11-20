//! The umbrella `POST …/{id}/action` over HTTP (WBE-213): `disable` and
//! `suspend` with their flag move, sweep and entries, the types with nothing
//! under them, the `report_id` resolution and its guards, the two 422s of an
//! unusable `type`, and the urlencoded form of decision 10.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::support::NOT_FOUND;
use super::write_support::{
    DISABLED, INVALID_TYPE, MISSING_TYPE, WRONG_TARGET, account_action_path, act, file_against,
    letters, logged, login, one_entry, post_json, report_state, row_state, seed_writes, sweeps_for,
    token_count, verify,
};
use crate::common::{self, TestRequest};

/// `disable` is the deactivation and nothing more: the flag moves, the row is
/// not stamped for deletion, the credential survives (refused, but live), and
/// the request leaves both the `deactivate` entry of the move and the `disable`
/// entry of the ask.
#[tokio::test]
async fn the_action_disables_the_account_it_is_pointed_at() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let mine = common::create_app_and_token(&seeded.pool, &seeded.plain, "read").await;
    let client = common::create_app(&seeded.pool, "read").await;

    act(
        &seeded.app,
        &seeded.token,
        seeded.plain.id,
        &json!({"type": "disable"}),
    )
    .await;

    let (active, _, doomed) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is there");
    assert!(!active, "the flag moved");
    assert!(
        !doomed,
        "and no deletion was stamped — disable is not delete"
    );
    assert_eq!(
        token_count(&seeded.pool, &seeded.plain).await,
        1,
        "the token is left in place, refused until the account comes back"
    );
    let denied = verify(&seeded.app, &mine).await;
    common::assert_answer(&denied, StatusCode::FORBIDDEN, "its bearer's request");
    common::assert_error(&denied, DISABLED, "is the disabled refusal");
    let refused = login(
        &seeded.app,
        &client.client_id,
        &client.client_secret,
        "plain",
    )
    .await;
    common::assert_answer(&refused, StatusCode::FORBIDDEN, "and its sign-in");
    assert_eq!(
        refused.json_value()["identifier"],
        json!("account_is_disabled"),
        "with the grant's own reason code"
    );
    assert!(
        sweeps_for(&seeded.pool, &seeded.plain).await.is_empty(),
        "and no content sweep was queued"
    );

    let (message, subjects) = one_entry(&seeded.pool, "deactivate").await;
    assert_eq!(
        message, "@root deactivated users: @plain",
        "the move keeps the entry every other writer of the flag leaves"
    );
    assert_eq!(subjects, vec!["plain"]);
    let (asked, asked_subjects) = one_entry(&seeded.pool, "disable").await;
    assert_eq!(
        asked, "@root performed action disable",
        "and the request is recorded under the type it stated"
    );
    assert_eq!(asked_subjects, vec!["plain"], "naming its account");
}

/// `suspend` deactivates and queues the `users.delete` sweep, answering 200
/// `{}`; the three warning parameters the client sent alongside are dropped
/// without a trace, and the two entries name the move and the ask.
#[tokio::test]
async fn the_action_suspends_and_queues_the_content_sweep() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let mine = common::create_app_and_token(&seeded.pool, &seeded.plain, "read").await;

    act(
        &seeded.app,
        &seeded.token,
        seeded.plain.id,
        &json!({
            "type": "suspend",
            "text": "read the rules",
            "warning_preset_id": "7",
            "send_email_notification": true,
        }),
    )
    .await;

    let (active, _, doomed) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is still there for the sweep to work on");
    assert!(!active, "suspension deactivates");
    assert!(
        !doomed,
        "and queues the sweep instead of stamping the deletion itself"
    );
    let queued = sweeps_for(&seeded.pool, &seeded.plain).await;
    assert_eq!(queued.len(), 1, "the content sweep is on the queue");
    assert_eq!(
        queued[0]["user_id"],
        json!(seeded.plain.id.to_string()),
        "and names the account it will clear"
    );
    let denied = verify(&seeded.app, &mine).await;
    common::assert_answer(
        &denied,
        StatusCode::FORBIDDEN,
        "the suspended account's own request",
    );
    common::assert_error(&denied, DISABLED, "is refused as disabled");
    assert_eq!(
        token_count(&seeded.pool, &seeded.plain).await,
        1,
        "the row's own credential is the sweep's business, not this request's"
    );
    assert!(
        letters(&seeded.pool).await.is_empty(),
        "`send_email_notification` is accepted and ignored: there is no warning letter to send"
    );

    let (message, subjects) = one_entry(&seeded.pool, "deactivate").await;
    assert_eq!(
        message, "@root deactivated users: @plain",
        "the suspension's flag move is recorded as the deactivation it is"
    );
    assert_eq!(subjects, vec!["plain"]);
    let (asked, asked_subjects) = one_entry(&seeded.pool, "suspend").await;
    assert_eq!(asked, "@root performed action suspend");
    assert_eq!(
        asked_subjects,
        vec!["plain"],
        "and the ask is recorded by its type"
    );
}

/// `none`, `sensitive` and `silence` are the types with nothing under them:
/// this schema has no sensitive flag and no silence state, so each is the log
/// entry alone, and the account is left exactly as it stood.
#[tokio::test]
async fn the_unmodelled_action_types_are_recorded_alone() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;

    for kind in ["none", "sensitive", "silence"] {
        act(
            &seeded.app,
            &seeded.token,
            seeded.plain.id,
            &json!({"type": kind}),
        )
        .await;
        let (active, pending, doomed) = row_state(&seeded.pool, seeded.plain.id)
            .await
            .expect("the row is there");
        assert!(
            active && !pending && !doomed,
            "`{kind}` changed nothing on the account"
        );
        let (message, subjects) = one_entry(&seeded.pool, kind).await;
        assert_eq!(
            message,
            format!("@root performed action {kind}"),
            "Pleroma's fall-through sentence"
        );
        assert_eq!(subjects, vec!["plain"], "and the account it names");
    }

    assert!(
        logged(&seeded.pool, "deactivate").await.is_empty(),
        "and no type that moves nothing left a deactivation entry"
    );
    assert_eq!(
        logged(&seeded.pool, "none").await.len(),
        1,
        "one entry per request"
    );
}

/// A `report_id` on an action request is resolved as part of it: the row moves
/// to `resolved`, its action stamp lands, and the `report_update` entry and the
/// `report.updated` event are the ones every other state change writes.
#[tokio::test]
async fn the_action_resolves_the_report_it_names() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let report = file_against(&seeded.pool, &seeded.beeper, &seeded.plain).await;
    assert_eq!(
        report_state(&seeded.pool, report).await,
        ("open".to_owned(), false),
        "the fixture report is filed open"
    );

    act(
        &seeded.app,
        &seeded.token,
        seeded.plain.id,
        &json!({"type": "disable", "report_id": report.to_string()}),
    )
    .await;

    assert_eq!(
        report_state(&seeded.pool, report).await,
        ("resolved".to_owned(), true),
        "resolved, with its action stamp"
    );
    let entries = logged(&seeded.pool, "report_update").await;
    assert_eq!(entries.len(), 1, "one resolution entry for one report");
    assert_eq!(
        entries[0].0,
        format!("@root updated report ##{report} ( on user @plain) with 'resolved' state"),
        "the same sentence the console's state change writes, naming its target"
    );
    assert_eq!(
        entries[0].1["subject"]["id"],
        json!(report.to_string()),
        "whose subject is the report itself, Pleroma's report map"
    );
    let (asked, _) = one_entry(&seeded.pool, "disable").await;
    assert_eq!(
        asked, "@root performed action disable",
        "and the action itself is recorded beside the resolution"
    );
    let (moved, _) = one_entry(&seeded.pool, "deactivate").await;
    assert_eq!(
        moved, "@root deactivated users: @plain",
        "with the flag move"
    );
}

/// Pleroma's `associated_report` guard comes with the umbrella: an id that
/// names no report is the 404 the account half gives, and a report about
/// somebody else the 400 that says so — both before anything moves.
#[tokio::test]
async fn the_action_refuses_a_report_it_does_not_belong_to() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let theirs = file_against(&seeded.pool, &seeded.beeper, &seeded.asleep).await;
    let token = seeded.token.clone();

    let unknown = post_json(
        &seeded.app,
        &token,
        &account_action_path(seeded.plain.id, "action"),
        &json!({"type": "disable", "report_id": Uuid::now_v7().to_string()}),
    )
    .await;
    common::assert_answer(
        &unknown,
        StatusCode::NOT_FOUND,
        "a report that names nothing",
    );
    common::assert_error(&unknown, NOT_FOUND, "is the shared 404 body");

    let malformed = post_json(
        &seeded.app,
        &token,
        &account_action_path(seeded.plain.id, "action"),
        &json!({"type": "disable", "report_id": "nope"}),
    )
    .await;
    common::assert_answer(
        &malformed,
        StatusCode::NOT_FOUND,
        "text that is no report id",
    );

    let stranger = post_json(
        &seeded.app,
        &token,
        &account_action_path(seeded.plain.id, "action"),
        &json!({"type": "disable", "report_id": theirs.to_string()}),
    )
    .await;
    common::assert_answer(
        &stranger,
        StatusCode::BAD_REQUEST,
        "a report about another account",
    );
    common::assert_error(&stranger, WRONG_TARGET, "says so in Pleroma's words");

    let (active, _, _) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is there");
    assert!(active, "and the refused request touched nothing");
    assert_eq!(
        report_state(&seeded.pool, theirs).await,
        ("open".to_owned(), false),
        "least of all somebody else's report"
    );
    assert!(
        logged(&seeded.pool, "report_update").await.is_empty(),
        "and no resolution was recorded"
    );
    assert!(
        logged(&seeded.pool, "deactivate").await.is_empty()
            && logged(&seeded.pool, "disable").await.is_empty(),
        "nor the action's own entries"
    );
}

/// A `type` off the five words, and no `type` at all, are the two 422s the
/// endpoint answers a request it cannot name an action for — with the exact
/// bodies the ticket pins, and no effect on the account.
#[tokio::test]
async fn the_action_refuses_an_unusable_type() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let token = seeded.token.clone();
    let path = account_action_path(seeded.plain.id, "action");

    let invalid = post_json(&seeded.app, &token, &path, &json!({"type": "freeze"})).await;
    common::assert_answer(
        &invalid,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unknown type",
    );
    common::assert_error(
        &invalid,
        INVALID_TYPE,
        "Pleroma's enum refusal, spelled as it is",
    );

    let wrong_case = post_json(&seeded.app, &token, &path, &json!({"type": "Disable"})).await;
    common::assert_answer(
        &wrong_case,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the wrong case",
    );
    common::assert_error(&wrong_case, INVALID_TYPE, "is no more a type");

    let absent = post_json(&seeded.app, &token, &path, &json!({})).await;
    common::assert_answer(&absent, StatusCode::UNPROCESSABLE_ENTITY, "no type stated");
    common::assert_error(&absent, MISSING_TYPE, "is the required-key refusal");

    let number = post_json(&seeded.app, &token, &path, &json!({"type": 4})).await;
    common::assert_answer(
        &number,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a type that is no text",
    );

    let (active, _, _) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is there");
    assert!(active, "and none of the four refusals moved anything");
    for action in ["none", "disable", "suspend", "deactivate", "report_update"] {
        assert!(
            logged(&seeded.pool, action).await.is_empty(),
            "nor left a {action} entry"
        );
    }
}

/// Decision 10: the same request arrives urlencoded, with the ignored keys
/// beside the two that matter, and is answered the same — the umbrella's `type`
/// reaching the endpoint as form text.
#[tokio::test]
async fn the_action_reads_a_form_body() {
    let db = common::test_db().await;
    let seeded = seed_writes(&db).await;
    let fields = [
        ("type", "silence"),
        ("text", "shush"),
        ("warning_preset_id", "3"),
        ("send_email_notification", "true"),
    ];

    let response = TestRequest::post(&account_action_path(seeded.plain.id, "action"))
        .bearer(&seeded.token)
        .form(&fields)
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the form action");
    assert_eq!(response.json_value(), json!({}), "the same empty object");

    let (active, _, _) = row_state(&seeded.pool, seeded.plain.id)
        .await
        .expect("the row is there");
    assert!(active, "and `silence` silenced nothing but the request");
    let (message, subjects) = one_entry(&seeded.pool, "silence").await;
    assert_eq!(
        message, "@root performed action silence",
        "the form's type arrived as the umbrella's own word"
    );
    assert_eq!(subjects, vec!["plain"]);
}
