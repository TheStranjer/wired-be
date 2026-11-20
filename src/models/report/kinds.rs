//! The two closed sets the `reports` row's text columns hold: [`Category`],
//! why the reporter filed, and [`ReportState`], where the moderation stands.
//! Both are stored as their lowercase variant names, exactly like
//! [`Visibility`](crate::models::status::Visibility) and
//! [`FollowState`](crate::models::follow::FollowState).

use std::fmt;
use std::str::FromStr;

/// Why the reporter filed a report.
///
/// Stored in the `category` text column as the enum's lowercase variant name
/// (`spam`, `legal`, `violation` or `other`, which is exactly what the
/// column's `CHECK` constraint allows). The four are Mastodon's
/// `REPORT_CATEGORIES`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Category {
    /// Unsolicited bulk or repetitive content.
    Spam,
    /// Content that breaks applicable law in the instance's jurisdiction.
    Legal,
    /// Content that breaks an instance rule (the report's
    /// [`rule_ids`](super::Report::rule_ids) name which).
    Violation,
    /// Anything else; the column default, and what an absent `category`
    /// becomes.
    Other,
}

/// The message [`FromStr`] answers for text the column would not accept.
const UNKNOWN_CATEGORY: &str = "unknown report category";

impl Category {
    /// Every category, in the order the column's `CHECK` constraint lists
    /// them.
    pub const ALL: [Self; 4] = [Self::Spam, Self::Legal, Self::Violation, Self::Other];

    /// The stored (and wire) text of this category: its lowercase name, which
    /// is what the `category` column holds and what the entity reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spam => "spam",
            Self::Legal => "legal",
            Self::Violation => "violation",
            Self::Other => "other",
        }
    }
}

/// [`as_str`](Self::as_str) is the text form, so `format!("{category}")` and
/// the database agree.
impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses the stored text back, case-sensitively: the column only ever holds
/// these four spellings and clients send them verbatim.
impl FromStr for Category {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "spam" => Ok(Self::Spam),
            "legal" => Ok(Self::Legal),
            "violation" => Ok(Self::Violation),
            "other" => Ok(Self::Other),
            _ => Err(UNKNOWN_CATEGORY),
        }
    }
}

/// Where a report stands with the moderators.
///
/// Stored in the `state` text column as the enum's lowercase variant name
/// (`open`, `closed` or `resolved`, which is exactly what the column's
/// `CHECK` constraint allows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ReportState {
    /// Filed and waiting: what [`create`](super::create) leaves a report at.
    Open,
    /// Dropped without action; nothing was done about the report.
    Closed,
    /// Action was taken, which is the moment [`set_state`](super::set_state)
    /// stamps [`action_taken_at`](super::Report::action_taken_at) with.
    Resolved,
}

/// The message [`FromStr`] answers for text the column would not accept.
const UNKNOWN_STATE: &str = "unknown report state";

impl ReportState {
    /// Every state, in the order the column's `CHECK` constraint lists them.
    pub const ALL: [Self; 3] = [Self::Open, Self::Closed, Self::Resolved];

    /// The stored (and wire) text of this state: its lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Resolved => "resolved",
        }
    }
}

/// [`as_str`](Self::as_str) is the text form, so `format!("{state}")` and the
/// database agree.
impl fmt::Display for ReportState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses the stored text back, case-sensitively: the column only ever holds
/// these three spellings and the admin endpoints send them verbatim.
impl FromStr for ReportState {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            "resolved" => Ok(Self::Resolved),
            _ => Err(UNKNOWN_STATE),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::TEST_BASE_URL,
        models::report::{NewReport, create, test_support::*},
    };

    use super::*;

    /// The two enums and the database agree on spellings: what [`as_str`]
    /// states is what the column holds, what [`FromStr`] accepts is exactly
    /// the `CHECK` constraint's set, and sqlx stores the same text.
    #[tokio::test]
    async fn the_text_enums_are_the_columns_own_set() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;

        for category in Category::ALL {
            assert_eq!(
                category.to_string(),
                category.as_str(),
                "Display is the stored text"
            );
            let parsed: Category = category
                .as_str()
                .parse()
                .expect("every stored name parses back");
            assert_eq!(parsed, category, "and comes back the same variant");

            let row = create(
                &pool,
                TEST_BASE_URL,
                &alice,
                &bob,
                &NewReport { category, ..bare() },
            )
            .await
            .expect("the fixture report files");
            assert_eq!(
                stored(&pool, row.id).await.category,
                category,
                "`{}` round-trips through the column",
                category.as_str()
            );
        }
        assert_eq!(
            report_count(&pool).await,
            i64::try_from(Category::ALL.len()).expect("four categories fit"),
            "one filing per category"
        );

        for state in ReportState::ALL {
            assert_eq!(state.to_string(), state.as_str(), "Display is text");
            let parsed: ReportState = state
                .as_str()
                .parse()
                .expect("every stored name parses back");
            assert_eq!(parsed, state, "and comes back the same variant");
        }

        for unknown in ["", "SPAM", "Violation", "whatever", "spam "] {
            assert!(
                unknown.parse::<Category>().is_err(),
                "`{unknown}` is not a category"
            );
        }
        for unknown in ["", "OPEN", "fixed", "other"] {
            assert!(
                unknown.parse::<ReportState>().is_err(),
                "`{unknown}` is not a state"
            );
        }
    }
}
