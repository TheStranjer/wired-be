//! The Mastodon `WebPush::SubscriptionEntity` response: one access
//! token's Web Push subscription (`docs/ARCHITECTURE.md` decision 3).
//!
//! One shape serves all three reads and writes of `src/api/push.rs`:
//! Pleroma's `SubscriptionView.show.json`
//! (`lib/pleroma/web/mastodon_api/views/subscription_view.ex`) renders
//! `id`, `endpoint`, `alerts` and `server_key`; the ticket adds `policy`
//! and pins `alerts` to list **every** known notification type as a
//! boolean, defaulting the ones the client never named. `server_key` is
//! the instance VAPID public key (see [`crate::services::vapid`]), which
//! the push endpoints hand out alongside the row.

use serde_json::{Map, Value};

use crate::models::web_push_subscription::WebPushSubscription;

/// Every notification type the push `alerts` object knows, in the order
/// the ticket lists.
///
/// Mastodon's ten subscription alert types (the 4.5 `quote` and
/// `quoted_update` among them) beside its two `admin.` ones and
/// Pleroma's two `pleroma:` chat and reaction types. An alert named here
/// may be turned on or off; anything else a client sends is ignored,
/// Pleroma's `Map.take` of its supported list.
pub const ALERT_TYPES: [&str; 14] = [
    "follow",
    "favourite",
    "reblog",
    "mention",
    "poll",
    "status",
    "update",
    "quote",
    "quoted_update",
    "follow_request",
    "pleroma:chat_mention",
    "pleroma:emoji_reaction",
    "admin.sign_up",
    "admin.report",
];

/// The `WebPush::SubscriptionEntity`: one token's subscription as the
/// four endpoints answer it.
#[derive(Debug, serde::Serialize)]
pub struct PushSubscription {
    /// The row's `UUIDv7` id as lowercase hyphenated text (decision 4).
    pub id: String,
    /// The browser callback URL, exactly as registered.
    pub endpoint: String,
    /// Every [`ALERT_TYPES`] entry as a boolean, the stored flags first.
    pub alerts: Map<String, Value>,
    /// The instance VAPID public key clients verify pushes against.
    pub server_key: String,
    /// Who may wake the subscription: `all`, `followed`, `follower` or
    /// `none`.
    pub policy: String,
}

impl PushSubscription {
    /// Renders the stored `row` with the instance's `server_key`.
    #[must_use]
    pub fn from_row(row: &WebPushSubscription, server_key: &str) -> Self {
        Self {
            id: row.id.to_string(),
            endpoint: row.endpoint.clone(),
            alerts: full_alerts(&row.alerts),
            server_key: server_key.to_owned(),
            policy: row.policy.clone(),
        }
    }
}

/// The complete [`alerts`](PushSubscription::alerts) object: every
/// [`ALERT_TYPES`] name as a boolean.
///
/// A type is true where `stored` says so and false everywhere else — for
/// types `stored` does not name and for any stored value that is not a
/// boolean. Unknown stored keys drop out: the response lists what this
/// instance knows, not what a client once sent.
#[must_use]
pub fn full_alerts(stored: &Value) -> Map<String, Value> {
    let mut alerts = Map::new();
    for alert_type in ALERT_TYPES {
        let on = stored
            .get(alert_type)
            .and_then(Value::as_bool)
            .unwrap_or_default();
        let _entry: Option<Value> = alerts.insert(alert_type.to_owned(), Value::Bool(on));
    }
    alerts
}

/// The stored alerts with every flag `given` names replaced.
///
/// A flag `given` does not name keeps its stored value, defaulting to
/// `false` when neither object has it: `create` merges over the empty
/// object, `update` over the stored one, the same present-means-change
/// rule Pleroma's changeset follows.
#[must_use]
pub fn merge_alerts(stored: &Value, given: &Value) -> Value {
    let mut base = stored.as_object().cloned().unwrap_or_default();
    if let Value::Object(given) = given {
        for (key, value) in given {
            if value.is_boolean() {
                let _entry: Option<Value> = base.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(full_alerts(&Value::Object(base)))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    /// A stored row whose alerts name two types, one on.
    fn row(alerts: Value) -> WebPushSubscription {
        WebPushSubscription {
            id: Uuid::now_v7(),
            user_id: Some(Uuid::now_v7()),
            token_id: Uuid::now_v7(),
            endpoint: "https://push.example/sub".to_owned(),
            key_p256dh: "key".to_owned(),
            key_auth: "auth".to_owned(),
            alerts,
            policy: "all".to_owned(),
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn from_row_renders_the_entity_with_the_server_key() {
        let stored = row(json!({"follow": true, "mention": false}));
        let id = stored.id.to_string();
        let json = serde_json::to_value(PushSubscription::from_row(&stored, "vapid-key")).unwrap();
        assert_eq!(json["id"], json!(id), "the uuid as hyphenated text");
        assert_eq!(json["endpoint"], "https://push.example/sub", "the URL");
        assert_eq!(json["server_key"], "vapid-key", "the instance public key");
        assert_eq!(json["policy"], "all", "the stored policy");
        assert_eq!(json["alerts"]["follow"], true, "the named flag");
        assert_eq!(json["alerts"]["mention"], false, "named off");
    }

    #[test]
    fn alerts_list_every_known_type() {
        let stored = row(json!({}));
        let json = serde_json::to_value(PushSubscription::from_row(&stored, "k")).unwrap();
        let alerts = json["alerts"].as_object().expect("alerts is an object");
        assert_eq!(
            alerts.len(),
            ALERT_TYPES.len(),
            "every known type is listed"
        );
        for alert_type in ALERT_TYPES {
            assert_eq!(
                alerts[alert_type],
                Value::Bool(false),
                "`{alert_type}` defaults to false"
            );
        }
    }

    #[test]
    fn full_alerts_drops_unknown_names_and_non_booleans() {
        let complete = full_alerts(&json!({
            "follow": true,
            "bogus": true,
            "mention": "yes",
        }));
        assert_eq!(complete["follow"], json!(true), "the stored flag shows");
        assert!(
            !complete.contains_key("bogus"),
            "a type this instance does not know is not listed"
        );
        assert_eq!(
            complete["mention"],
            json!(false),
            "a stored value that is not a boolean reads as off"
        );
    }

    #[test]
    fn merge_alerts_replaces_named_flags_and_completes_the_rest() {
        let merged = merge_alerts(
            &json!({"follow": true, "mention": true}),
            &json!({"follow": false, "reblog": true, "junk": "text"}),
        );
        assert_eq!(
            merged,
            json!(full_alerts(&json!({
                "follow": false,
                "mention": true,
                "reblog": true,
            }))),
            "named flags change, unnamed ones stay, non-booleans are ignored"
        );
    }

    #[test]
    fn merge_over_nothing_gives_the_all_default_object() {
        let merged = merge_alerts(&json!({"follow": true}), &Value::Null);
        assert_eq!(
            merged,
            json!(full_alerts(&json!({"follow": true}))),
            "an empty submission keeps the stored flags"
        );
        let fresh = merge_alerts(&Value::Null, &json!({"poll": true}));
        assert_eq!(
            fresh["poll"],
            json!(true),
            "a create starting from nothing still lists every type"
        );
        assert_eq!(
            fresh["follow"],
            json!(false),
            "and defaults the rest to off"
        );
    }
}
