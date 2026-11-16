//! The admin API's report entity: one filed report as the moderation
//! console sees it (WBE-200).
//!
//! The reference shape is Pleroma's `AdminAPI.ReportView`
//! (`lib/pleroma/web/admin_api/views/report_view.ex`), whose `"show.json"`
//! renders the report with its target under `account`, its reporter under
//! `actor`, the cited statuses as full [`Status`] entities and the
//! moderators' [`notes`](AdminReportNote) beside them — and whose
//! `"index.json"` wraps a page of them under `reports` with the filter's
//! `total` beside the list. The two account slots are the admin render
//! ([`AdminAccount`]), not the client [`Account`](crate::entities::account::Account):
//! a moderation console is owed the private columns. The ticket pins the
//! entity's keys — `id`, `state`, `account`, `actor`, `content`,
//! `created_at`, `statuses`, `notes` and `assigned_account` — and leaves
//! out Pleroma's `rules` array.
//!
//! The parts are read before the render (decision 3): [`AdminReport::from_parts`]
//! is pure, so the endpoint hands it the row, the three users it names,
//! the already-rendered statuses and the already-read notes. `content` is
//! the reporter's [`comment`](ReportRow::comment) verbatim (Pleroma's
//! `HTML.filter_tags` over the Flag's content, whose storage here is
//! already plain text), `created_at` is the filing instant under
//! [`inserted_at`](ReportRow::inserted_at) in the format of decision 5, and
//! `assigned_account` — the one key Pleroma answers as `null` — is
//! omitted when the report sits in the unassigned queue, as the ticket
//! pins.
//!
//! [`Status`]: crate::entities::status::Status
//! [`ReportRow`]: crate::models::report::Report

use serde::Serialize;

use crate::{
    config::Config,
    entities::{admin_account::AdminAccount, status::Status},
    models::{
        report::{Report as ReportRow, ReportNote as ReportNoteRow},
        user::User,
    },
    util::time::mastodon_time,
};

/// One moderator's note as the admin report entity renders it: Pleroma's
/// `"show_note.json"` with the note's text and instant.
///
/// The author rides under `user_id` — the ticket's spelling of the slot
/// `"show_note.json"` fills with a full account render — and the note's
/// instant under `created_at`.
#[derive(Clone, Debug, Serialize)]
pub struct AdminReportNote {
    /// The note row's `UUIDv7` as lowercase hyphenated text (decision 4).
    pub id: String,
    /// The writing moderator's id as text; `None` rendered as `null`, the
    /// column's no-author spelling no endpoint writes today.
    pub user_id: Option<String>,
    /// The note's text, verbatim.
    pub content: String,
    /// When the note was written, in the Mastodon time format (decision 5).
    pub created_at: String,
}

impl AdminReportNote {
    /// Renders one `report_notes` row.
    #[must_use]
    pub fn from_row(row: &ReportNoteRow) -> Self {
        Self {
            id: row.id.to_string(),
            user_id: row.user_id.map(|author| author.to_string()),
            content: row.content.clone(),
            created_at: mastodon_time(&row.inserted_at),
        }
    }
}

/// The parts [`AdminReport::from_parts`] renders one entity from: the row
/// and everything it names, already read.
///
/// The three users are separate because the entity gives each slot its
/// own name: `account` is the reported, `actor` the reporter,
/// `assigned_account` the moderator who owns the report.
#[derive(Debug)]
pub struct AdminReportParts<'a> {
    /// The report row.
    pub row: &'a ReportRow,
    /// The reported account: the entity's `account`.
    pub target: &'a User,
    /// The reporter: the entity's `actor`.
    pub reporter: &'a User,
    /// The moderator the report is assigned to: the entity's
    /// `assigned_account`, omitted when `None`.
    pub assigned: Option<&'a User>,
    /// The cited statuses, rendered (with the reading admin as viewer) and
    /// oldest first, as the entity's `statuses`.
    pub statuses: Vec<Status>,
    /// The moderators' notes, oldest first, as the entity's `notes`.
    pub notes: Vec<AdminReportNote>,
}

/// One report as the admin API renders it: Pleroma's
/// `AdminAPI.ReportView` `"show.json"`, the ticket's nine keys in the
/// ticket's order.
#[derive(Debug, Serialize)]
pub struct AdminReport {
    /// The row's `UUIDv7` as lowercase hyphenated text (decision 4).
    pub id: String,
    /// Where the report stands: `open`, `closed` or `resolved`.
    pub state: String,
    /// The reported account, as an admin sees it.
    pub account: AdminAccount,
    /// The reporting account, as an admin sees it.
    pub actor: AdminAccount,
    /// The reporter's own words, verbatim; `""` when they said nothing.
    pub content: String,
    /// When the report was filed, in the Mastodon time format (decision 5).
    pub created_at: String,
    /// The cited statuses, rendered with the reading admin as viewer —
    /// Pleroma's admin view runs no visibility filter, and neither does
    /// the endpoint that fills this.
    pub statuses: Vec<Status>,
    /// The moderators' notes, oldest first.
    pub notes: Vec<AdminReportNote>,
    /// The moderator who owns the report; omitted, not nulled, while the
    /// report sits in the unassigned queue (the ticket's pin).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_account: Option<AdminAccount>,
}

impl AdminReport {
    /// Renders one report from its parts under `config`.
    #[must_use]
    pub fn from_parts(parts: AdminReportParts<'_>, config: &Config) -> Self {
        let AdminReportParts {
            row,
            target,
            reporter,
            assigned,
            statuses,
            notes,
        } = parts;
        Self {
            id: row.id.to_string(),
            state: row.state.as_str().to_owned(),
            account: AdminAccount::from_user(target, config),
            actor: AdminAccount::from_user(reporter, config),
            content: row.comment.clone(),
            created_at: mastodon_time(&row.inserted_at),
            statuses,
            notes,
            assigned_account: assigned.map(|owner| AdminAccount::from_user(owner, config)),
        }
    }
}

/// The whole `GET /api/v1/pleroma/admin/reports` answer: Pleroma's
/// `"index.json"`, the page's entities under `reports` and the total the
/// filters select under `total`.
#[derive(Debug, Serialize)]
pub struct AdminReportPage {
    /// How many reports the filters match, pages included.
    pub total: i64,
    /// The page, newest filing first.
    pub reports: Vec<AdminReport>,
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            report::{Category, ReportState},
            user::{NewUser, insert, local_ap_id},
        },
        util::time::parse_time,
    };

    /// 2024-01-31T12:34:56.789Z, the instant every fixture row carries.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").expect("the fixture instant")
    }

    /// A local, unlocked user named `nickname`.
    async fn user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the fixture user inserts")
    }

    /// The report row the entity tests render: filed by `reporter` about
    /// `target`, assigned to nobody unless the caller says otherwise.
    fn row(reporter: &User, target: &User, assigned_to: Option<Uuid>) -> ReportRow {
        ReportRow {
            id: Uuid::parse_str("01934567-89ab-7cde-f012-3456789abcde").expect("fixture id"),
            reporter_id: reporter.id,
            target_id: target.id,
            comment: "harassment".to_owned(),
            category: Category::Violation,
            forwarded: false,
            state: ReportState::Open,
            assigned_to,
            action_taken_at: None,
            rule_ids: Vec::new(),
            ap_id: None,
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// One note row, authored by `author` at the fixture instant.
    fn note(author: Option<&User>) -> ReportNoteRow {
        ReportNoteRow {
            id: Uuid::parse_str("01934567-89ab-7cde-f012-3456789abcee").expect("fixture id"),
            report_id: Uuid::parse_str("01934567-89ab-7cde-f012-3456789abcde").expect("fixture id"),
            user_id: author.map(|found| found.id),
            content: "looked".to_owned(),
            inserted_at: stamped(),
        }
    }

    /// The entity's keys, sorted, for exact key-set assertions.
    fn json_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("reports render objects")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    /// The unassigned entity renders the ticket's eight keys — no
    /// `assigned_account` — with the two accounts in their admin shape.
    #[tokio::test]
    async fn the_entity_renders_the_pinned_keys() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "ar_alice").await;
        let bob = user(&pool, "ar_bob").await;
        let report = row(&alice, &bob, None);
        let entity = AdminReport::from_parts(
            AdminReportParts {
                row: &report,
                target: &bob,
                reporter: &alice,
                assigned: None,
                statuses: Vec::new(),
                notes: vec![AdminReportNote::from_row(&note(Some(&alice)))],
            },
            &Config::for_tests(),
        );
        let value = serde_json::to_value(entity).expect("the entity serializes");
        assert_eq!(
            json_keys(&value),
            vec![
                "account",
                "actor",
                "content",
                "created_at",
                "id",
                "notes",
                "state",
                "statuses",
            ],
            "the eight keys an unassigned report answers, no ninth"
        );
        assert_eq!(value["id"], report.id.to_string(), "the id as text");
        assert_eq!(value["state"], "open", "the stored state");
        assert_eq!(value["content"], "harassment", "the comment verbatim");
        assert_eq!(
            value["created_at"], "2024-01-31T12:34:56.789Z",
            "inserted_at in the Mastodon format"
        );
        assert_eq!(value["statuses"], json!([]), "no cited rows rendered");
        assert_eq!(value["notes"][0]["content"], "looked", "the note text");
        assert_eq!(
            value["notes"][0]["user_id"],
            json!(alice.id.to_string()),
            "the note author by id"
        );
        assert_eq!(value["notes"][0]["created_at"], "2024-01-31T12:34:56.789Z");
        assert_eq!(value["account"]["id"], bob.id.to_string(), "the target");
        assert!(
            value["account"].get("email").is_some() && value["actor"].get("roles").is_some(),
            "both accounts are the admin render"
        );
    }

    /// An assigned report carries the assignee's admin entity under
    /// `assigned_account`, completing the ticket's nine keys.
    #[tokio::test]
    async fn an_assigned_report_names_its_assignee() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let root = user(&pool, "ar_root").await;
        let bob = user(&pool, "ar_spam").await;
        let report = row(&bob, &bob, Some(root.id));
        let entity = AdminReport::from_parts(
            AdminReportParts {
                row: &report,
                target: &bob,
                reporter: &bob,
                assigned: Some(&root),
                statuses: Vec::new(),
                notes: Vec::new(),
            },
            &Config::for_tests(),
        );
        let value = serde_json::to_value(entity).expect("the entity serializes");
        assert_eq!(
            value["assigned_account"]["nickname"], "ar_root",
            "the assignee renders like the other two slots"
        );
    }

    /// The note renders a missing author as `null` rather than dropping
    /// the key: `user_id` is the row's column, not the endpoint's promise.
    #[test]
    fn a_note_without_an_author_nulls_the_slot() {
        let value =
            serde_json::to_value(AdminReportNote::from_row(&note(None))).expect("it serializes");
        assert_eq!(value["user_id"], json!(null), "null, not absent");
        assert_eq!(value["content"], "looked", "and the rest is the row");
    }

    /// The page answer is Pleroma's `"index.json"`: the `reports` array
    /// beside the filter's `total`.
    #[test]
    fn the_page_wraps_the_entities_with_the_total() {
        let page = AdminReportPage {
            total: 7,
            reports: Vec::new(),
        };
        let value = serde_json::to_value(page).expect("the page serializes");
        assert_eq!(
            value,
            json!({"total": 7, "reports": []}),
            "the two keys, the empty page spelled"
        );
    }

    /// The column defaults render as empties: an unassigned, comment-less,
    /// note-less report is still a full entity.
    #[tokio::test]
    async fn an_empty_report_renders_empties_not_nulls() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "ar_a").await;
        let bob = user(&pool, "ar_b").await;
        let filed = ReportRow {
            comment: String::new(),
            ..row(&alice, &bob, None)
        };
        let entity = AdminReport::from_parts(
            AdminReportParts {
                row: &filed,
                target: &bob,
                reporter: &alice,
                assigned: None,
                statuses: Vec::new(),
                notes: Vec::new(),
            },
            &Config::for_tests(),
        );
        let value = serde_json::to_value(entity).expect("the entity serializes");
        assert_eq!(value["content"], "", "no comment is empty, not null");
        assert_eq!(value["notes"], json!([]), "no notes is an empty array");
        assert!(value.get("assigned_account").is_none(), "still omitted");
    }
}
