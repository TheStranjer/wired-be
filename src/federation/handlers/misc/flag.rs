//! The `Flag` route: [`handle_flag`] files the forwarded report a remote
//! instance sends about a local account — the object's local-account entry
//! names the target, the stored statuses among the rest are cited, and the
//! row lands under the activity's id.

use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::reports::after_report_created,
    error::AppResult,
    federation::{
        dispatch::{ActivityView, FederationError, Outcome},
        json,
    },
    models::{report, status, user::User},
    state::AppState,
};

use super::{find_local_account, transient};

/// The `Flag` route: file the forwarded report a remote instance sends
/// about a local account.
///
/// # Errors
///
/// [`FederationError::Transient`] when a database failure stopped the
/// report from being filed or its activity id recorded; a `Flag` naming no
/// local account is [`Outcome::Ignored`].
pub fn handle_flag(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> BoxFuture<'static, Result<Outcome, FederationError>> {
    Box::pin(async move { apply_flag(state, view, signer).await })
}

/// The body of [`handle_flag`]: find the local account among the object
/// ids, match the rest against stored statuses, file the forwarded report
/// under the activity's id, and run the filing's shared hook.
async fn apply_flag(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    if signer.local.get() {
        return Ok(Outcome::Ignored(
            "a Flag signed by an account this instance owns",
        ));
    }
    let objects = view
        .raw
        .get("object")
        .map_or_else(Vec::new, json::references);
    let account = find_local_account(&state.db, &objects)
        .await
        .map_err(|err| transient("report target", &err))?;
    let Some((account_index, target)) = account else {
        return Ok(Outcome::Ignored(
            "a Flag whose object names no local account",
        ));
    };
    let status_ids = cited_statuses(&state.db, &objects, account_index)
        .await
        .map_err(|err| transient("cited statuses", &err))?;
    let comment = view
        .raw
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let new = report::NewReport {
        comment,
        category: report::Category::Other,
        forwarded: true,
        rule_ids: Vec::new(),
        status_ids: status_ids.clone(),
    };
    let row = report::create(&state.db, &state.config.base_url, &signer, &target, &new)
        .await
        .map_err(|err| transient("report", &err))?;
    report::set_ap_id(&state.db, row.id, &view.id)
        .await
        .map_err(|err| transient("report activity id", &err))?;
    after_report_created(&state, &row).await;
    debug!(
        report = %row.id,
        target = %target.nickname,
        cited = status_ids.len(),
        "an inbound Flag filed the forwarded report of a local account"
    );
    Ok(Outcome::Handled)
}

/// The stored statuses a `Flag` cites: every object id but the reported
/// account's that is the `uri` of a status this instance keeps
/// ([`status::get_by_uri`], the uri-as-identity rule of decision 6). An id
/// nothing stores is silently dropped — the fetch ticket gives unknown
/// statuses a home later; a report's context only ever names what is here.
async fn cited_statuses(
    pool: &PgPool,
    ids: &[String],
    account_index: usize,
) -> AppResult<Vec<Uuid>> {
    let mut cited = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        if index == account_index {
            continue;
        }
        if let Some(stored) = status::get_by_uri(pool, id).await? {
            cited.push(stored.id);
        }
    }
    Ok(cited)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::federation::handlers::misc::test_support::*;

    /// The `Flag` route, full shape: the local account among the object ids
    /// is the target, the stored statuses among the rest are cited (an
    /// unknown uri drops out), and the row lands as the forwarded report of
    /// the delivered activity — reporter the remote signer, comment the
    /// `content`, the activity id mirrored onto it.
    #[tokio::test]
    async fn a_flag_files_the_forwarded_report_with_its_statuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let bob = local_user(&pool, "flag-bob").await;
        let _mallory = remote_at(&pool, "mallory", MALLORY).await;
        let cited_local = local_post(&pool, &bob, "the reported post").await;
        let cited_remote = remote_post(&pool, &alice, "flagged").await;

        let filed = handle_flag(
            state,
            view(
                "Flag",
                "flag-1",
                ALICE,
                json!({
                    "object": [
                        MALLORY,
                        bob.ap_id.clone(),
                        cited_local.uri.clone(),
                        "https://peer.example/posts/never-stored",
                        cited_remote.uri.clone(),
                    ],
                    "content": "harassment, please look",
                }),
            ),
            alice,
        )
        .await;
        assert_handled(&filed);

        let rows = stored_reports(&pool).await;
        assert_eq!(rows.len(), 1, "the Flag filed exactly one report");
        let row = &rows[0];
        assert_eq!(row.reporter_id, alice_id, "the remote actor reported");
        assert_eq!(row.target_id, bob.id, "about the local account");
        assert_eq!(row.comment, "harassment, please look", "with its words");
        assert!(row.forwarded, "a Flag delivered here is a forward");
        assert_eq!(row.category, report::Category::Other, "no category named");
        assert_eq!(
            row.ap_id.as_deref(),
            Some("https://peer.example/activities/flag-1"),
            "the row stands for the delivered activity"
        );
        let mut want = vec![cited_local.id, cited_remote.id];
        want.sort_unstable();
        let mut cited = report::statuses_for(&pool, row.id).await.unwrap();
        cited.sort_unstable();
        assert_eq!(cited, want, "both stored statuses matched by uri");
    }

    /// The `Flag` route, bare shape: an object that is a lone account id and
    /// no `content` still files the report — with nothing cited and the
    /// empty comment.
    #[tokio::test]
    async fn a_flag_of_a_lone_local_account_files_a_bare_report() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let bob = local_user(&pool, "bare-bob").await;

        let filed = handle_flag(
            state,
            object_view("Flag", "flag-2", ALICE, json!(bob.ap_id.clone())),
            alice,
        )
        .await;
        assert_handled(&filed);

        let rows = stored_reports(&pool).await;
        assert_eq!(rows.len(), 1, "one filing");
        assert_eq!(rows[0].comment, "", "no content is the empty comment");
        assert!(
            report::statuses_for(&pool, rows[0].id)
                .await
                .unwrap()
                .is_empty(),
            "and nothing cited"
        );
    }

    /// A `Flag` about a non-local account files nothing: neither a stored
    /// remote account nor unknown ids in the object list name a target this
    /// instance moderates.
    #[tokio::test]
    async fn a_flag_about_a_non_local_account_files_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = remote_signer(&pool).await;
        let alice_id = alice.id;
        let mallory = remote_at(&pool, "mallory", MALLORY).await;
        let their_post = remote_post(&pool, &mallory, "subject").await;

        let remote_subject = handle_flag(
            state.clone(),
            object_view(
                "Flag",
                "flag-3",
                ALICE,
                json!([mallory.ap_id.clone(), their_post.uri.clone()]),
            ),
            alice,
        )
        .await;
        assert!(
            ignored(remote_subject).contains("no local account"),
            "a report about another instance's account is that instance's"
        );

        let statuses_only = handle_flag(
            state,
            object_view("Flag", "flag-4", ALICE, json!([their_post.uri.clone()])),
            fresh_user(&pool, alice_id).await,
        )
        .await;
        assert!(
            ignored(statuses_only).contains("no local account"),
            "and a Flag that names no account at all"
        );
        assert!(
            stored_reports(&pool).await.is_empty(),
            "neither delivery filed a report"
        );
    }
}
