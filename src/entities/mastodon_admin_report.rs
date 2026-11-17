//! The Mastodon admin API's report entity: one filed report as
//! `GET /api/v1/admin/reports` and its routes answer it (WBE-214).
//!
//! The reference is Pleroma's `MastodonAPI.Admin.ReportView`
//! (`lib/pleroma/web/mastodon_api/admin/views/report_view.ex`), whose
//! `"show.json"` renders the report Mastodon's own admin shape: the
//! reporter under `account`, the reported account under `target_account`,
//! the moderator who owns the report and the one who acted on it beside
//! them — all four as [`AdminAccount`] renders, the moderation console's
//! account entity — with the cited [`Status`] entities and the cited
//! rules' `{id, text, hint}` entries in the arrays. The ticket pins the
//! entity's fourteen keys and their spellings; where Pleroma hardcodes
//! (`category: "other"`, `forwarded: false`, `action_taken_by_account:
//! nil`) this schema has the real column and the entity reads it.
//!
//! `action_taken` is the ticket's reading of the state — `resolved` or
//! `closed` — and `action_taken_at` is the column [`set_state`] stamps
//! with the resolution: a `closed` report is acted on without a stamp,
//! exactly as Mastodon's `close!` leaves `action_taken_at` null. The two
//! account slots the schema can leave unfilled, `assigned_account` and
//! `action_taken_by_account`, answer `null` rather than dropping the key:
//! the ticket spells both slots `AdminAccount|null`, where the Pleroma
//! admin entity omits its assignee while unassigned.
//!
//! The parts are read before the render (decision 3): [`from_parts`] is
//! pure, so the endpoint hands it the row, the four users it names, the
//! already-rendered statuses and the already-read rules. `created_at` is
//! the filing instant under [`inserted_at`](ReportRow::inserted_at) and
//! `updated_at` the row's last change, both in the format of decision 5.
//!
//! [`from_parts`]: MastodonAdminReport::from_parts
//! [`set_state`]: crate::models::report::set_state
//! [`Status`]: crate::entities::status::Status
//! [`AdminAccount`]: crate::entities::mastodon_admin::AdminAccount
//! [`ReportRow`]: crate::models::report::Report

use serde::Serialize;

use crate::{
    config::Config,
    entities::{mastodon_admin::AdminAccount, status::Status},
    models::{
        instance_rule::InstanceRule,
        report::{Report as ReportRow, ReportState},
        user::User,
    },
    util::time::mastodon_time,
};

/// One cited instance rule as the Mastodon admin report entity renders
/// it: the `{id, text, hint}` entry the ticket pins.
///
/// It is the same triple `GET /api/v1/instance`'s `rules` block states,
/// keyed by the rule's `UUIDv7` text (decision 4); Mastodon's own
/// `Admin::ReportSerializer` hangs the list beside the report.
#[derive(Clone, Debug, Serialize)]
pub struct MastodonAdminReportRule {
    /// The rule's primary key as lowercase hyphenated text.
    pub id: String,
    /// The rule text, verbatim.
    pub text: String,
    /// The explanatory hint, empty when the instance gave none.
    pub hint: String,
}

impl MastodonAdminReportRule {
    /// Renders one stored rule for this entity.
    #[must_use]
    pub fn from_rule(rule: &InstanceRule) -> Self {
        Self {
            id: rule.id.to_string(),
            text: rule.text.clone(),
            hint: rule.hint.clone(),
        }
    }
}

/// The parts [`MastodonAdminReport::from_parts`] renders one entity from:
/// the row and everything it names, already read.
///
/// The four users are separate because the entity gives each slot its own
/// name: `account` is the reporter, `target_account` the reported,
/// `assigned_account` the moderator who owns the report,
/// `action_taken_by_account` the one who resolved it.
#[derive(Debug)]
pub struct MastodonAdminReportParts<'a> {
    /// The report row.
    pub row: &'a ReportRow,
    /// The reporting account: the entity's `account`.
    pub reporter: &'a User,
    /// The reported account: the entity's `target_account`.
    pub target: &'a User,
    /// The moderator the report is assigned to: the entity's
    /// `assigned_account`, `null` when the report is unassigned.
    pub assigned: Option<&'a User>,
    /// The moderator who resolved the report: the entity's
    /// `action_taken_by_account`, `null` while no resolution named one.
    pub acted_by: Option<&'a User>,
    /// The cited statuses, rendered (with the reading staff member as
    /// viewer) and oldest first, as the entity's `statuses`.
    pub statuses: Vec<Status>,
    /// The cited rules, in the order the reporter cited them, as the
    /// entity's `rules`.
    pub rules: Vec<MastodonAdminReportRule>,
}

/// One report as the Mastodon admin API renders it: the ticket's
/// fourteen keys, Pleroma's `MastodonAPI.Admin.ReportView` `"show.json"`
/// read against this schema's real columns.
#[derive(Debug, Serialize)]
pub struct MastodonAdminReport {
    /// The row's `UUIDv7` as lowercase hyphenated text (decision 4).
    pub id: String,
    /// Whether moderation has acted: the ticket's reading of the state,
    /// `resolved` or `closed` (Pleroma's view spells the same fact
    /// `state != "open"`).
    pub action_taken: bool,
    /// When a moderator resolved the report, in the Mastodon time format
    /// (decision 5); `null` while none has — a closed report took no
    /// stamping action, and an open one has had none.
    pub action_taken_at: Option<String>,
    /// Why the report was filed: the row's [`Category`](ReportRow::category)
    /// text, where Pleroma's view hardcodes `"other"`.
    pub category: String,
    /// The reporter's own words, verbatim; `""` when they said nothing.
    pub comment: String,
    /// Whether the reporter asked to forward the report to the target's
    /// own instance, the row's column where Pleroma's view hardcodes
    /// `false`.
    pub forwarded: bool,
    /// When the report was filed, in the Mastodon time format.
    pub created_at: String,
    /// When the row last changed, likewise.
    pub updated_at: String,
    /// The reporting account, as an admin sees it.
    pub account: AdminAccount,
    /// The reported account, as an admin sees it.
    pub target_account: AdminAccount,
    /// The moderator who owns the report; `null` for the unassigned
    /// queue (the ticket's `AdminAccount|null`, where the Pleroma admin
    /// entity omits the key instead).
    pub assigned_account: Option<AdminAccount>,
    /// The moderator who resolved the report; `null` until a resolution
    /// names one — Pleroma's view answers this slot `nil` outright, the
    /// acting account having no column of its own there.
    pub action_taken_by_account: Option<AdminAccount>,
    /// The cited statuses, rendered with the reading staff member as
    /// viewer — Pleroma's admin view runs no visibility filter, and
    /// neither does the endpoint that fills this.
    pub statuses: Vec<Status>,
    /// The cited rules, in citation order; empty when the reporter cited
    /// none (Pleroma's `rules/1` reads the Flag's rule ids the same way,
    /// and this schema's rows carry the array on the row itself).
    pub rules: Vec<MastodonAdminReportRule>,
}

impl MastodonAdminReport {
    /// Renders one report from its parts under `config`.
    #[must_use]
    pub fn from_parts(parts: MastodonAdminReportParts<'_>, config: &Config) -> Self {
        let MastodonAdminReportParts {
            row,
            reporter,
            target,
            assigned,
            acted_by,
            statuses,
            rules,
        } = parts;
        Self {
            id: row.id.to_string(),
            action_taken: row.state != ReportState::Open,
            action_taken_at: row.action_taken_at.as_ref().map(mastodon_time),
            category: row.category.as_str().to_owned(),
            comment: row.comment.clone(),
            forwarded: row.forwarded,
            created_at: mastodon_time(&row.inserted_at),
            updated_at: mastodon_time(&row.updated_at),
            account: AdminAccount::from_user(reporter, config),
            target_account: AdminAccount::from_user(target, config),
            assigned_account: assigned.map(|owner| AdminAccount::from_user(owner, config)),
            action_taken_by_account: acted_by.map(|actor| AdminAccount::from_user(actor, config)),
            statuses,
            rules,
        }
    }
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
    /// `target`, unassigned and unacted unless the caller says otherwise.
    fn row(reporter: &User, target: &User) -> ReportRow {
        ReportRow {
            id: Uuid::parse_str("01934567-89ab-7cde-f012-3456789abcde").expect("fixture id"),
            reporter_id: reporter.id,
            target_id: target.id,
            comment: "harassment".to_owned(),
            category: Category::Violation,
            forwarded: true,
            state: ReportState::Open,
            assigned_to: None,
            action_taken_at: None,
            action_taken_by: None,
            rule_ids: Vec::new(),
            ap_id: None,
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// A stored rule, for the entity's `rules` array.
    fn rule(text: &str, hint: &str) -> InstanceRule {
        InstanceRule {
            id: Uuid::parse_str("01934567-89ab-7cde-f012-3456789abcdf").expect("fixture id"),
            text: text.to_owned(),
            hint: hint.to_owned(),
            priority: 1,
            inserted_at: stamped(),
            updated_at: stamped(),
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

    /// The unassigned, unacted entity renders the ticket's fourteen keys,
    /// both moderator slots spelled as `null`, and the state-derived
    /// `action_taken` false.
    #[tokio::test]
    async fn the_entity_renders_the_pinned_keys() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "mr_alice").await;
        let bob = user(&pool, "mr_bob").await;
        let report = row(&alice, &bob);
        let entity = MastodonAdminReport::from_parts(
            MastodonAdminReportParts {
                row: &report,
                reporter: &alice,
                target: &bob,
                assigned: None,
                acted_by: None,
                statuses: Vec::new(),
                rules: Vec::new(),
            },
            &Config::for_tests(),
        );
        let value = serde_json::to_value(entity).expect("the entity serializes");
        assert_eq!(
            json_keys(&value),
            vec![
                "account",
                "action_taken",
                "action_taken_at",
                "action_taken_by_account",
                "assigned_account",
                "category",
                "comment",
                "created_at",
                "forwarded",
                "id",
                "rules",
                "statuses",
                "target_account",
                "updated_at",
            ],
            "exactly the fourteen pinned keys"
        );
        assert_eq!(value["id"], report.id.to_string(), "the id as text");
        assert_eq!(value["action_taken"], false, "open is no action");
        assert_eq!(value["action_taken_at"], json!(null), "null, not absent");
        assert_eq!(value["action_taken_by_account"], json!(null));
        assert_eq!(value["assigned_account"], json!(null), "and ditto");
        assert_eq!(value["category"], "violation", "the row's own category");
        assert_eq!(value["comment"], "harassment", "the comment verbatim");
        assert_eq!(value["forwarded"], true, "the row's own answer");
        assert_eq!(
            value["created_at"], "2024-01-31T12:34:56.789Z",
            "inserted_at in the Mastodon format"
        );
        assert_eq!(
            value["updated_at"], "2024-01-31T12:34:56.789Z",
            "and the row's stamp beside it"
        );
        assert_eq!(value["statuses"], json!([]), "no cited rows rendered");
        assert_eq!(value["rules"], json!([]), "no cited rules rendered");
        assert_eq!(value["account"]["id"], alice.id.to_string(), "the reporter");
        assert_eq!(
            value["target_account"]["id"],
            bob.id.to_string(),
            "the reported"
        );
        assert!(
            value["account"].get("role").is_some(),
            "the accounts are the Mastodon admin render, roles and all"
        );
    }

    /// A resolved, assigned report fills both moderator slots with
    /// [`AdminAccount`] entities and reads `action_taken` from its state.
    #[tokio::test]
    async fn an_acted_report_fills_both_moderator_slots() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let root = user(&pool, "mr_root").await;
        let moddie = user(&pool, "mr_moddie").await;
        let spammer = user(&pool, "mr_spam").await;
        let report = ReportRow {
            state: ReportState::Resolved,
            assigned_to: Some(moddie.id),
            action_taken_at: Some(stamped()),
            action_taken_by: Some(root.id),
            ..row(&spammer, &spammer)
        };
        let entity = MastodonAdminReport::from_parts(
            MastodonAdminReportParts {
                row: &report,
                reporter: &spammer,
                target: &spammer,
                assigned: Some(&moddie),
                acted_by: Some(&root),
                statuses: Vec::new(),
                rules: Vec::new(),
            },
            &Config::for_tests(),
        );
        let value = serde_json::to_value(entity).expect("the entity serializes");
        assert_eq!(value["action_taken"], true, "resolved is an action");
        assert_eq!(
            value["action_taken_at"], "2024-01-31T12:34:56.789Z",
            "the stamp in the ticket's format"
        );
        assert_eq!(
            value["assigned_account"]["id"],
            moddie.id.to_string(),
            "the assignee renders like the other slots"
        );
        assert_eq!(
            value["action_taken_by_account"]["id"],
            root.id.to_string(),
            "and so does the acting moderator"
        );
    }

    /// A closed report is acted on without a stamp: `action_taken` reads
    /// the state pair, `action_taken_at` stays the column it left alone.
    #[tokio::test]
    async fn a_closed_report_is_acted_but_unstamped() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "mr_a").await;
        let bob = user(&pool, "mr_b").await;
        let report = ReportRow {
            state: ReportState::Closed,
            ..row(&alice, &bob)
        };
        let entity = MastodonAdminReport::from_parts(
            MastodonAdminReportParts {
                row: &report,
                reporter: &alice,
                target: &bob,
                assigned: None,
                acted_by: None,
                statuses: Vec::new(),
                rules: Vec::new(),
            },
            &Config::for_tests(),
        );
        let value = serde_json::to_value(entity).expect("the entity serializes");
        assert_eq!(value["action_taken"], true, "closed is in the pair");
        assert_eq!(
            value["action_taken_at"],
            json!(null),
            "and closing stamps nothing"
        );
    }

    /// The cited rules render as `{id, text, hint}` entries, in the order
    /// the reporter cited them, and the rule's own entity keys are the
    /// ticket's three.
    #[test]
    fn the_cited_rules_render_their_three_keys() {
        let cited = rule("No harassment", "keep it civil");
        let entity = MastodonAdminReportRule::from_rule(&cited);
        let value = serde_json::to_value(entity).expect("the rule serializes");
        assert_eq!(
            json_keys(&value),
            vec!["hint", "id", "text"],
            "exactly the three pinned keys"
        );
        assert_eq!(value["id"], cited.id.to_string(), "the id as text");
        assert_eq!(value["text"], "No harassment", "the text verbatim");
        assert_eq!(value["hint"], "keep it civil", "and the hint");
    }
}
