//! Scenario 1: the moderation flow walked through both admin report APIs.
//! A report filed over `POST /api/v1/reports` is read back through the
//! Pleroma admin surface, the Mastodon admin surface and the legacy
//! `/api/v0` twin; the notes, the assignment through both APIs and the
//! unassign through both are walked with their exact moderation sentences
//! — the resolve/reopen/resolve trio, the admin status edit and delete and
//! the deactivation leg live in
//! [`reports_enforcement`](super::reports_enforcement).

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};

use super::reports_enforcement::{
    admin_edits_and_deletes, deactivation_blinds_then_restores, resolve_reopen_resolve,
};
use super::support::{
    Account, M_ADMIN_REPORTS, P_ADMIN, REPORTS, V0_REPORTS, World, assert_log, get_array, id_of,
    post_status,
};
use crate::common;

/// Scenario 1, with scenario 4's moderation-surface half walked inline.
#[tokio::test]
async fn a_report_walks_both_admin_apis_and_the_staff_actions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let world = World::open(pool).await;
    let reporter = world.register("witness").await;
    let target = world.register("target").await;

    let flagged = post_status(&world, &target, "the reported post").await;
    let flagged_id = id_of(&flagged);
    let survivor = post_status(&world, &target, "the survivor").await;
    let survivor_id = id_of(&survivor);

    let filed = common::api_post(
        &world.app,
        Some(&reporter.token),
        REPORTS,
        &json!({
            "account_id": target.id,
            "status_ids": [flagged_id],
            "comment": "this is harassment",
            "category": "spam",
        }),
    )
    .await;
    common::assert_answer(&filed, StatusCode::OK, "the filing");
    let report_id = id_of(&filed.json_value());
    assert_eq!(
        filed.json_value()["action_taken"],
        json!(false),
        "a fresh report acts on nothing"
    );

    report_tellings(&world, &report_id, &reporter, &target).await;
    notes_assign_and_resolve(&world, &report_id).await;
    admin_edits_and_deletes(&world, &flagged_id, &survivor_id).await;
    deactivation_blinds_then_restores(&world, &target).await;
}

/// The one filing, told three ways: the Pleroma admin list entity, the
/// Mastodon admin list entity, and the legacy `/api/v0` twin's total.
async fn report_tellings(world: &World, report_id: &str, reporter: &Account, target: &Account) {
    let pleroma = listed_pleroma_report(&world.app, &world.moderator, report_id).await;
    assert_eq!(
        pleroma["content"],
        json!("this is harassment"),
        "the Pleroma comment"
    );
    assert_eq!(pleroma["state"], json!("open"), "a fresh report is open");
    assert_eq!(
        pleroma["actor"]["id"],
        json!(reporter.id),
        "the Pleroma actor is the reporter"
    );
    assert_eq!(
        pleroma["account"]["id"],
        json!(target.id),
        "the Pleroma account is the target"
    );
    assert_eq!(
        pleroma["statuses"].as_array().map(Vec::len),
        Some(1),
        "the cited status rides the admin entity"
    );

    let mastodon = get_array(&world.app, Some(&world.admin), M_ADMIN_REPORTS)
        .await
        .into_iter()
        .find(|row| row["id"] == json!(report_id))
        .expect("the filing shows in the Mastodon admin list");
    assert_eq!(
        mastodon["target_account"]["id"],
        json!(target.id),
        "the Mastodon target"
    );
    assert_eq!(
        mastodon["assigned_account"],
        json!(null),
        "the Mastodon queue starts unassigned"
    );
    assert_eq!(
        mastodon["statuses"].as_array().map(Vec::len),
        Some(1),
        "the Mastodon entity cites the status too"
    );

    let legacy = common::api_get(&world.app, Some(&world.admin), V0_REPORTS).await;
    common::assert_answer(&legacy, StatusCode::OK, "the legacy admin listing");
    assert!(
        legacy.json_value()["total"].as_i64() >= Some(1),
        "the legacy twin counts it"
    );
}

/// `GET {P_ADMIN}/reports?state=open` narrowed to `report_id`, asserted
/// present.
async fn listed_pleroma_report(app: &Router, token: &str, report_id: &str) -> Value {
    let response =
        common::api_get(app, Some(token), &format!("{P_ADMIN}/reports?state=open")).await;
    common::assert_answer(&response, StatusCode::OK, "the Pleroma admin listing");
    let body = response.json_value();
    let rows = body["reports"]
        .as_array()
        .expect("the admin page answers its reports array");
    rows.iter()
        .find(|row| row["id"].as_str() == Some(report_id))
        .cloned()
        .unwrap_or_else(|| panic!("the filing shows in the Pleroma admin list: {body}"))
}

/// The notes lifecycle, the assignment through both APIs, the unassign
/// through both, and the resolve/reopen/resolve walk — every move with its
/// exact moderation sentence.
async fn notes_assign_and_resolve(world: &World, report_id: &str) {
    let base = format!("{P_ADMIN}/reports");
    let note_head = format!("@moddy added note 'looked at it' to report ##{report_id}");
    let assigned_head =
        format!("@moddy assigned report ##{report_id} ( on user @target) to user moddy");
    let noted = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{base}/{report_id}/notes"),
        &json!({"content": "looked at it"}),
    )
    .await;
    common::assert_answer(&noted, StatusCode::NO_CONTENT, "the note write");
    let shown = common::api_get(
        &world.app,
        Some(&world.moderator),
        &format!("{base}/{report_id}"),
    )
    .await;
    let note_id = shown.json_value()["notes"][0]["id"]
        .as_str()
        .expect("the note carries its id")
        .to_owned();
    assert_log(
        &world.app,
        &world.admin,
        "report_note",
        &[format!("{note_head} on user @target")],
    )
    .await;

    let assigned = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{base}/assign_account"),
        &json!({"reports": [{"id": report_id, "assigned_account": "moddy"}]}),
    )
    .await;
    common::assert_answer(&assigned, StatusCode::OK, "the Pleroma assignment");
    assert_eq!(
        assigned.json_value()[0]["assigned_account"]["id"],
        json!(world.moderator_user.id.to_string()),
        "the Pleroma entity names its new owner"
    );

    let to_self = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{M_ADMIN_REPORTS}/{report_id}/assign_to_self"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&to_self, StatusCode::OK, "the Mastodon assign-to-self");
    assert_eq!(
        to_self.json_value()["assigned_account"]["id"],
        json!(world.moderator_user.id.to_string()),
        "the Mastodon entity names its owner too"
    );
    assert_log(
        &world.app,
        &world.admin,
        "report_assigned",
        &[assigned_head.clone(), assigned_head],
    )
    .await;

    unassign_report(world, report_id, &base).await;
    resolve_reopen_resolve(world, report_id).await;
    delete_report_note(world, report_id, &note_id, &base).await;
}

/// The unassignment through both admin APIs, logged once per move.
async fn unassign_report(world: &World, report_id: &str, base: &str) {
    let unassigned_head =
        format!("@moddy unassigned report ##{report_id} ( on user @target) from a user");
    let unassigned = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{base}/assign_account"),
        &json!({"reports": [{"id": report_id, "assigned_account": null}]}),
    )
    .await;
    common::assert_answer(&unassigned, StatusCode::OK, "the Pleroma unassignment");
    assert!(
        unassigned.json_value()[0].get("assigned_account").is_none(),
        "the key comes back off once unassigned: {}",
        unassigned.text()
    );
    let m_unassign = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{M_ADMIN_REPORTS}/{report_id}/unassign"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&m_unassign, StatusCode::OK, "the Mastodon unassign");
    assert_eq!(
        m_unassign.json_value()["assigned_account"],
        json!(null),
        "the Mastodon null"
    );
    assert_log(
        &world.app,
        &world.admin,
        "report_unassigned",
        &[unassigned_head.clone(), unassigned_head],
    )
    .await;
}

/// The note delete and its `report_note_delete` sentence.
async fn delete_report_note(world: &World, report_id: &str, note_id: &str, base: &str) {
    let removed = common::api_delete(
        &world.app,
        Some(&world.moderator),
        &format!("{base}/{report_id}/notes/{note_id}"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&removed, StatusCode::NO_CONTENT, "the note delete");
    assert_log(
        &world.app,
        &world.admin,
        "report_note_delete",
        &[format!(
            "@moddy deleted note 'looked at it' from report ##{report_id} on user @target"
        )],
    )
    .await;
}
