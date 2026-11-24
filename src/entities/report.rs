//! The Mastodon `Report` entity: one filed report as the reporting client
//! sees it.
//!
//! The reference shape is the ticket's ten keys — `id`, `action_taken`,
//! `action_taken_at`, `category`, `comment`, `forwarded`, `created_at`,
//! `status_ids`, `rule_ids` and `target_account` — which is Mastodon's
//! `REST::Mastodon::ReportSerializer`. Pleroma 2.10's own
//! `ReportView.render("show.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/report_view.ex`) answers only `id`
//! and `action_taken`, so the entity is Mastodon's and the ticket pins it.
//! `id` is Pleroma's integer replaced by the row's `UUIDv7` as lowercase
//! hyphenated text (decision 4), and `created_at` is the row's
//! [`inserted_at`](ReportRow::inserted_at) under Mastodon's name in the
//! format of decision 5.
//!
//! Two keys are read off the row rather than stored beside it.
//! [`action_taken`](Report::action_taken) is
//! [`action_taken_at`](ReportRow::action_taken_at) as a flag — the schema has
//! one column, not a stamp and a boolean that could disagree with it — and
//! [`status_ids`](Report::status_ids) is what the caller read out of
//! `report_statuses` ([`statuses_for`](crate::models::report::statuses_for)),
//! because that context is a table, not a column.
//!
//! [`Report::from_row`] is pure (decision 3): it renders a
//! [`models::report`](crate::models::report) row, the [`User`] behind
//! [`target_id`](ReportRow::target_id) and the instance
//! [`Config`] — the target's [`Account`] is the whole nested entity, rendered
//! as `viewer` sees it.

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    config::Config,
    entities::account::{Account, AccountViewer},
    models::{report::Report as ReportRow, user::User},
};

/// The Mastodon `Report` entity.
///
/// Build one with [`Report::from_row`]; its [`Serialize`] impl emits the ten
/// keys in the order Mastodon names them.
#[derive(Debug, Serialize)]
pub struct Report {
    /// The report id as the hyphenated lowercase `UUIDv7` string (decision 4).
    pub id: String,
    /// Whether a moderator has acted on the report:
    /// [`action_taken_at`](ReportRow::action_taken_at) read as a flag, so the
    /// two can never disagree. A freshly filed report answers `false`.
    pub action_taken: bool,
    /// When moderation acted, in the Mastodon time format (decision 5);
    /// `null` while nothing has.
    #[serde(with = "crate::util::time::serde_mastodon_time::option")]
    pub action_taken_at: Option<DateTime<Utc>>,
    /// Why the reporter filed: `spam`, `legal`, `violation` or `other`.
    pub category: String,
    /// The reporter's own words, verbatim; `""` when they said nothing.
    pub comment: String,
    /// Whether the reporter asked for the report to be forwarded to the
    /// target's instance.
    pub forwarded: bool,
    /// When the report was filed, in the Mastodon time format: the row's
    /// [`inserted_at`](ReportRow::inserted_at) under Mastodon's name.
    #[serde(with = "crate::util::time::serde_mastodon_time")]
    pub created_at: DateTime<Utc>,
    /// The live statuses the report cites, oldest first, as id strings.
    pub status_ids: Vec<String>,
    /// The instance rules the reporter cited, in the order it cited them.
    pub rule_ids: Vec<String>,
    /// The reported account, as `viewer` sees it.
    pub target_account: Account,
}

impl Report {
    /// Renders `row` as the entity: `target` (the account
    /// [`target_id`](ReportRow::target_id) names, which the caller has
    /// already fetched) as [`target_account`](Self::target_account) seen by
    /// `viewer`, and `status_ids` — read back with
    /// [`statuses_for`](crate::models::report::statuses_for) — as the
    /// [`status_ids`](Self::status_ids) array.
    #[must_use]
    pub fn from_row(
        row: &ReportRow,
        target: &User,
        status_ids: &[Uuid],
        config: &Config,
        viewer: AccountViewer<'_>,
    ) -> Self {
        Self {
            id: row.id.to_string(),
            action_taken: row.action_taken_at.is_some(),
            action_taken_at: row.action_taken_at,
            category: row.category.as_str().to_owned(),
            comment: row.comment.clone(),
            forwarded: row.forwarded,
            created_at: row.inserted_at,
            status_ids: status_ids.iter().map(ToString::to_string).collect(),
            rule_ids: row.rule_ids.iter().map(ToString::to_string).collect(),
            target_account: Account::from_user_for(viewer, target, config, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::{
            report::{Category, ReportState},
            user::Flag,
        },
        test_fixtures::json_keys,
        util::time::parse_time,
    };

    /// The instant every fixture row in these tests is stamped with.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").expect("the fixture timestamp parses")
    }

    /// `text` as an id, or panic: the fixture literals are ids.
    fn id(text: &str) -> Uuid {
        Uuid::parse_str(text).expect("the fixture id parses")
    }

    /// The report id, as a client sees it.
    const REPORT_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// The reported account's id.
    const TARGET_ID: &str = "01934567-89ab-7cde-f012-3456789abcd2";
    /// The reporter's id.
    const REPORTER_ID: &str = "01934567-89ab-7cde-f012-3456789abcd1";
    /// The cited rule's id.
    const RULE_ID: &str = "01934567-89ab-7cde-f012-3456789abcd3";
    /// The cited status's id.
    const STATUS_ID: &str = "01934567-89ab-7cde-f012-3456789abcd4";

    /// Every JSON key the entity must emit, sorted.
    const EXPECTED_KEYS: [&str; 10] = [
        "action_taken",
        "action_taken_at",
        "category",
        "comment",
        "created_at",
        "forwarded",
        "id",
        "rule_ids",
        "status_ids",
        "target_account",
    ];

    /// A freshly filed report: nothing done about it, nothing assigned, one
    /// rule and one status cited.
    fn row() -> ReportRow {
        ReportRow {
            id: id(REPORT_ID),
            reporter_id: id(REPORTER_ID),
            target_id: id(TARGET_ID),
            comment: "harassment in my replies".to_owned(),
            category: Category::Violation,
            forwarded: true,
            state: ReportState::Open,
            assigned_to: None,
            action_taken_at: None,
            action_taken_by: None,
            rule_ids: vec![id(RULE_ID)],
            ap_id: Some(format!("{TEST_BASE_URL}/activities/filing")),
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// The reported account: a remote `Person` at the field defaults a freshly
    /// federated mirror carries, with counts to hide.
    fn target() -> User {
        User {
            id: id(TARGET_ID),
            nickname: "mallory@remote.example".to_owned(),
            ap_id: "https://remote.example/users/mallory".to_owned(),
            local: Flag::new(false),
            domain: Some("remote.example".to_owned()),
            actor_type: "Person".to_owned(),
            email: None,
            password_hash: None,
            name: "Mallory".to_owned(),
            bio: String::new(),
            raw_bio: String::new(),
            avatar_url: None,
            banner_url: None,
            profile_url: None,
            locked: Flag::new(false),
            bot: Flag::new(false),
            discoverable: Flag::new(false),
            fields: json!([]),
            raw_fields: json!([]),
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            is_admin: Flag::new(false),
            is_moderator: Flag::new(false),
            is_active: Flag::new(true),
            hide_followers: Flag::new(false),
            hide_follows: Flag::new(false),
            hide_followers_count: Flag::new(true),
            hide_follows_count: Flag::new(true),
            hide_favorites: Flag::new(true),
            allow_following_move: Flag::new(true),
            skip_thread_containment: Flag::new(false),
            accepts_chat_messages: Flag::new(true),
            show_role: Flag::new(true),
            no_rich_text: Flag::new(false),
            default_scope: "public".to_owned(),
            sensitive_default: Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            notification_settings: json!({}),
            mfa_settings: crate::models::user::mfa::MfaSettings::default_value(),
            background_image_url: None,
            birthday: None,
            show_birthday: Flag::new(false),
            also_known_as: Vec::new(),
            moved_to_ap_id: None,
            last_move_at: None,
            email_confirmed: Flag::new(true),
            approval_pending: Flag::new(false),
            confirmation_token: None,
            confirmation_sent_at: None,
            registration_reason: None,
            last_active_at: None,
            followers_count: 7,
            following_count: 3,
            statuses_count: 11,
            last_status_at: None,
            last_fetched_at: None,
            inserted_at: stamped(),
            updated_at: stamped(),
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            email_notifications: Flag::new(true),
            deleted_at: None,
        }
    }

    /// The instance configuration an entity render needs: the test origin.
    fn config() -> Config {
        Config::for_tests()
    }

    /// `row` rendered with the one fixture status cited, as an anonymous
    /// viewer sees it.
    fn render(row: &ReportRow) -> Value {
        serde_json::to_value(Report::from_row(
            row,
            &target(),
            &[id(STATUS_ID)],
            &config(),
            None,
        ))
        .expect("an entity serializes")
    }

    #[test]
    fn a_fresh_report_renders_the_mastodon_entity() {
        let value = render(&row());

        assert_eq!(json_keys(&value), EXPECTED_KEYS, "exactly the keys");
        assert_eq!(value["id"], REPORT_ID, "the id as hyphenated text");
        assert_eq!(value["action_taken"], false, "nothing has been done");
        assert_eq!(value["action_taken_at"], Value::Null, "and no stamp");
        assert_eq!(value["category"], "violation", "the stored category");
        assert_eq!(
            value["comment"], "harassment in my replies",
            "the reporter's words verbatim"
        );
        assert_eq!(value["forwarded"], true, "the stored flag");
        assert_eq!(
            value["created_at"], "2024-01-31T12:34:56.789Z",
            "inserted_at in the Mastodon format"
        );
        assert_eq!(
            value["status_ids"],
            json!([STATUS_ID]),
            "the cited statuses as id strings"
        );
        assert_eq!(
            value["rule_ids"],
            json!([RULE_ID]),
            "the cited rules as id strings"
        );
        assert_eq!(
            value["target_account"]["id"], TARGET_ID,
            "the target renders as an Account of its own"
        );
        assert!(
            value["target_account"]["pleroma"].is_object(),
            "and it is the full entity, not a stub"
        );
    }

    #[test]
    fn the_column_defaults_render_as_empties_not_nulls() {
        let filed = ReportRow {
            comment: String::new(),
            category: Category::Other,
            forwarded: false,
            rule_ids: Vec::new(),
            ..row()
        };
        let value = Report::from_row(&filed, &target(), &[], &config(), None);
        let value = serde_json::to_value(value).expect("an entity serializes");

        assert_eq!(value["comment"], "", "an empty comment is empty, not null");
        assert_eq!(value["category"], "other", "the default category");
        assert_eq!(value["forwarded"], false, "the default flag");
        assert_eq!(
            value["status_ids"],
            Value::Array(Vec::new()),
            "no context is an empty array"
        );
        assert_eq!(
            value["rule_ids"],
            Value::Array(Vec::new()),
            "no rules is an empty array"
        );
    }

    #[test]
    fn every_category_renders_as_the_column_spells_it() {
        for category in Category::ALL {
            let filed = ReportRow { category, ..row() };
            let value =
                serde_json::to_value(Report::from_row(&filed, &target(), &[], &config(), None))
                    .expect("an entity serializes");
            assert_eq!(
                value["category"],
                json!(category.as_str()),
                "`{category}` goes through verbatim"
            );
        }
    }

    #[test]
    fn action_taken_is_the_stamp_read_as_a_flag() {
        let acted = ReportRow {
            action_taken_at: Some(stamped() + Duration::days(1)),
            state: ReportState::Resolved,
            ..row()
        };
        let value = render(&acted);

        assert_eq!(value["action_taken"], true, "a stamp is an action");
        assert_eq!(
            value["action_taken_at"], "2024-02-01T12:34:56.789Z",
            "and the stamp itself is in the Mastodon format"
        );
    }

    #[test]
    fn the_target_renders_as_the_viewer_sees_it() {
        // The account in the entity is a full render, not a snapshot: an
        // account that hides its counts shows them to itself and `0` to a
        // stranger, and the report entity follows whoever asked.
        let mallory = target();
        let about_mallory = row();
        let stranger = render(&about_mallory);
        let owner = serde_json::to_value(Report::from_row(
            &about_mallory,
            &mallory,
            &[],
            &config(),
            Some(&mallory),
        ))
        .expect("an entity serializes");

        assert_eq!(
            stranger["target_account"]["followers_count"], 0,
            "a stranger sees the hidden count as zero"
        );
        assert_eq!(
            owner["target_account"]["followers_count"], 7,
            "and the account itself sees the real one"
        );
    }
}
