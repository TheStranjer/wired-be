//! Writing a row: [`insert`] and the [`CountDelta`] step the denormalised
//! counters move by. The id is minted here and every other column comes
//! from the handed-in [`NewUser`].

use sqlx::{Executor, Postgres};

use super::{NewUser, User};
use crate::{error::AppResult, util::id::new_id};

/// One step to apply to the denormalised counters of a user atomically.
///
/// Each field is added to its column by [`update_counts`](super::update_counts); negatives
/// shrink the count, and the result is clamped at zero.
#[derive(Debug, Default, Clone, Copy)]
pub struct CountDelta {
    /// Step for `followers_count`.
    pub followers: i32,
    /// Step for `following_count`.
    pub following: i32,
    /// Step for `statuses_count`.
    pub statuses: i32,
}

/// The `INSERT` behind [`insert`]: every column is written from a
/// [`NewUser`] (the id is the extra bind), with `RETURNING *` so the row
/// comes back with its database-side timestamps.
const INSERT_SQL: &str = "INSERT INTO users (
     id, nickname, ap_id, local, domain, actor_type, email, password_hash,
     name, bio, raw_bio, avatar_url, banner_url, profile_url, locked, bot,
     discoverable, fields, inbox, shared_inbox, outbox, followers_url,
     following_url, featured_url, public_key, private_key, is_admin,
     is_moderator, is_active, hide_followers, hide_follows,
     hide_followers_count, hide_follows_count, hide_favorites,
     allow_following_move, skip_thread_containment, accepts_chat_messages,
     show_role, no_rich_text, default_scope, sensitive_default, language,
     tags, settings_store, background_image_url, birthday, show_birthday,
     also_known_as, moved_to_ap_id, last_move_at, email_confirmed,
     approval_pending, last_active_at, followers_count, following_count,
     statuses_count, last_status_at, last_fetched_at, mfa_settings,
     raw_fields, is_suggested, registration_reason,
     password_reset_pending, notification_settings, email_notifications,
     confirmation_token, confirmation_sent_at
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
     $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28,
     $29, $30, $31, $32, $33, $34, $35, $36, $37, $38, $39, $40, $41,
     $42, $43, $44, $45, $46, $47, $48, $49, $50, $51, $52, $53, $54,
     $55, $56, $57, $58, $59, $60, $61, $62, $63, $64, $65, $66, $67
) RETURNING *";

/// Inserts `new_user` on `executor` — the pool or an open transaction —
/// and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4); every other column comes from
/// `new_user`, whose [`Default`](NewUser::default) mirrors the column
/// defaults. `nickname`, `ap_id` and `email` are unique, and citext folds
/// case in the nickname and email comparisons, so a duplicate in any case
/// is rejected.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); a unique violation
/// (SQLSTATE `23505`) is reported through it unchanged.
pub async fn insert<'e, E>(executor: E, new_user: &NewUser) -> AppResult<User>
where
    E: Executor<'e, Database = Postgres>,
{
    let user = sqlx::query_as::<_, User>(INSERT_SQL)
        .bind(new_id())
        .bind(new_user.nickname.as_str())
        .bind(new_user.ap_id.as_str())
        .bind(new_user.local)
        .bind(new_user.domain.as_deref())
        .bind(new_user.actor_type.as_str())
        .bind(new_user.email.as_deref())
        .bind(new_user.password_hash.as_deref())
        .bind(new_user.name.as_str())
        .bind(new_user.bio.as_str())
        .bind(new_user.raw_bio.as_str())
        .bind(new_user.avatar_url.as_deref())
        .bind(new_user.banner_url.as_deref())
        .bind(new_user.profile_url.as_deref())
        .bind(new_user.locked)
        .bind(new_user.bot)
        .bind(new_user.discoverable)
        .bind(new_user.fields.clone())
        .bind(new_user.inbox.as_deref())
        .bind(new_user.shared_inbox.as_deref())
        .bind(new_user.outbox.as_deref())
        .bind(new_user.followers_url.as_deref())
        .bind(new_user.following_url.as_deref())
        .bind(new_user.featured_url.as_deref())
        .bind(new_user.public_key.as_deref())
        .bind(new_user.private_key.as_deref())
        .bind(new_user.is_admin)
        .bind(new_user.is_moderator)
        .bind(new_user.is_active)
        .bind(new_user.hide_followers)
        .bind(new_user.hide_follows)
        .bind(new_user.hide_followers_count)
        .bind(new_user.hide_follows_count)
        .bind(new_user.hide_favorites)
        .bind(new_user.allow_following_move)
        .bind(new_user.skip_thread_containment)
        .bind(new_user.accepts_chat_messages)
        .bind(new_user.show_role)
        .bind(new_user.no_rich_text)
        .bind(new_user.default_scope.as_str())
        .bind(new_user.sensitive_default)
        .bind(new_user.language.as_deref())
        .bind(&new_user.tags)
        .bind(new_user.settings_store.clone())
        .bind(new_user.background_image_url.as_deref())
        .bind(new_user.birthday)
        .bind(new_user.show_birthday)
        .bind(&new_user.also_known_as)
        .bind(new_user.moved_to_ap_id.as_deref())
        .bind(new_user.last_move_at)
        .bind(new_user.email_confirmed)
        .bind(new_user.approval_pending)
        .bind(new_user.last_active_at)
        .bind(new_user.followers_count)
        .bind(new_user.following_count)
        .bind(new_user.statuses_count)
        .bind(new_user.last_status_at)
        .bind(new_user.last_fetched_at)
        .bind(new_user.mfa_settings.clone())
        .bind(new_user.raw_fields.clone())
        .bind(new_user.is_suggested)
        .bind(new_user.registration_reason.as_deref())
        .bind(new_user.password_reset_pending)
        .bind(new_user.notification_settings.clone())
        .bind(new_user.email_notifications)
        .bind(new_user.confirmation_token.as_deref())
        .bind(new_user.confirmation_sent_at)
        .fetch_one(executor)
        .await?;
    Ok(user)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, NaiveDate, SubsecRound as _, Utc};
    use serde_json::json;
    use sqlx::Error as SqlxError;

    use super::*;
    use crate::{
        error::AppError,
        models::user::{
            DEFAULT_SCOPE, Flag, get_by_nickname, get_many, local_ap_id, notification_settings,
            test_support::*,
        },
    };

    /// The privacy and behaviour flags a plain [`NewUser::local`] row gets
    /// from the migration.
    fn assert_pleroma_flag_defaults(user: &User) {
        assert!(
            !user.hide_followers.get() && !user.hide_follows.get(),
            "the follow collections are public by default"
        );
        assert!(
            !user.hide_followers_count.get() && !user.hide_follows_count.get(),
            "and so are both counters"
        );
        assert!(
            user.hide_favorites.get(),
            "hide_favorites is the one privacy flag that defaults to true"
        );
        assert!(
            user.allow_following_move.get(),
            "a move takes the following list unless the account says otherwise"
        );
        assert!(
            !user.skip_thread_containment.get(),
            "thread containment applies by default"
        );
        assert!(
            user.accepts_chat_messages.get(),
            "chat is accepted unless the account opts out"
        );
        assert!(user.show_role.get(), "roles are shown by default");
        assert!(
            !user.no_rich_text.get(),
            "rich text is on unless the account asks for plain text"
        );
        assert!(
            !user.sensitive_default.get(),
            "new statuses are not flagged sensitive by default"
        );
        assert!(
            !user.is_suggested.get(),
            "the suggestion pool is admin-curated, so nothing starts in it"
        );
        assert!(
            user.email_notifications.get(),
            "accounts start opted in to email"
        );
    }

    /// The rest of the Pleroma extension columns of a plain
    /// [`NewUser::local`] row: the empty collections and blobs, the unset
    /// images and moves, and the account-state stamps.
    fn assert_pleroma_value_defaults(user: &User) {
        assert_eq!(
            user.default_scope, DEFAULT_SCOPE,
            "statuses default to public"
        );
        assert!(
            user.language.is_none(),
            "an account that named no language has no default"
        );
        assert!(user.tags.is_empty(), "tags default to the empty array");
        assert!(
            user.also_known_as.is_empty(),
            "aliases default to the empty array"
        );
        assert_eq!(
            user.settings_store,
            json!({}),
            "the settings blob defaults to an empty object"
        );
        assert!(
            user.background_image_url.is_none(),
            "there is no default background image"
        );
        assert!(user.birthday.is_none(), "no birthday is stored by default");
        assert!(
            !user.show_birthday.get(),
            "a birthday stays private by default"
        );
        assert!(user.moved_to_ap_id.is_none(), "nobody has moved by default");
        assert!(
            user.last_move_at.is_none(),
            "an account that never moved has no move stamp"
        );
        assert!(
            user.email_confirmed.get(),
            "email_confirmed defaults to true: an account is not pending \
             confirmation until the confirmation flow says otherwise"
        );
        assert!(
            !user.approval_pending.get(),
            "approval is not pending by default"
        );
        assert!(
            user.confirmation_token.is_none(),
            "no confirmation token exists until the confirmation flow mints one"
        );
        assert!(
            user.confirmation_sent_at.is_none(),
            "no confirmation letter was queued by default"
        );
        assert!(
            user.registration_reason.is_none(),
            "an account that stated no registration reason has none stored"
        );
        assert!(
            user.last_active_at.is_none(),
            "an account that never logged in has no last-active time"
        );
        assert_eq!(
            user.mfa_settings,
            json!({"enabled": false, "totp": {"secret": null, "confirmed": false}, "backup_codes": []}),
            "mfa_settings defaults to the migration's all-off blob"
        );
        assert_eq!(
            user.notification_settings,
            notification_settings::NotificationSettings::default_value(),
            "notification_settings defaults to the migration's all-off blob"
        );
    }

    /// Every Pleroma extension column at the default the migration states,
    /// for a row inserted through the plain [`NewUser::local`] constructor
    /// (which mirrors those defaults).
    #[tokio::test]
    async fn pleroma_columns_default_as_the_migration_states() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = insert(&pool, &local_new("cara")).await.unwrap();

        assert_pleroma_flag_defaults(&user);
        assert_pleroma_value_defaults(&user);
    }

    /// The Pleroma extension flags, [`NewUser::default_scope`] and the
    /// sensitive default, each set to the opposite of its migration default.
    fn assert_pleroma_flags_round_trip(user: &User) {
        assert!(
            user.hide_followers.get()
                && user.hide_follows.get()
                && user.hide_followers_count.get()
                && user.hide_follows_count.get(),
            "the four hiding flags round trip as set"
        );
        assert!(
            !user.hide_favorites.get(),
            "so does the one defaulting to true, flipped"
        );
        assert!(
            !user.allow_following_move.get()
                && user.skip_thread_containment.get()
                && !user.accepts_chat_messages.get()
                && !user.show_role.get()
                && user.no_rich_text.get(),
            "behaviour and role flags round trip"
        );
        assert_eq!(
            user.default_scope, "private",
            "the default scope round trips"
        );
        assert!(
            user.sensitive_default.get(),
            "the sensitive default round trips through its flag"
        );
        assert!(user.show_birthday.get(), "the birthday switch round trips");
        assert!(
            !user.email_confirmed.get(),
            "an unconfirmed email round trips"
        );
        assert!(user.approval_pending.get(), "pending approval round trips");
        assert!(
            !user.email_notifications.get(),
            "the email switch round trips flipped"
        );
    }

    /// The Pleroma extension values: both arrays in the order they were
    /// written, both jsonb blobs, the date, the move and the two stamps.
    fn assert_pleroma_values_round_trip(user: &User, active_at: DateTime<Utc>) {
        assert_eq!(
            user.language.as_deref(),
            Some("eo"),
            "the default language round trips"
        );
        assert_eq!(
            user.tags,
            vec!["gnw".to_owned(), "rust".to_owned()],
            "a text[] keeps its order"
        );
        assert_eq!(
            user.also_known_as,
            vec![local_ap_id(BASE, "alicia"), local_ap_id(BASE, "alice2")],
            "the alias array keeps its order"
        );
        assert_eq!(
            user.settings_store,
            json!({"pleroma_fe": {"theme": "dark"}}),
            "the settings blob round trips through jsonb"
        );
        assert_eq!(
            user.background_image_url.as_deref(),
            Some("https://test.example/bg.png"),
            "the background url round trips"
        );
        assert_eq!(
            user.birthday,
            NaiveDate::from_ymd_opt(1990, 1, 31),
            "a date column round trips"
        );
        assert_eq!(
            user.moved_to_ap_id.as_deref(),
            Some("https://other.example/users/alice"),
            "the move target round trips"
        );
        assert_eq!(
            user.last_move_at,
            Some(active_at),
            "the last-move timestamp round trips"
        );
        assert_eq!(
            user.last_active_at,
            Some(active_at),
            "the last-active timestamp round trips"
        );
        assert_eq!(
            user.confirmation_token.as_deref(),
            Some(SECRET_CONFIRMATION),
            "the confirmation token round trips even though Debug redacts it"
        );
        assert_eq!(
            user.confirmation_sent_at,
            Some(active_at),
            "the confirmation send stamp round trips"
        );
        assert_eq!(
            user.mfa_settings,
            json!({
                "enabled": true,
                "totp": {"secret": "BASE32SECRET", "confirmed": true},
                "backup_codes": ["0123456789abcdef"],
            }),
            "the mfa_settings blob round trips through jsonb"
        );
    }

    /// The Pleroma extension columns round trip through [`insert`]: arrays
    /// keep their order, the jsonb blob keeps its shape, the date and the
    /// timestamp come back as stored, and every flag keeps its value.
    #[tokio::test]
    async fn pleroma_columns_round_trip() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let active_at = Utc::now().trunc_subsecs(6);
        let new = NewUser {
            hide_followers: Flag::new(true),
            hide_follows: Flag::new(true),
            hide_followers_count: Flag::new(true),
            hide_follows_count: Flag::new(true),
            hide_favorites: Flag::new(false),
            allow_following_move: Flag::new(false),
            skip_thread_containment: Flag::new(true),
            accepts_chat_messages: Flag::new(false),
            show_role: Flag::new(false),
            no_rich_text: Flag::new(true),
            default_scope: "private".to_owned(),
            sensitive_default: Flag::new(true),
            language: Some("eo".to_owned()),
            tags: vec!["gnw".to_owned(), "rust".to_owned()],
            settings_store: json!({"pleroma_fe": {"theme": "dark"}}),
            background_image_url: Some(format!("{BASE}/bg.png")),
            birthday: NaiveDate::from_ymd_opt(1990, 1, 31),
            show_birthday: Flag::new(true),
            also_known_as: vec![local_ap_id(BASE, "alicia"), local_ap_id(BASE, "alice2")],
            moved_to_ap_id: Some("https://other.example/users/alice".to_owned()),
            last_move_at: Some(active_at),
            email_confirmed: Flag::new(false),
            approval_pending: Flag::new(true),
            confirmation_token: Some(SECRET_CONFIRMATION.to_owned()),
            confirmation_sent_at: Some(active_at),
            registration_reason: Some("please let me in".to_owned()),
            last_active_at: Some(active_at),
            mfa_settings: json!({
                "enabled": true,
                "totp": {"secret": "BASE32SECRET", "confirmed": true},
                "backup_codes": ["0123456789abcdef"],
            }),
            email_notifications: Flag::new(false),
            ..local_new("dora")
        };
        let user = insert(&pool, &new).await.unwrap();
        assert_pleroma_flags_round_trip(&user);
        assert_pleroma_values_round_trip(&user, active_at);
        assert_eq!(
            user.registration_reason.as_deref(),
            Some("please let me in"),
            "the registration reason round trips"
        );
    }

    #[tokio::test]
    async fn duplicate_nickname_in_other_case_is_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("Alice")).await.unwrap();
        let err = insert(&pool, &local_new("aLiCe")).await.unwrap_err();
        let AppError::Database(SqlxError::Database(db_err)) = err else {
            panic!("a nickname collision must surface as a database error")
        };
        assert_eq!(
            db_err.code().as_deref(),
            Some("23505"),
            "the duplicate is the citext unique constraint, not some other failure"
        );

        let rows = get_many(&pool, &[alice.id]).await.unwrap();
        assert_eq!(rows.len(), 1, "the rejected insert stored nothing");
        let stored = get_by_nickname(&pool, "aLiCe")
            .await
            .unwrap()
            .expect("citext lookup folds case");
        assert_eq!(stored.id, alice.id, "the first spelling owns the row");
        assert_eq!(stored.nickname, "Alice", "the stored case survives citext");
    }
}
