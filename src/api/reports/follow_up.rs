//! What a filed report owes once it is stored: the forwarding `Flag`, the
//! webhook event and the letters to the staff.

use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    mailer,
    models::{report::Report as ReportRow, user},
    services::{federation_out, webhooks},
    state::AppState,
};

/// Post-write hook of `POST /api/v1/reports`: three things happen once a
/// report is stored, in the order Pleroma does them in
/// `ActivityPub.flag/1` — forward, announce, write to the staff.
///
/// - [`federation_out::after_report_created`] forwards the report's `Flag` to
///   a REMOTE target's instance when the reporter asked for it (its delivery
///   queues through the job table, decision 11), and stays silent for a local
///   target or an unforwarded filing.
/// - [`webhooks::report_created`] queues the signed `report.created` event
///   for every enabled webhook subscribed to it (decision 11 again: the
///   delivery is a job, not part of this request).
/// - [`email_staff`] posts the report to the staff mailboxes.
///
/// The inbound federation `Flag` ([`crate::federation::handlers::misc`])
/// runs the same hook on the report it files, so whatever stands here
/// fires for locally filed and forwarded reports alike; the federating
/// half answers silence for the inbound one, whose target is by
/// construction a local account.
pub async fn after_report_created(state: &AppState, report: &ReportRow) {
    federation_out::after_report_created(state, report).await;
    webhooks::report_created(state, report).await;
    email_staff(state, report).await;
}

/// The two accounts a filed report names, as
/// [`report_to_admin`](mailer::templates::report_to_admin) addresses them.
#[derive(Debug)]
struct Parties {
    /// The nickname of the account that filed the report.
    reporter: String,
    /// The nickname of the account the report is about.
    target: String,
}

/// The staff letters a filed report owes: Pleroma's `AdminEmail.report`
/// ([`report_to_admin`](mailer::templates::report_to_admin) here) to every
/// local admin or moderator that left an address — the recipients of
/// Pleroma's `all_users_with_privilege(:reports_manage_reports)` filtered to
/// the mailable ones, with the reporter left out of its own report's letters
/// as Pleroma leaves it out.
///
/// While `SMTP_HOST` is unset nothing is queued at all
/// ([`mailer::deliver_later`] drops the letter), so the reads are skipped
/// rather than run for a letter that will never be written. A failure at any
/// step is logged and costs at most one recipient's letter: the report itself
/// is filed, and a missing notification has never been a reason to undo one.
async fn email_staff(state: &AppState, row: &ReportRow) {
    if state.config.smtp_host.is_none() {
        debug!(report = %row.id, "mail is off: no staff letter is queued for this report");
        return;
    }
    let staff = match user::admin::staff_with_email(&state.db).await {
        Ok(staff) => staff,
        Err(err) => {
            warn!(%err, report = %row.id, "the staff listing failed, no report letter is queued");
            return;
        }
    };
    if staff.is_empty() {
        debug!(report = %row.id, "the instance has no mailable staff account");
        return;
    }
    let Some(parties) = report_parties(state, row).await else {
        debug!(report = %row.id, "a report account is gone, no staff letter can name them");
        return;
    };
    for staffer in staff {
        if staffer.id == row.reporter_id {
            continue;
        }
        let Some(address) = staffer.email.as_deref() else {
            continue;
        };
        let letter = mailer::templates::report_to_admin(
            &state.config,
            address,
            &parties.reporter,
            &parties.target,
            &row.comment,
        );
        if let Err(err) = mailer::deliver_later(state, letter).await {
            warn!(%err, report = %row.id, "a staff report letter could not be queued");
        }
    }
}

/// The nicknames [`email_staff`] writes into a letter, `None` when either
/// account is already gone — a deletion cascade that beat the hook, and a
/// letter about accounts that no longer exist is not written.
async fn report_parties(state: &AppState, row: &ReportRow) -> Option<Parties> {
    let reporter = nickname_of(state, row.reporter_id).await?;
    let target = nickname_of(state, row.target_id).await?;
    Some(Parties { reporter, target })
}

/// The nickname of the account behind `id`, `None` when no account carries it
/// or the read failed.
async fn nickname_of(state: &AppState, id: Uuid) -> Option<String> {
    match user::get_by_id(&state.db, id).await {
        Ok(Some(account)) => Some(account.nickname),
        Ok(None) => None,
        Err(err) => {
            warn!(%err, account = %id, "the report letter's account could not be read");
            None
        }
    }
}

#[cfg(test)]
mod tests;
