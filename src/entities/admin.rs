//! The admin API's JSON shapes (WBE-199, WBE-209, WBE-208, WBE-205,
//! WBE-203, WBE-201, WBE-204).
//!
//! They cover the moderation-log page, the stats counters, the invite,
//! webhook, instance-rule and ops entities, the OAuth-app and
//! instance-document answers, the relay ledger and the `{total,
//! activities}` envelope of the two narrower admin status listings.
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

use std::fmt;

use chrono::NaiveDate;
use serde::Serialize;
use serde::ser::SerializeMap as _;
use serde_json::Value;

use crate::{
    entities::status::Status,
    models::{
        instance_rule::InstanceRule, invite_token::InviteToken, moderation_log,
        oauth_app::OauthApp, relay::RelayListing, status::StatusVisibilityCounts, webhook::Webhook,
    },
};

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

/// The `{total, activities}` answer of the two narrower admin status
/// listings.
///
/// `GET /api/v1/pleroma/admin/users/{nickname}/statuses` and
/// `GET …/instances/{instance}/statuses` render Pleroma's
/// `AdminAPI.StatusView` `"index.json"` clause that carries a total,
/// `%{total: total, activities: activities}`
/// (`lib/pleroma/web/admin_api/views/status_view.ex`), the `activities`
/// being the very [`Status`] entities of the client API. The whole-network
/// `GET /statuses` answer is the bare array and has no page of its own.
#[derive(Debug, Serialize)]
pub struct AdminStatusPage {
    /// How many statuses the listing's filters select, pages included.
    pub total: i64,
    /// The page, newest post first.
    pub activities: Vec<Status>,
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

/// One webhook, the admin API's `show.json`.
///
/// Pleroma's `WebhookView` (`lib/pleroma/web/admin_api/views/webhook_view.ex`)
/// without its two timestamps — the entity is the six keys the ticket
/// names. `id` is the `UUIDv7` as lowercase hyphenated text (decision 4)
/// and `secret` the stored signing hex, answered to admins verbatim.
#[derive(Debug, Serialize)]
pub struct AdminWebhook {
    /// The row's `UUIDv7` as text.
    pub id: String,
    /// Where events are delivered.
    pub url: String,
    /// The subscribed event types.
    pub events: Vec<String>,
    /// The signing secret.
    pub secret: String,
    /// Whether deliveries happen at all.
    pub enabled: bool,
    /// Whether the instance itself installed this webhook.
    pub internal: bool,
}

impl AdminWebhook {
    /// Renders one [`Webhook`] row in the admin API's shape.
    #[must_use]
    pub fn from_webhook(webhook: &Webhook) -> Self {
        Self {
            id: webhook.id.to_string(),
            url: webhook.url.clone(),
            events: webhook.events.clone(),
            secret: webhook.secret.clone(),
            enabled: webhook.enabled,
            internal: webhook.internal,
        }
    }

    /// Renders a whole listing page, in the row order [`Webhook`]s come
    /// in — the id-ordered window the model reads.
    #[must_use]
    pub fn from_rows(webhooks: &[Webhook]) -> Vec<Self> {
        webhooks.iter().map(Self::from_webhook).collect()
    }
}

/// One instance rule, the admin API's `show.json`.
///
/// Pleroma's `AdminAPI.RuleView`
/// (`lib/pleroma/web/admin_api/views/rule_view.ex`) verbatim: the four
/// keys it takes off the row. `id` is the `UUIDv7` as lowercase hyphenated
/// text (decision 4), `hint` the stored guidance — empty when the instance
/// gave none — and `priority` the integer the listing sorts by.
#[derive(Debug, Serialize)]
pub struct AdminRule {
    /// The row's `UUIDv7` as text.
    pub id: String,
    /// The rule as clients display it.
    pub text: String,
    /// The guidance shown beside the rule.
    pub hint: String,
    /// The sort position: lower shows first.
    pub priority: i32,
}

impl AdminRule {
    /// Renders one [`InstanceRule`] row in the admin API's shape.
    #[must_use]
    pub fn from_rule(rule: &InstanceRule) -> Self {
        Self {
            id: rule.id.to_string(),
            text: rule.text.clone(),
            hint: rule.hint.clone(),
            priority: rule.priority,
        }
    }

    /// Renders a whole listing, in the row order the model read it — the
    /// priority-then-id order [`list`](crate::models::instance_rule::list)
    /// gives.
    #[must_use]
    pub fn from_rows(rules: &[InstanceRule]) -> Vec<Self> {
        rules.iter().map(Self::from_rule).collect()
    }
}

/// The `GET /api/v1/pleroma/admin/media_proxy_caches` answer: one page of
/// the served-URL ledger beside its window size and the filtered total.
///
/// Pleroma's `index.json` `{urls, page_size, count}`
/// (`lib/pleroma/web/admin_api/views/media_proxy_cache_view.ex`).
#[derive(Debug, Serialize)]
pub struct MediaProxyCachePage {
    /// The page's URLs, in `url` order.
    pub urls: Vec<String>,
    /// The effective `page_size` the window was cut with.
    pub page_size: i64,
    /// How many ledger entries the filter selects, pages included.
    pub count: i64,
}

/// The `POST /api/v1/pleroma/admin/media_proxy_caches/{delete,purge}`
/// answer: the ticket's `{"urls":[...]}` — the set the request named,
/// handed back as processed.
#[derive(Debug, Serialize)]
pub struct MediaProxyCacheUrls {
    /// The URLs the action took.
    pub urls: Vec<String>,
}

impl MediaProxyCacheUrls {
    /// Echoes `urls` as the answer body.
    #[must_use]
    pub fn new(urls: &[String]) -> Self {
        Self {
            urls: urls.to_vec(),
        }
    }
}

/// One relay of the admin ledger: the followed actor's `ActivityPub` id
/// beside whether that relay follows this instance back.
///
/// Pleroma's `{actor, followed_back}` map, one per `Relay.list` entry
/// (`lib/pleroma/web/activity_pub/relay.ex`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AdminRelay {
    /// The relay actor's `ActivityPub` id (the `relays` row's key).
    pub actor: String,
    /// Whether the relay's mirror holds an accepted follow of the
    /// instance actor.
    pub followed_back: bool,
}

/// The `GET /api/v1/pleroma/admin/relay` answer: every relay subscription
/// in Pleroma's `{actor, followed_back}` shape, under the `relays` key
/// `RelayController.index` renders.
#[derive(Debug, Serialize)]
pub struct AdminRelayList {
    /// The ledger, oldest subscription first.
    pub relays: Vec<AdminRelay>,
}

impl AdminRelay {
    /// Renders one [`RelayListing`] row under the `actor` name Pleroma's
    /// map gives the actor id.
    #[must_use]
    pub fn from_listing(listing: &RelayListing) -> Self {
        Self {
            actor: listing.actor_ap_id.clone(),
            followed_back: listing.followed_back,
        }
    }
}

impl AdminRelayList {
    /// Renders the whole ledger read, keeping the model's order.
    #[must_use]
    pub fn from_listings(listings: &[RelayListing]) -> Self {
        Self {
            relays: listings.iter().map(AdminRelay::from_listing).collect(),
        }
    }
}

/// One OAuth application, the admin API's shape.
///
/// Pleroma's `AdminAPI.OAuthAppView` is `MastodonAPI.AppView`'s
/// `show.json` with the `admin: true` arm adding `trusted` and the row
/// `id` (`lib/pleroma/web/admin_api/views/o_auth_app_view.ex`), and the
/// ticket pins those seven keys. `id` is the `UUIDv7` as lowercase
/// hyphenated text (decision 4); `client_secret` is secret material the
/// admin answer carries verbatim — which is why the manual `Debug` impl
/// below redacts it, as the stored row's does.
#[derive(Serialize)]
pub struct AdminOauthApp {
    /// The row's `UUIDv7` as text.
    pub id: String,
    /// The application name (`client_name`).
    pub name: String,
    /// The public OAuth client id.
    pub client_id: String,
    /// The OAuth client secret. Never in `Debug` output.
    pub client_secret: String,
    /// The redirect URIs, exactly as stored (`redirect_uri`, singular —
    /// Pleroma's view name).
    pub redirect_uri: String,
    /// The application homepage, `None` rendered as `null`.
    pub website: Option<String>,
    /// Whether the app skips the authorization consent screen.
    pub trusted: bool,
}

/// `client_secret` is secret material and renders as `<redacted>`;
/// everything else shows.
impl fmt::Debug for AdminOauthApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminOauthApp")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("website", &self.website)
            .field("trusted", &self.trusted)
            .finish()
    }
}

impl AdminOauthApp {
    /// Renders one stored [`OauthApp`] row in the admin API's shape.
    #[must_use]
    pub fn from_app(app: &OauthApp) -> Self {
        Self {
            id: app.id.to_string(),
            name: app.client_name.clone(),
            client_id: app.client_id.clone(),
            client_secret: app.client_secret.clone(),
            redirect_uri: app.redirect_uris.clone(),
            website: app.website.clone(),
            trusted: app.trusted,
        }
    }

    /// Renders a listing page's rows, in the order
    /// [`search`](crate::models::oauth_app::search) read them (newest
    /// first).
    #[must_use]
    pub fn from_rows(apps: &[OauthApp]) -> Vec<Self> {
        apps.iter().map(Self::from_app).collect()
    }
}

/// The `GET /api/v1/pleroma/admin/oauth_app` answer: the page's apps
/// beside the window size and the total.
///
/// Pleroma's `index.json` `{apps, count, page_size}`
/// (`lib/pleroma/web/mastodon_api/views/app_view.ex`, the `admin: true`
/// arm); `count` is the whole-table total the model reports, not the
/// filtered one — Pleroma's `App.search` aggregates `Repo.all` and the
/// answer keeps that meaning.
#[derive(Debug, Serialize)]
pub struct AdminOauthAppPage {
    /// The page's apps, newest first.
    pub apps: Vec<AdminOauthApp>,
    /// How many apps the instance has registered, filters aside.
    pub count: i64,
    /// The effective `page_size` the window was cut with.
    pub page_size: i64,
}

/// The `PATCH /api/v1/pleroma/admin/instance_document/{name}` answer:
/// Pleroma's `InstanceDocumentController.update` body, the public address
/// of the document just written (`%{"url" => url}`).
#[derive(Debug, Serialize)]
pub struct InstanceDocumentUrl {
    /// The `{BASE_URL}/instance/<name>.html` the document is served at.
    pub url: String,
}

/// The `DELETE /api/v1/pleroma/admin/instance_document/{name}` answer:
/// Pleroma's `json(conn, %{})` — the empty JSON object. A unit struct's
/// derived `Serialize` would write `null`, so this renders the map
/// itself.
#[derive(Debug)]
pub struct InstanceDocumentAck;

impl Serialize for InstanceDocumentAck {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_map(Some(0))?.end()
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

    /// A stored webhook row for the render tests.
    fn webhook(internal: bool) -> Webhook {
        Webhook {
            id: Uuid::parse_str("0198fc3a-0000-7000-8000-000000000042").expect("test uuid"),
            url: "https://example.org/hook".to_owned(),
            events: vec!["report.created".to_owned(), "account.created".to_owned()],
            secret: "ab".repeat(32),
            enabled: true,
            internal,
            inserted_at: Utc.with_ymd_and_hms(2026, 1, 31, 12, 34, 56).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 2, 1, 9, 0, 0).unwrap(),
        }
    }

    /// The entity carries exactly the ticket's six keys — no timestamps —
    /// the secret answered verbatim.
    #[test]
    fn a_webhook_renders_the_six_pinned_keys() {
        let rendered = serde_json::to_value(AdminWebhook::from_webhook(&webhook(false)))
            .expect("the entity serializes");
        assert_eq!(
            rendered,
            json!({
                "id": "0198fc3a-0000-7000-8000-000000000042",
                "url": "https://example.org/hook",
                "events": ["report.created", "account.created"],
                "secret": "ab".repeat(32),
                "enabled": true,
                "internal": false,
            }),
            "no created_at, no surprises"
        );
    }

    /// The page render keeps the rows in the order the model read them.
    #[test]
    fn the_page_render_keeps_the_row_order() {
        let rows = vec![webhook(false), webhook(true)];
        let rendered =
            serde_json::to_value(AdminWebhook::from_rows(&rows)).expect("the page serializes");
        let entries = rendered.as_array().expect("a JSON array");
        assert_eq!(entries.len(), 2, "both rows");
        assert_eq!(
            entries[1]["internal"],
            json!(true),
            "the second row is the internal one"
        );
    }

    /// A stored app with recognizable values in every column the admin
    /// entity reads.
    fn stored_app() -> OauthApp {
        OauthApp {
            id: Uuid::parse_str("0198fc3a-0000-7000-8000-000000000042").expect("test uuid"),
            client_name: "Admin Client".to_owned(),
            redirect_uris: "https://a.example/cb\nhttps://b.example/cb".to_owned(),
            scopes: "read write".to_owned(),
            website: None,
            client_id: "cid".to_owned(),
            client_secret: "top-secret".to_owned(),
            trusted: true,
            user_id: None,
            inserted_at: Utc.with_ymd_and_hms(2026, 1, 31, 12, 34, 56).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 2, 1, 9, 0, 0).unwrap(),
        }
    }

    /// The app entity carries exactly the ticket's seven keys, the
    /// redirect URIs under the singular `redirect_uri` name Pleroma's
    /// view uses.
    #[test]
    fn an_oauth_app_renders_the_seven_pinned_keys() {
        let rendered = serde_json::to_value(AdminOauthApp::from_app(&stored_app()))
            .expect("the entity serializes");
        assert_eq!(
            rendered,
            json!({
                "id": "0198fc3a-0000-7000-8000-000000000042",
                "name": "Admin Client",
                "client_id": "cid",
                "client_secret": "top-secret",
                "redirect_uri": "https://a.example/cb\nhttps://b.example/cb",
                "website": null,
                "trusted": true,
            }),
            "no scopes, no timestamps — the seven keys and nothing else"
        );
    }

    /// `client_secret` is secret material: the entity's `Debug` redacts
    /// it while the JSON keeps it (admins are owed the pair).
    #[test]
    fn the_app_debug_redacts_the_client_secret() {
        let entity = AdminOauthApp::from_app(&stored_app());
        let debug = format!("{entity:?}");
        assert!(
            !debug.contains("top-secret"),
            "the client secret must not leak"
        );
        assert!(
            debug.contains(r#"client_secret: "<redacted>""#),
            "the redaction marker names the field"
        );
        assert!(debug.contains("cid"), "the public id still shows");
    }

    /// The page wraps the rendered apps beside the total and the window
    /// size, the three keys Pleroma's `index.json` names.
    #[test]
    fn the_app_page_renders_apps_count_and_page_size() {
        let apps = AdminOauthApp::from_rows(&[stored_app()]);
        let rendered = serde_json::to_value(AdminOauthAppPage {
            apps,
            count: 7,
            page_size: 50,
        })
        .expect("the page serializes");
        assert_eq!(rendered["count"], json!(7), "the whole-table total");
        assert_eq!(rendered["page_size"], json!(50), "the effective window");
        let entries = rendered["apps"].as_array().expect("the apps array");
        assert_eq!(entries.len(), 1, "the one row");
        assert_eq!(entries[0]["name"], "Admin Client", "rendered as the entity");
    }

    /// The document answers: the URL object, and the empty
    /// object — which a derived unit-struct render would get wrong, hence
    /// the manual `Serialize`.
    #[test]
    fn the_document_answers_render_the_pinned_bodies() {
        let rendered = serde_json::to_value(InstanceDocumentUrl {
            url: "https://test.example/instance/terms-of-service.html".to_owned(),
        })
        .expect("the url object serializes");
        assert_eq!(
            rendered,
            json!({"url": "https://test.example/instance/terms-of-service.html"}),
            "one key, the public address"
        );
        let ack = serde_json::to_value(InstanceDocumentAck).expect("the ack serializes");
        assert_eq!(ack, json!({}), "the delete body is the empty object");
    }

    /// Two ledger rows for the relay renders: one followed back, one not.
    fn relay_listings() -> [RelayListing; 2] {
        [
            RelayListing {
                actor_ap_id: "https://relay.example/relay".to_owned(),
                followed_back: true,
            },
            RelayListing {
                actor_ap_id: "https://quiet.example/relay".to_owned(),
                followed_back: false,
            },
        ]
    }

    /// The listing renders Pleroma's `{actor, followed_back}` pair per
    /// ledger row under the `relays` key, order kept and nothing else;
    /// the single answer is one such pair bare.
    #[test]
    fn the_relay_entries_render_the_paired_keys() {
        let listings = relay_listings();
        let rendered = serde_json::to_value(AdminRelayList::from_listings(&listings))
            .expect("the list serializes");
        assert_eq!(
            rendered,
            json!({
                "relays": [
                    {"actor": "https://relay.example/relay", "followed_back": true},
                    {"actor": "https://quiet.example/relay", "followed_back": false},
                ]
            }),
            "two entries, the model's order, exactly the pinned keys"
        );
        let single = serde_json::to_value(AdminRelay::from_listing(&listings[0]))
            .expect("the entry serializes");
        assert_eq!(
            single,
            json!({"actor": "https://relay.example/relay", "followed_back": true}),
            "the follow answer carries no wrapper"
        );
    }

    /// A stored rule with the given position, for the render tests.
    fn rule(priority: i32) -> InstanceRule {
        InstanceRule {
            id: Uuid::parse_str("0198fc3a-0000-7000-8000-000000000042").expect("test uuid"),
            text: "No harassment".to_owned(),
            hint: "including in replies".to_owned(),
            priority,
            inserted_at: Utc.with_ymd_and_hms(2026, 1, 31, 12, 34, 56).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 2, 1, 8, 0, 0).unwrap(),
        }
    }

    /// The rule entity is Pleroma's four keys: the id as text, the two
    /// texts and the integer position — and no timestamps.
    #[test]
    fn a_rule_renders_the_four_admin_keys() {
        let rendered =
            serde_json::to_value(AdminRule::from_rule(&rule(1))).expect("the entity serializes");
        assert_eq!(
            rendered,
            json!({
                "id": "0198fc3a-0000-7000-8000-000000000042",
                "text": "No harassment",
                "hint": "including in replies",
                "priority": 1,
            }),
            "exactly the view's keys"
        );
    }

    /// The listing render keeps the model's priority-then-id order, an
    /// empty listing being an empty array.
    #[test]
    fn the_rule_listing_keeps_the_row_order() {
        let rows = vec![rule(-5), rule(1), rule(1)];
        let rendered = serde_json::to_value(AdminRule::from_rows(&rows)).expect("page serializes");
        let entries = rendered.as_array().expect("a JSON array");
        assert_eq!(entries.len(), 3, "one entry per row");
        assert_eq!(
            entries
                .iter()
                .filter_map(|e| e["priority"].as_i64())
                .collect::<Vec<i64>>(),
            vec![-5, 1, 1],
            "the order the model read them in"
        );
        assert_eq!(
            serde_json::to_value(AdminRule::from_rows(&[])).expect("the empty page serializes"),
            json!([]),
            "no rows, an empty array"
        );
    }
}
