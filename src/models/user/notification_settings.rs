//! The `users.notification_settings` column: per-account notification
//! flags (WBE-111).
//!
//! One jsonb value holds the settings Pleroma's
//! `Pleroma.User.NotificationSetting` embedded schema
//! (`lib/pleroma/user/notification_setting.ex`) defines: whether the
//! account blocks notifications from actors it does not follow
//! ([`block_from_strangers`](NotificationSettings::block_from_strangers))
//! and whether push deliveries must hide notification contents
//! ([`hide_notification_contents`](
//! NotificationSettings::hide_notification_contents)). The migration
//! default and [`NotificationSettings::default_value`] agree on one
//! spelling of "all off", and [`NotificationSettings::from_value`] parses
//! leniently (any missing or malformed piece reads as its default), so a
//! row written before a later flag existed still decodes.
//!
//! `block_from_strangers` is applied by
//! [`maybe_notify`](crate::models::notification::maybe_notify) when a
//! notification is created; `hide_notification_contents` is only stored
//! here — push delivery (a later ticket) reads it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::User;
use crate::error::AppResult;

/// The `notification_settings` jsonb column: everything the account
/// toggles at `PUT /api/pleroma/notification_settings` (Pleroma's
/// `User.NotificationSetting`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    /// Whether notifications from actors the account does not follow are
    /// withheld (Pleroma's `block_from_strangers`).
    pub block_from_strangers: bool,
    /// Whether push deliveries must hide the notification's contents
    /// (Pleroma's `hide_notification_contents`); stored only, read by the
    /// later push ticket.
    pub hide_notification_contents: bool,
}

impl NotificationSettings {
    /// The column value of an account that never touched the settings:
    /// exactly the migration default.
    #[must_use]
    pub fn default_value() -> Value {
        Self::default().to_value()
    }

    /// Reads `settings` out of a stored column [`Value`], falling back to
    /// the all-off default for anything unrecognised: a missing flag
    /// decodes as `false`, and a value of the wrong shape never fails a
    /// user read.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }

    /// The parsed `notification_settings` column of `user`.
    #[must_use]
    pub fn for_user(user: &User) -> Self {
        Self::from_value(&user.notification_settings)
    }

    /// The canonical column [`Value`] of these settings: always the full
    /// migration shape, so a stored row never drifts.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({
            "block_from_strangers": self.block_from_strangers,
            "hide_notification_contents": self.hide_notification_contents,
        })
    }
}

/// The `UPDATE` behind [`save`]: writes the whole column in one statement
/// and refreshes `updated_at`.
const UPDATE_SQL: &str =
    "UPDATE users SET notification_settings = $2, updated_at = now() WHERE id = $1";

/// Stores `settings` as user `id`'s `notification_settings` column.
///
/// Unknown ids match no row and change nothing; the endpoint runs on an
/// authenticated user, so the row exists.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn save(pool: &PgPool, id: Uuid, settings: &NotificationSettings) -> AppResult<()> {
    sqlx::query(UPDATE_SQL)
        .bind(id)
        .bind(settings.to_value())
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::user::{NewUser, insert, local_ap_id};

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = "https://test.example";

    /// The migration's `DEFAULT` spelling, verbatim.
    fn migration_default() -> Value {
        serde_json::from_str(
            r#"{"block_from_strangers": false, "hide_notification_contents": false}"#,
        )
        .unwrap()
    }

    #[test]
    fn default_value_is_the_migration_default_shape() {
        assert_eq!(
            NotificationSettings::default_value(),
            migration_default(),
            "Rust and SQL agree on \"all off\""
        );
    }

    #[test]
    fn parsing_is_lenient_about_missing_and_malformed_pieces() {
        assert_eq!(
            NotificationSettings::from_value(&json!({"block_from_strangers": true})),
            NotificationSettings {
                block_from_strangers: true,
                hide_notification_contents: false,
            },
            "a flag stated alone decodes, the other defaults"
        );
        assert_eq!(
            NotificationSettings::from_value(&json!({"strange": "key"})),
            NotificationSettings::default(),
            "unknown keys are ignored"
        );
        for junk in [json!({}), json!(null), json!("yes"), json!([1])] {
            assert_eq!(
                NotificationSettings::from_value(&junk),
                NotificationSettings::default(),
                "`{junk}` reads as all off rather than failing the read"
            );
        }
        assert_eq!(
            NotificationSettings::from_value(&json!({"block_from_strangers": "true"})),
            NotificationSettings::default(),
            "a string is not a boolean"
        );
    }

    #[test]
    fn to_value_is_always_the_full_shape() {
        let settings = NotificationSettings {
            block_from_strangers: true,
            hide_notification_contents: true,
        };
        assert_eq!(
            settings.to_value(),
            json!({
                "block_from_strangers": true,
                "hide_notification_contents": true,
            }),
            "both flags render"
        );
        assert_eq!(
            NotificationSettings::from_value(&settings.to_value()),
            settings,
            "and the round trip is the identity"
        );
    }

    #[tokio::test]
    async fn save_writes_the_column_and_reads_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = insert(
            pool,
            &NewUser::local("settings-alice", &local_ap_id(BASE, "settings-alice")),
        )
        .await
        .expect("local user fixture");
        assert_eq!(
            alice.notification_settings,
            migration_default(),
            "a fresh row starts at the migration default"
        );

        let settings = NotificationSettings {
            block_from_strangers: true,
            hide_notification_contents: false,
        };
        save(pool, alice.id, &settings)
            .await
            .expect("the save runs");
        let reread = crate::models::user::get_by_id(pool, alice.id)
            .await
            .expect("the read runs")
            .expect("the row is there");
        assert_eq!(
            NotificationSettings::for_user(&reread),
            settings,
            "the stored column is the written settings"
        );
        assert!(
            reread.updated_at > alice.updated_at,
            "the write refreshes updated_at"
        );

        save(pool, Uuid::now_v7(), &settings)
            .await
            .expect("an unknown id changes nothing and is not an error");
    }
}
