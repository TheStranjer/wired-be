//! The `Flag` a forwarded report owes a REMOTE target's instance: the
//! instance actor speaks it, the reported account and its cited statuses are
//! the object, and the target's inbox alone is the audience.
//!
//! A local target, an unforwarded filing and an id-less row announce
//! nothing; the row readers and the publisher hand are the root's.

use tracing::warn;
use uuid::Uuid;

use super::{load_user, queue_activity};
use crate::{
    federation::activities::{FlagContext, flag_json},
    models::{
        report::{self, Report as ReportRow},
        status,
        user::User,
    },
    services::instance_actor,
    state::AppState,
};

/// Forward the report `POST /api/v1/reports` (or the inbound `Flag`
/// handler) just filed, when the reporter asked for it and the target
/// lives on another instance.
///
/// The document is [`flag_json`]'s, Pleroma's `Flag` reduced to the fields
/// the ticket pins: the INSTANCE actor speaks it (the reporter's own
/// instance vouches on the wire, which is who the target's moderators must
/// hear from), `object` names the reported account first and the cited
/// live statuses' object uris after it, and `content` carries the
/// reporter's comment. Its id is the report row's stored `ap_id` (decision
/// 6 — the id [`report::create`] minted for exactly this document), so the
/// per-inbox delivery dedup swallows a repeat filing's redelivery.
///
/// Nothing goes out when the reporter asked for no forwarding or the
/// target is one of our own accounts — those facts stay with our
/// moderators — and the audience of a forwarded remote one is the target's
/// inbox alone: `to` names the target's actor id, which the publisher
/// resolves to their personal inbox, and the fan-out never touches
/// followers (the publishing actor is the instance actor anyway).
pub async fn after_report_created(state: &AppState, report: &ReportRow) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if !report.forwarded {
        return; // an unforwarded filing is this instance's own business
    }
    let Some(stored_id) = report.ap_id.as_deref() else {
        return; // an id-less row has no document to stand behind
    };
    let Some(target) = load_user(state, report.target_id).await else {
        return; // a gone target is nobody to report to
    };
    if target.local.get() {
        return; // a local target's moderators already have the row
    }
    let Some(instance_actor) = forwarding_actor(state).await else {
        return; // without the instance's own actor there is no signer
    };
    let object_ids = flag_object_ids(state, &target, report.id).await;
    let to = [target.ap_id.as_str()];
    let document = flag_json(&FlagContext {
        config: &state.config,
        id: stored_id,
        actor_ap_id: &instance_actor.ap_id,
        object_ids: &object_ids,
        content: &report.comment,
        to: &to,
        cc: &[],
        published: &report.inserted_at,
    });
    queue_activity(state, &instance_actor, &document, "report forward").await;
}

/// The instance actor's row, provisioned on first use: the account a
/// forwarded report is attributed to and signed as. A failure to read or
/// create it is logged and answers `None` — the filing stands, this
/// forward does not go out.
async fn forwarding_actor(state: &AppState) -> Option<User> {
    instance_actor::ensure_instance_actor(&state.db, &state.config)
        .await
        .map_err(|err| {
            warn!(%err, "could not provision the instance actor for a forwarded report");
        })
        .ok()
}

/// The `Flag` document's `object` array: the reported account's actor id
/// first, then the cited statuses' object uris, oldest first (the order
/// [`report::statuses_for`] keeps, which [`status::get_many`]’s id order
/// preserves). A citation deleted between the filing and the fan-out
/// drops out on its own; a read that fails is logged and leaves the array
/// naming the account alone — the report itself is worth forwarding even
/// with no context beside it.
async fn flag_object_ids(state: &AppState, target: &User, report_id: Uuid) -> Vec<String> {
    let mut objects = vec![target.ap_id.clone()];
    let cited = report::statuses_for(&state.db, report_id)
        .await
        .unwrap_or_else(|err| {
            warn!(%err, %report_id, "federation hook could not read the report's citations");
            Vec::new()
        });
    if cited.is_empty() {
        return objects;
    }
    let rows = status::get_many(&state.db, &cited)
        .await
        .unwrap_or_else(|err| {
            warn!(%err, %report_id, "federation hook could not read the cited statuses");
            Vec::new()
        });
    objects.extend(rows.into_iter().map(|row| row.uri));
    objects
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    use crate::{
        config::Config,
        models::status::Visibility,
        services::federation_out::test_support::{
            accepted_follow, app_state, delivered_inboxes, filed, filing, job_count, local,
            publish_jobs, remote, remote_status, run_publishes, the_single_activity,
        },
        util::time::mastodon_time,
    };

    /// A forwarded report about a REMOTE account publishes the `Flag` from
    /// the instance actor: the stored activity id, the account first and
    /// the cited statuses after it in the object, the comment as content —
    /// and the fan-out lands in the target's personal inbox alone.
    #[tokio::test]
    async fn a_forwarded_report_publishes_the_flag_to_the_target_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let fan = remote(&pool, "fan").await;
        let first = remote_status(&pool, &bob, "flagged-1", Visibility::Public).await;
        let second = remote_status(&pool, &bob, "flagged-2", Visibility::Public).await;
        let row = filed(
            &pool,
            &alice,
            &bob,
            &filing("they will not stop", true, vec![second.id, first.id]),
        )
        .await;
        let instance_actor = instance_actor::ensure_instance_actor(&pool, &Config::for_tests())
            .await
            .expect("the instance actor provisions");
        accepted_follow(&pool, &fan, &instance_actor).await;
        let state = app_state(&pool);

        after_report_created(&state, &row).await;

        let jobs = publish_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the forward is announced once");
        assert!(jobs[0].1.is_none(), "reports never debounce");
        assert_eq!(
            jobs[0].0["actor_id"],
            instance_actor.id.to_string(),
            "the publish runs as the instance actor"
        );
        let activity = &jobs[0].0["activity"];
        assert_eq!(activity["type"], "Flag");
        assert_eq!(
            activity["id"],
            row.ap_id.clone().expect("a local filing carries its id"),
            "the document id is the report row's stored ap_id"
        );
        assert_eq!(activity["actor"], instance_actor.ap_id);
        assert_eq!(
            activity["object"],
            json!([bob.ap_id, first.uri, second.uri]),
            "the account leads the object list, then the citations oldest first"
        );
        assert_eq!(activity["content"], "they will not stop");
        assert_eq!(
            activity["to"],
            json!([bob.ap_id]),
            "the reported account is the only address"
        );
        assert_eq!(activity["cc"], json!([]));
        assert_eq!(activity["published"], mastodon_time(&row.inserted_at));

        run_publishes(&state, &pool).await;
        assert_eq!(
            delivered_inboxes(&pool).await,
            vec!["https://remote.example/users/bob/inbox"],
            "the target's personal inbox only — not even the instance actor's follower hears"
        );
    }

    /// A forwarded report that cited nothing still flags the account.
    #[tokio::test]
    async fn a_flag_without_citations_still_names_the_account() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let row = filed(&pool, &alice, &bob, &filing("spam", true, Vec::new())).await;
        let state = app_state(&pool);

        after_report_created(&state, &row).await;

        let activity = the_single_activity(&pool).await;
        assert_eq!(activity["type"], "Flag");
        assert_eq!(activity["object"], json!([bob.ap_id]));
        assert_eq!(activity["content"], "spam");
    }

    /// A report about a LOCAL account stays with our own moderators,
    /// forwarded or not.
    #[tokio::test]
    async fn a_report_about_a_local_account_publishes_no_flag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let forwarded = filed(&pool, &alice, &bob, &filing("noise", true, Vec::new())).await;
        let state = app_state(&pool);

        after_report_created(&state, &forwarded).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "a local target never sees an instance-to-instance Flag"
        );
    }

    /// A filing that asked for no forwarding federates nothing, even
    /// about a remote target.
    #[tokio::test]
    async fn an_unforwarded_report_publishes_no_flag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let row = filed(&pool, &alice, &bob, &filing("mine", false, Vec::new())).await;
        let state = app_state(&pool);

        after_report_created(&state, &row).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "the reporter chose not to tell the other instance"
        );
    }

    /// A report row with no stored `ap_id` — the inbound mirror's shape —
    /// has no document of ours to stand behind.
    #[tokio::test]
    async fn a_report_without_a_stored_id_publishes_no_flag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob").await;
        let mut row = filed(&pool, &alice, &bob, &filing("echo", true, Vec::new())).await;
        row.ap_id = None;
        let state = app_state(&pool);

        after_report_created(&state, &row).await;

        assert_eq!(job_count(&pool).await, 0, "an id-less row has no document");
    }
}
