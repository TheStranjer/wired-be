//! The notification text a push carries: the plain-text `body`, the
//! actor's avatar as `icon` and the display name the title leads with.

use sqlx::PgPool;

use super::retry;
use crate::{
    entities::account::DEFAULT_AVATAR_PATH,
    jobs::JobError,
    models::{
        notification::Notification,
        status,
        user::{User, notification_settings::NotificationSettings},
    },
    state::AppState,
    util::{format, sanitize},
};

/// The longest delivered `body`, in characters — Pleroma's `@body_chars`
/// in `lib/pleroma/web/push/impl.ex`.
const BODY_CHARS: usize = 140;

/// The delivered `body`: the notification's status reduced to plain text
/// and cut to [`BODY_CHARS`] (Pleroma's `scrub_html_and_truncate`), empty
/// when the recipient hides notification contents or the notification
/// names no (living) status.
///
/// # Errors
///
/// [`JobError::Retry`] on database failure.
pub(super) async fn body_text(
    pool: &PgPool,
    recipient: &User,
    note: &Notification,
) -> Result<String, JobError> {
    if NotificationSettings::for_user(recipient).hide_notification_contents {
        return Ok(String::new());
    }
    let Some(status_id) = note.status_id else {
        return Ok(String::new());
    };
    let Some(row) = status::get_by_id(pool, status_id)
        .await
        .map_err(|err| retry(&err))?
    else {
        return Ok(String::new());
    };
    let plain = sanitize::strip_html(&row.content);
    Ok(format::truncate_chars(plain.trim(), BODY_CHARS))
}

/// The delivered `icon`: the actor's avatar URL, or the same instance
/// default the [`Account`](crate::entities::account::Account) entity
/// renders when the actor has none — under the frontend origin, where the
/// file is actually served.
pub(super) fn icon_for(state: &AppState, actor: &User) -> String {
    actor
        .avatar_url
        .clone()
        .unwrap_or_else(|| format!("{}{DEFAULT_AVATAR_PATH}", state.config.frontend_base_url))
}

/// The title's first half: the actor's display name, its nickname when the
/// account never set one (what a client shows where a name belongs).
pub(super) fn display_name(actor: &User) -> &str {
    if actor.name.is_empty() {
        &actor.nickname
    } else {
        &actor.name
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::test_support::{
        Pref, account, app_state, app_state_with, captured_body, client_keys, local,
        mock_push_service, note_about, open_pref, run, subscribe,
    };
    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        models::{notification, user},
        services::web_push,
    };

    #[tokio::test]
    async fn the_default_icon_hangs_off_the_frontend_origin() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut config = Config::for_tests();
        config.frontend_base_url = "https://fe.example".to_owned();
        let state = app_state_with(&pool, config);
        let actor = local(&pool, "iconalice", "Alice").await;

        let icon = icon_for(&state, &actor);

        assert!(
            icon.starts_with("https://fe.example") && icon.ends_with(DEFAULT_AVATAR_PATH),
            "the fallback icon is the default avatar on the frontend origin: {icon}"
        );
    }

    #[tokio::test]
    async fn a_mention_delivers_the_status_plain_text_under_a_nickname_title() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // An actor with no display name falls back to the nickname; the
        // mention names its status, whose text becomes the body.
        let actor = local(&pool, "wp-m-alice", "").await;
        let recipient = account(&pool, "wp-m-bob").await;
        let post = status::insert(
            &pool,
            TEST_BASE_URL,
            &status::NewStatus::local(&actor, "look at this"),
        )
        .await
        .expect("the status fixture inserts");
        let (url, seen, server) = mock_push_service(201).await;
        let keys = client_keys();
        let subscription = subscribe(
            &pool,
            &recipient,
            &url,
            &keys,
            &Pref {
                alerts: json!({"mention": true}),
                policy: "all",
            },
        )
        .await;
        let note = note_about(
            &pool,
            &recipient.user,
            &actor,
            notification::TYPE_MENTION,
            Some(post.id),
        )
        .await;

        run(&app_state(&pool), note.id, subscription.id)
            .await
            .expect("the delivery runs");
        server.abort();
        let decrypted =
            web_push::decrypt_payload(&captured_body(&seen), &keys.private_key, &keys.auth)
                .expect("the client reads the body");
        let payload: Value = serde_json::from_slice(&decrypted).expect("the payload is JSON");
        assert_eq!(payload["notification_type"], notification::TYPE_MENTION);
        assert_eq!(
            payload["title"], "wp-m-alice mentioned you",
            "a nameless actor wakes by nickname"
        );
        assert_eq!(
            payload["body"], "look at this",
            "the body is the status's plain text"
        );
    }

    #[tokio::test]
    async fn a_hidden_account_gets_no_body_but_still_a_title() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let actor = local(&pool, "wp-h-alice", "Alice").await;
        let recipient = account(&pool, "wp-h-bob").await;
        user::notification_settings::save(
            &pool,
            recipient.user.id,
            &NotificationSettings {
                hide_notification_contents: true,
                ..NotificationSettings::default()
            },
        )
        .await
        .expect("the settings save");
        let post = status::insert(
            &pool,
            TEST_BASE_URL,
            &status::NewStatus::local(&actor, "a secret mention"),
        )
        .await
        .expect("the status fixture inserts");
        let (url, seen, server) = mock_push_service(201).await;
        let keys = client_keys();
        let subscription = subscribe(&pool, &recipient, &url, &keys, &open_pref()).await;
        let note = note_about(
            &pool,
            &recipient.user,
            &actor,
            notification::TYPE_MENTION,
            Some(post.id),
        )
        .await;

        run(&app_state(&pool), note.id, subscription.id)
            .await
            .expect("the delivery runs");
        server.abort();
        let decrypted =
            web_push::decrypt_payload(&captured_body(&seen), &keys.private_key, &keys.auth)
                .expect("the client reads the body");
        let payload: Value = serde_json::from_slice(&decrypted).expect("the payload is JSON");
        assert_eq!(payload["body"], "", "the hidden contents never leave here");
        assert_eq!(
            payload["title"], "Alice mentioned you",
            "the title still says who"
        );
    }
}
