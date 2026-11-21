//! The delivered document: whether an alert wakes a subscription
//! ([`alert_enabled`]), the title verb [`verb_for`] looks up, and the
//! Mastodon-shaped JSON [`payload_json`] stitches from a [`PushPayload`].

use serde_json::{Value, json};
use uuid::Uuid;

use crate::models::notification;

/// The payload's `preferred_locale`: this instance has no localized
/// notification strings yet, so every wake reads English — the same
/// constant [`crate::mailer`] letters in.
const PREFERRED_LOCALE: &str = "en";

/// The verbs [`payload_json`] pairs with the actor's display name, by
/// notification type (Mastodon's push wording; Pleroma's `format_body`
/// clauses in spirit). [`DEFAULT_VERB`] covers a type not listed.
const VERBS: &[(&str, &str)] = &[
    (notification::TYPE_FOLLOW, "followed you"),
    (notification::TYPE_FOLLOW_REQUEST, "requested to follow you"),
    (notification::TYPE_FAVOURITE, "favourited your post"),
    (notification::TYPE_REBLOG, "boosted your post"),
    (notification::TYPE_MENTION, "mentioned you"),
    (notification::TYPE_EMOJI_REACTION, "reacted to your post"),
    ("status", "posted"),
    ("update", "edited a post"),
    ("poll", "ended a poll"),
    ("pleroma:chat_mention", "sent you a chat message"),
    ("admin.sign_up", "signed up"),
    ("admin.report", "filed a report"),
];
/// The title verb for a notification type [`VERBS`] does not name.
const DEFAULT_VERB: &str = "sent you a notification";

/// Whether `notification_type` wakes this subscription.
///
/// The stored `alerts[type]` boolean decides, defaulting to on for a type
/// the object does not name (Pleroma's `Subscription.enabled?` treats an
/// absent flag as enabled, and the API's rows carry every known type
/// anyway).
#[must_use]
pub fn alert_enabled(alerts: &Value, notification_type: &str) -> bool {
    alerts
        .get(notification_type)
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// The title verb [`payload_json`] pairs with the actor's display name for
/// `notification_type` — `DEFAULT_VERB` for a type `VERBS` does not name.
#[must_use]
pub fn verb_for(notification_type: &str) -> &'static str {
    VERBS
        .iter()
        .find(|(kind, _verb)| *kind == notification_type)
        .map_or(DEFAULT_VERB, |(_kind, verb)| *verb)
}

/// Everything [`payload_json`] stitches into the delivered document, read
/// from the database by the [`push.deliver` job](crate::jobs::web_push).
#[derive(Clone, Debug)]
pub struct PushPayload<'a> {
    /// The subscription's access token string (`oauth_tokens.token`): what
    /// the client wakes with to fetch what changed.
    pub access_token: &'a str,
    /// The notification row's id.
    pub notification_id: Uuid,
    /// The notification's type, as stored.
    pub notification_type: &'a str,
    /// The actor's avatar URL.
    pub icon: &'a str,
    /// The actor's display name (its nickname when unnamed): the title's
    /// first half.
    pub actor_display_name: &'a str,
    /// The status's plain text, or empty — hidden contents and status-less
    /// types both send `""`.
    pub body: &'a str,
}

/// The Mastodon-shaped push payload as compact JSON.
///
/// The document: `access_token`, `notification_id` (hyphenated text,
/// decision 4), `notification_type`, `icon`, the fixed
/// `PREFERRED_LOCALE` `preferred_locale`, the `"<actor display name> <verb>"` title (the verb
/// from [`verb_for`]) and the plain-text `body`.
#[must_use]
pub fn payload_json(parts: &PushPayload<'_>) -> String {
    json!({
        "access_token": parts.access_token,
        "notification_id": parts.notification_id.to_string(),
        "notification_type": parts.notification_type,
        "icon": parts.icon,
        "preferred_locale": PREFERRED_LOCALE,
        "title": format!(
            "{} {}",
            parts.actor_display_name,
            verb_for(parts.notification_type)
        ),
        "body": parts.body,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_flags_read_the_stored_object_with_a_default_on() {
        let alerts = json!({"follow": true, "favourite": false, "mention": "yes"});
        assert!(alert_enabled(&alerts, "follow"), "explicitly on");
        assert!(!alert_enabled(&alerts, "favourite"), "explicitly off");
        assert!(
            alert_enabled(&alerts, "reblog"),
            "a type the object does not name is on"
        );
        assert!(
            alert_enabled(&alerts, "mention"),
            "a non-boolean flag is not an off switch"
        );
        assert!(
            alert_enabled(&Value::Null, "follow"),
            "a null object alerts all"
        );
    }

    #[test]
    fn every_wakeable_type_gets_its_verb_and_the_fallback_covers_the_rest() {
        assert_eq!(verb_for(notification::TYPE_FOLLOW), "followed you");
        assert_eq!(
            verb_for(notification::TYPE_FOLLOW_REQUEST),
            "requested to follow you"
        );
        assert_eq!(
            verb_for(notification::TYPE_FAVOURITE),
            "favourited your post"
        );
        assert_eq!(verb_for(notification::TYPE_REBLOG), "boosted your post");
        assert_eq!(verb_for(notification::TYPE_MENTION), "mentioned you");
        assert_eq!(
            verb_for(notification::TYPE_EMOJI_REACTION),
            "reacted to your post"
        );
        assert_eq!(verb_for("status"), "posted", "a followed account posted");
        assert_eq!(verb_for("update"), "edited a post");
        assert_eq!(verb_for("poll"), "ended a poll");
        assert_eq!(verb_for("pleroma:chat_mention"), "sent you a chat message");
        assert_eq!(verb_for("admin.sign_up"), "signed up");
        assert_eq!(verb_for("admin.report"), "filed a report");
        assert_eq!(verb_for("mystery.type"), DEFAULT_VERB, "the fallback");
    }

    #[test]
    fn the_payload_document_carries_the_mastodon_fields() {
        let parts = PushPayload {
            access_token: "tok3n",
            notification_id: Uuid::now_v7(),
            notification_type: notification::TYPE_FAVOURITE,
            icon: "https://wired.example/avatar.png",
            actor_display_name: "Alice",
            body: "the liked post's plain text",
        };
        let text = payload_json(&parts);
        let value: Value = serde_json::from_str(&text).expect("the payload is JSON");
        assert_eq!(
            value["access_token"], "tok3n",
            "the client wakes with its token"
        );
        assert_eq!(
            value["notification_id"],
            parts.notification_id.to_string(),
            "the id round-trips as decision 4's hyphenated text"
        );
        assert_eq!(value["notification_type"], "favourite");
        assert_eq!(value["icon"], "https://wired.example/avatar.png");
        assert_eq!(value["preferred_locale"], "en");
        assert_eq!(value["title"], "Alice favourited your post", "name + verb");
        assert_eq!(value["body"], "the liked post's plain text");
        let hidden = payload_json(&PushPayload {
            body: "",
            ..parts.clone()
        });
        let hidden: Value = serde_json::from_str(&hidden).expect("the hidden payload is JSON");
        assert_eq!(hidden["body"], "", "a hidden-content wake carries no body");
    }
}
