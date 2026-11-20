//! Scenario 1's enforcement half, walked by the admin and moderator
//! bearers: the Pleroma `PATCH` resolve, the Mastodon reopen that clears
//! the action stamp and the Mastodon resolve, each with its
//! `report_update` sentence and the state filter reading the closure
//! back; the admin statuses surface's re-scope and delete with their
//! `status_update` and `status_delete` lines; and the deactivation leg —
//! a fresh public post leaves the timeline while the account is
//! deactivated (its profile a 404, its bearer a 403, the admin god-mode
//! ledger still reaching the hidden post) and returns on reactivation,
//! both moves logged.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    Account, DISABLED, M_ADMIN_REPORTS, P_ADMIN, STATUSES, USERS, VERIFY, World, assert_log,
    carries, get_array, id_of, post_status, public_ids,
};
use crate::common;

/// The suspended account's own surfaces: the 404 profile, the 403 bearer,
/// and the admin god-mode ledger that still reaches the hidden post.
async fn assert_hidden_from_the_client(world: &World, target: &Account, fresh_id: &str) {
    let profile = common::api_get(
        &world.app,
        None,
        &format!("{}/{}", "/api/v1/accounts", target.id),
    )
    .await;
    common::assert_answer(
        &profile,
        StatusCode::NOT_FOUND,
        "the deactivated profile is a 404",
    );
    let refused = common::api_get(&world.app, Some(&target.token), VERIFY).await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "the disabled bearer is refused",
    );
    common::assert_error(&refused, DISABLED, "the disabled bearer is refused");
    assert!(
        carries(
            &get_array(
                &world.app,
                Some(&world.admin),
                &format!("{P_ADMIN}/statuses?godmode=true")
            )
            .await,
            "id",
            fresh_id,
        ),
        "the admin ledger still reaches the hidden post"
    );
}

/// The state-change trio: the Pleroma `PATCH` resolve, the Mastodon
/// reopen (which clears the action stamp), and the Mastodon resolve —
/// three `report_update` sentences, and the Pleroma entity reading
/// `resolved` again at the end.
pub(super) async fn resolve_reopen_resolve(world: &World, report_id: &str) {
    let patched = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{P_ADMIN}/reports"),
        &json!({"reports": [{"id": report_id, "state": "resolved"}]}),
    )
    .await;
    common::assert_answer(&patched, StatusCode::NO_CONTENT, "the Pleroma resolve");

    let reopened = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{M_ADMIN_REPORTS}/{report_id}/reopen"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&reopened, StatusCode::OK, "the Mastodon reopen");
    assert_eq!(
        reopened.json_value()["action_taken"],
        json!(false),
        "the reopen clears the action"
    );

    let resolved = common::api_post(
        &world.app,
        Some(&world.moderator),
        &format!("{M_ADMIN_REPORTS}/{report_id}/resolve"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&resolved, StatusCode::OK, "the Mastodon resolve");
    assert_eq!(
        resolved.json_value()["action_taken"],
        json!(true),
        "the resolve acts"
    );
    assert_eq!(
        resolved.json_value()["action_taken_by_account"]["id"],
        json!(world.moderator_user.id.to_string()),
        "the resolving staffer is named"
    );
    assert_log(
        &world.app,
        &world.admin,
        "report_update",
        &[
            format!("@moddy updated report ##{report_id} ( on user @target) with 'resolved' state"),
            format!("@moddy updated report ##{report_id} ( on user @target) with 'open' state"),
            format!("@root updated report ##{report_id} ( on user @target) with 'resolved' state"),
        ],
    )
    .await;

    let closed_page = common::api_get(
        &world.app,
        Some(&world.moderator),
        &format!("{P_ADMIN}/reports?state=resolved"),
    )
    .await;
    common::assert_answer(&closed_page, StatusCode::OK, "the resolved filter");
    assert!(
        closed_page.json_value()["reports"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["id"] == json!(report_id))),
        "the resolved report reads back through the state filter"
    );
}

/// The admin statuses surface's edit and delete: the reported status
/// re-scoped with its `status_update`, then the survivor deleted with its
/// `status_delete` and the client read after it a 404.
pub(super) async fn admin_edits_and_deletes(world: &World, reported_id: &str, survivor_id: &str) {
    let edited = common::api_put(
        &world.app,
        Some(&world.moderator),
        &format!("{P_ADMIN}/statuses/{reported_id}"),
        &json!({"sensitive": true, "visibility": "private"}),
    )
    .await;
    common::assert_answer(&edited, StatusCode::OK, "the admin status edit");
    assert_eq!(
        edited.json_value()["sensitive"],
        json!(true),
        "the entity reads sensitive"
    );
    assert_eq!(
        edited.json_value()["visibility"],
        json!("private"),
        "and private"
    );
    assert!(
        !public_ids(&world.app)
            .await
            .contains(&reported_id.to_owned()),
        "a re-scoped private post leaves the public timeline"
    );
    assert_log(
        &world.app,
        &world.admin,
        "status_update",
        &[format!(
            "@moddy updated status ##{reported_id}, set sensitive: 'true', visibility: 'private'"
        )],
    )
    .await;

    let deleted = common::api_delete(
        &world.app,
        Some(&world.moderator),
        &format!("{P_ADMIN}/statuses/{survivor_id}"),
        &Value::Null,
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the admin status delete");
    assert_eq!(
        deleted.json_value(),
        json!({}),
        "the delete answers the empty object"
    );
    let gone = common::api_get(&world.app, None, &format!("{STATUSES}/{survivor_id}")).await;
    common::assert_answer(&gone, StatusCode::NOT_FOUND, "the deleted status is a 404");
    assert_log(
        &world.app,
        &world.admin,
        "status_delete",
        &[format!("@moddy deleted status ##{survivor_id}")],
    )
    .await;
}

/// The deactivation leg: a fresh public post rides the timeline, the
/// account's deactivation takes it off (and 403s its own bearer and 404s
/// its profile) while the admin ledger still reaches it, and the
/// reactivation returns it — both moves logged.
pub(super) async fn deactivation_blinds_then_restores(world: &World, target: &Account) {
    let fresh = post_status(world, target, "the live one").await;
    let fresh_id = id_of(&fresh);
    assert!(
        public_ids(&world.app).await.contains(&fresh_id),
        "the fresh post rides the timeline"
    );

    let off = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{USERS}/deactivate"),
        &json!({"nicknames": [target.nickname]}),
    )
    .await;
    common::assert_answer(&off, StatusCode::OK, "the deactivation");
    assert_eq!(
        off.json_value()["users"][0]["is_active"],
        json!(false),
        "answered deactivated"
    );
    assert!(
        !public_ids(&world.app).await.contains(&fresh_id),
        "the deactivated author's post leaves the public timeline"
    );
    assert_hidden_from_the_client(world, target, &fresh_id).await;
    assert_log(
        &world.app,
        &world.admin,
        "deactivate",
        &["@root deactivated users: @target".to_owned()],
    )
    .await;

    let on = common::api_patch(
        &world.app,
        Some(&world.admin),
        &format!("{USERS}/activate"),
        &json!({"nicknames": [target.nickname]}),
    )
    .await;
    common::assert_answer(&on, StatusCode::OK, "the reactivation");
    assert!(
        public_ids(&world.app).await.contains(&fresh_id),
        "the reactivated author's post returns to the timeline"
    );
    let ok_again = common::api_get(&world.app, Some(&target.token), VERIFY).await;
    common::assert_answer(
        &ok_again,
        StatusCode::OK,
        "the released account reads itself",
    );
    assert_log(
        &world.app,
        &world.admin,
        "activate",
        &["@root activated users: @target".to_owned()],
    )
    .await;
}
