//! The admin API's JSON shapes: the moderation-log page, the stats
//! counters and the registration-invite entity behind the `users/*`
//! invite endpoints (WBE-199).
//!
//! Pleroma renders all of them through
//! `Pleroma.Web.AdminAPI.ModerationLogView`
//! (`lib/pleroma/web/admin_api/views/moderation_log_view.ex`), the
//! inline map of `AdminAPIController.stats` and
//! `Pleroma.Web.AdminAPI.InviteView`
//! (`lib/pleroma/web/admin_api/views/invite_view.ex`). Like that view, a
//! log entry answers the four keys `show.json` names: the row `id`
//! (hyphenated `UUIDv7` text per decision 4), the stored `data` map
//! verbatim, `inserted_at` as unix seconds, and the `message` sentence —
//! stored as built here, where Pleroma re-derives it on every read. The
//! stats answer is the `status_visibility` block over
//! [`crate::models::status`]'s live counters, and the invite entity is
//! the seven keys `InviteView` takes off the row.

use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;

use crate::models::{invite_token::InviteToken, moderation_log, status::StatusVisibilityCounts};

/// One entry of the moderation log listing, Pleroma's
/// `ModerationLogView` `"show.json"`.
#[derive(Debug, Serialize)]
pub struct ModerationLogEntry {
    /// The row's `UUIDv7` as lowercase hyphenated text (decision 4).
    pub id: String,
    /// The stored data map, verbatim: `actor`, `action`, `message` and the
    /// action-specific subject and extras
    /// ([`crate::models::moderation_log::record`]).
    pub data: Value,
    /// `inserted_at` as unix epoch seconds — Pleroma's
    /// `DateTime.to_unix`.
    pub time: i64,
    /// The full sentence of the action, as stored.
    pub message: String,
}

/// The whole `GET /api/v1/pleroma/admin/moderation_log` answer: one
/// newest-first page and the total the filters select.
#[derive(Debug, Serialize)]
pub struct ModerationLogPage {
    /// The page's entries, newest first.
    pub items: Vec<ModerationLogEntry>,
    /// How many entries match the filters, pages included.
    pub total: i64,
}

/// The four visibility counters, the shape Pleroma's `CounterCache` row
/// answers them in: the Mastodon names, as JSON numbers.
#[derive(Debug, Serialize)]
pub struct StatusVisibility {
    /// Live, non-reblog `public` statuses of the selected audience.
    pub public: i64,
    /// Live, non-reblog `unlisted` statuses of the selected audience.
    pub unlisted: i64,
    /// Live, non-reblog `private` statuses of the selected audience.
    pub private: i64,
    /// Live, non-reblog `direct` statuses of the selected audience.
    pub direct: i64,
}

/// The `GET /api/v1/pleroma/admin/stats` answer: the four visibility
/// counters under their one key, Pleroma's `%{"status_visibility" =>
/// counters}`.
#[derive(Debug, Serialize)]
pub struct AdminStats {
    /// The live-status counters of the selected audience.
    pub status_visibility: StatusVisibility,
}

impl ModerationLogEntry {
    /// Renders `row` in Pleroma's entry shape.
    #[must_use]
    pub fn from_row(row: &moderation_log::ModerationLog) -> Self {
        Self {
            id: row.id.to_string(),
            data: row.data.clone(),
            time: row.inserted_at.timestamp(),
            message: row.message.clone(),
        }
    }
}

impl ModerationLogPage {
    /// Renders one [`moderation_log::Page`] of the listing.
    #[must_use]
    pub fn from_page(page: &moderation_log::Page) -> Self {
        Self {
            items: page
                .items
                .iter()
                .map(ModerationLogEntry::from_row)
                .collect(),
            total: page.total,
        }
    }
}

impl AdminStats {
    /// Renders the four counters of one
    /// [`status_visibility_counts`](crate::models::status::status_visibility_counts)
    /// read.
    #[must_use]
    pub const fn from_counts(counts: &StatusVisibilityCounts) -> Self {
        Self {
            status_visibility: StatusVisibility {
                public: counts.public,
                unlisted: counts.unlisted,
                private: counts.private,
                direct: counts.direct,
            },
        }
    }
}

/// One registration invite, Pleroma's `InviteView` `"show.json"`.
///
/// The seven keys `Map.take` names, `created_at` omitted: `id` is the
/// `UUIDv7` as lowercase hyphenated text (decision 4), `expires_at` the
/// plain `YYYY-MM-DD` day or `null`, and `invite_type` the stored
/// snake-case text ([`InviteType::as_str`](crate::models::invite_token::InviteType::as_str)).
#[derive(Debug, Serialize)]
pub struct AdminInvite {
    /// The row's `UUIDv7` as text.
    pub id: String,
    /// The secret token text a registrant submits.
    pub token: String,
    /// Whether the invite is spent.
    pub used: bool,
    /// The last valid day, `None` rendered as `null`.
    pub expires_at: Option<NaiveDate>,
    /// Registrations served so far.
    pub uses: i32,
    /// The use cap, `None` rendered as `null`.
    pub max_use: Option<i32>,
    /// The lifecycle type, in its stored snake-case spelling.
    pub invite_type: &'static str,
}

impl AdminInvite {
    /// Renders one [`InviteToken`] row in Pleroma's invite shape.
    #[must_use]
    pub fn from_invite(invite: &InviteToken) -> Self {
        Self {
            id: invite.id.to_string(),
            token: invite.token.clone(),
            used: invite.used,
            expires_at: invite.expires_at,
            uses: invite.uses,
            max_use: invite.max_use,
            invite_type: invite.invite_type.as_str(),
        }
    }
}

/// The `GET /api/v1/pleroma/admin/users/invites` answer: Pleroma's
/// `InviteView` `"index.json"`, the entity list under its one `invites`
/// key.
#[derive(Debug, Serialize)]
pub struct InviteList {
    /// The invites, newest first.
    pub invites: Vec<AdminInvite>,
}

impl InviteList {
    /// Renders the invites in reverse creation order: the model lists
    /// oldest-first, the admin API answers newest-first.
    #[must_use]
    pub fn newest_first(invites: &[InviteToken]) -> Self {
        Self {
            invites: invites.iter().rev().map(AdminInvite::from_invite).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::models::invite_token::InviteType;

    /// A stored invite for the render tests.
    fn invite(
        invite_type: InviteType,
        expires_at: Option<NaiveDate>,
        max_use: Option<i32>,
    ) -> InviteToken {
        InviteToken {
            id: Uuid::parse_str("0198fc3a-0000-7000-8000-000000000042").expect("test uuid"),
            token: "secret-text".to_owned(),
            invite_type,
            expires_at,
            max_use,
            uses: 1,
            used: false,
            inserted_at: Utc.with_ymd_and_hms(2026, 1, 31, 12, 34, 56).unwrap(),
        }
    }

    /// The entity carries exactly Pleroma's seven keys, the nulls spelled
    /// as nulls.
    #[test]
    fn an_invite_renders_the_views_seven_keys() {
        let rendered = serde_json::to_value(AdminInvite::from_invite(&invite(
            InviteType::OneTime,
            None,
            None,
        )))
        .expect("the entity serializes");
        assert_eq!(
            rendered,
            json!({
                "id": "0198fc3a-0000-7000-8000-000000000042",
                "token": "secret-text",
                "used": false,
                "expires_at": null,
                "uses": 1,
                "max_use": null,
                "invite_type": "one_time",
            }),
            "no created_at, no surprises"
        );
    }

    /// A date-limited invite answers its `expires_at` as the plain
    /// `YYYY-MM-DD` day.
    #[test]
    fn an_expiry_renders_as_the_plain_day() {
        let day = NaiveDate::from_ymd_opt(2026, 12, 31).expect("a valid day");
        let rendered = serde_json::to_value(AdminInvite::from_invite(&invite(
            InviteType::ReusableDateLimited,
            Some(day),
            Some(5),
        )))
        .expect("the entity serializes");
        assert_eq!(rendered["expires_at"], json!("2026-12-31"), "the ISO day");
        assert_eq!(rendered["max_use"], json!(5), "and the cap");
        assert_eq!(rendered["invite_type"], json!("reusable_date_limited"));
    }

    /// The list wraps the entities under `invites`, reversed to newest
    /// first.
    #[test]
    fn the_list_answers_newest_first() {
        let older = invite(InviteType::OneTime, None, None);
        let newer = invite(InviteType::Reusable, None, Some(3));
        let list = serde_json::to_value(InviteList::newest_first(&[older, newer]))
            .expect("the list serializes");
        let entries = list["invites"].as_array().expect("the invites array");
        assert_eq!(entries.len(), 2, "both invites");
        assert_eq!(entries[0]["max_use"], json!(3), "the newer one first");
        assert_eq!(entries[1]["max_use"], json!(null), "then the older");
    }
}
