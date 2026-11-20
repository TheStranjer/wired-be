//! The profile write behind `PATCH /api/v1/accounts/update_credentials`:
//! [`ProfileFields`] (with its [`PleromaPrefs`] block) carries every
//! profile column the endpoint owns and [`update_profile`] stores them in
//! one statement.

use chrono::NaiveDate;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::{Flag, User};
use crate::error::AppResult;

/// The eleven Pleroma preference flags `update_credentials` writes: the
/// body of the `pleroma` block of the account entity, as the account
/// itself sets them.
///
/// Grouped out of [`ProfileFields`] because the endpoint merges them as one
/// set (every one is the same present-or-keep-the-stored-value boolean) and
/// because [`ProfileFields`] already carries the rest of the
/// profile.
#[derive(Clone, Copy, Debug)]
pub struct PleromaPrefs {
    /// Whether the follower collection is hidden from other accounts.
    pub hide_followers: Flag,
    /// Whether the follow collection is hidden from other accounts.
    pub hide_follows: Flag,
    /// Whether [`followers_count`](User::followers_count) renders as zero
    /// for everyone but the owner.
    pub hide_followers_count: Flag,
    /// Whether [`following_count`](User::following_count) renders as zero
    /// for everyone but the owner.
    pub hide_follows_count: Flag,
    /// Whether the favourites list is private to the account.
    pub hide_favorites: Flag,
    /// Whether the admin/moderator flags may be shown to other accounts.
    pub show_role: Flag,
    /// Whether this account's statuses skip thread containment.
    pub skip_thread_containment: Flag,
    /// Whether a `Move` of this account may take the following list with
    /// it.
    pub allow_following_move: Flag,
    /// Whether the account accepts chat messages.
    pub accepts_chat_messages: Flag,
    /// Whether the account asked clients not to render rich text.
    pub no_rich_text: Flag,
    /// Whether strangers may see [`birthday`](ProfileFields::birthday).
    pub show_birthday: Flag,
}

/// The whole `PATCH /api/v1/accounts/update_credentials` write: every
/// profile column the endpoint owns.
///
/// The text and images of the Mastodon profile, the posting defaults of the
/// `source` block, the eleven `pleroma` flags, the birthday and the client
/// settings store.
///
/// Every one of them arrives already merged with the stored row by the
/// caller, so fields absent from the request are rewritten with the value
/// they already hold and change nothing.
#[derive(Debug)]
pub struct ProfileFields {
    /// The display name to store, sanitized by the caller.
    pub name: String,
    /// The profile note as rendered HTML for the `bio` column.
    pub bio: String,
    /// The profile note as its source text for the `raw_bio` column.
    pub raw_bio: String,
    /// Whether follow requests need manual approval.
    pub locked: Flag,
    /// Whether the account is flagged as automated.
    pub bot: Flag,
    /// Whether the account opted into discovery.
    pub discoverable: Flag,
    /// The profile metadata as rendered HTML entries for the `fields`
    /// column: a JSON array of `{name, value}` objects.
    pub fields: Value,
    /// The profile metadata as its source text for the `raw_fields`
    /// column: the same entries as [`fields`](Self::fields), with the
    /// values and names exactly as typed.
    pub raw_fields: Value,
    /// The avatar URL for the `avatar_url` column: the stored one when the
    /// body carried no `avatar` parameter, the public URL of a newly stored
    /// upload when it carried a file, `None` when it cleared the image.
    pub avatar_url: Option<String>,
    /// The banner URL for the `banner_url` column, the three states of
    /// [`avatar_url`](Self::avatar_url) read from the body's `header`.
    pub banner_url: Option<String>,
    /// The background URL for the `background_image_url` column, the three
    /// states of [`avatar_url`](Self::avatar_url) read from the body's
    /// `pleroma_background_image`.
    pub background_image_url: Option<String>,
    /// The visibility new statuses default to, for the `default_scope`
    /// column (the entity's `source.privacy`): one of `public`, `unlisted`,
    /// `private` or `direct`, validated by the caller.
    pub default_scope: String,
    /// Whether new statuses are flagged sensitive by default, for the
    /// `sensitive_default` column (the entity's `source.sensitive`).
    pub sensitive_default: Flag,
    /// The BCP 47 language tag new statuses default to, for the `language`
    /// column (the entity's `source.language`); `None` stores `NULL`, the
    /// account stating no preference.
    pub language: Option<String>,
    /// The eleven `pleroma` preference flags of [`PleromaPrefs`], one column
    /// each.
    pub pleroma: PleromaPrefs,
    /// The account's birthday for the `birthday` column; `None` stores
    /// `NULL`, which is how the endpoint clears the date.
    pub birthday: Option<NaiveDate>,
    /// The whole client settings blob for the `settings_store` column: the
    /// endpoint merges the submitted object over the stored one and the
    /// write stores the result.
    pub settings_store: Value,
    /// The `ActivityPub` actor kind for the `actor_type` column: `Person` or
    /// `Service`, the pair the endpoint keeps in step with [`bot`](Self::bot).
    pub actor_type: String,
}

/// The `UPDATE` behind [`update_profile`]: writes the profile columns in one
/// statement, refreshes `updated_at` and returns the stored row.
const UPDATE_PROFILE_SQL: &str = "UPDATE users
      SET name = $2,
          bio = $3,
          raw_bio = $4,
          locked = $5,
          bot = $6,
          discoverable = $7,
          fields = $8,
          raw_fields = $9,
          avatar_url = $10,
          banner_url = $11,
          background_image_url = $12,
          default_scope = $13,
          sensitive_default = $14,
          language = $15,
          actor_type = $16,
          birthday = $17,
          settings_store = $18,
          hide_followers = $19,
          hide_follows = $20,
          hide_followers_count = $21,
          hide_follows_count = $22,
          hide_favorites = $23,
          show_role = $24,
          skip_thread_containment = $25,
          allow_following_move = $26,
          accepts_chat_messages = $27,
          no_rich_text = $28,
          show_birthday = $29,
          updated_at = now()
     WHERE id = $1
  RETURNING *";

/// Stores `fields` as user `id`'s profile columns and returns the updated
/// row.
///
/// `None` when `id` names no row (the caller authenticated it moments
/// earlier, so that only happens if the account was deleted mid-request).
/// The endpoint layer computes the merged values — this function writes
/// exactly what it is handed, the same split the alias and
/// settings-store writers use.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update_profile(
    pool: &PgPool,
    id: Uuid,
    fields: &ProfileFields,
) -> AppResult<Option<User>> {
    Ok(sqlx::query_as::<_, User>(UPDATE_PROFILE_SQL)
        .bind(id)
        .bind(&fields.name)
        .bind(&fields.bio)
        .bind(&fields.raw_bio)
        .bind(fields.locked)
        .bind(fields.bot)
        .bind(fields.discoverable)
        .bind(fields.fields.clone())
        .bind(fields.raw_fields.clone())
        .bind(fields.avatar_url.as_deref())
        .bind(fields.banner_url.as_deref())
        .bind(fields.background_image_url.as_deref())
        .bind(fields.default_scope.as_str())
        .bind(fields.sensitive_default)
        .bind(fields.language.as_deref())
        .bind(fields.actor_type.as_str())
        .bind(fields.birthday)
        .bind(fields.settings_store.clone())
        .bind(fields.pleroma.hide_followers)
        .bind(fields.pleroma.hide_follows)
        .bind(fields.pleroma.hide_followers_count)
        .bind(fields.pleroma.hide_follows_count)
        .bind(fields.pleroma.hide_favorites)
        .bind(fields.pleroma.show_role)
        .bind(fields.pleroma.skip_thread_containment)
        .bind(fields.pleroma.allow_following_move)
        .bind(fields.pleroma.accepts_chat_messages)
        .bind(fields.pleroma.no_rich_text)
        .bind(fields.pleroma.show_birthday)
        .fetch_optional(pool)
        .await?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        models::user::{insert, test_support::*},
        util::id::new_id,
    };

    /// A [`ProfileFields`] that sets every profile column to something
    /// distinct: images, notes, rendered and raw fields, the posting defaults,
    /// the actor type and every Pleroma preference.
    fn all_profile_fields() -> ProfileFields {
        ProfileFields {
            name: "Alice Adams".to_owned(),
            bio: "<p>hi</p>".to_owned(),
            raw_bio: "hi".to_owned(),
            locked: Flag::new(true),
            bot: Flag::new(false),
            discoverable: Flag::new(true),
            fields: json!([{"name": "site", "value": "<p><a href=\"https://alice.example\" rel=\"ugc nofollow noopener noreferrer\">alice.example</a></p>"}]),
            raw_fields: json!([{"name": "site", "value": "https://alice.example"}]),
            avatar_url: Some(format!("{BASE}/media/alice/avatar.png")),
            banner_url: Some(format!("{BASE}/media/alice/banner.png")),
            background_image_url: Some(format!("{BASE}/media/alice/bg.png")),
            default_scope: "private".to_owned(),
            sensitive_default: Flag::new(true),
            language: Some("pt-BR".to_owned()),
            pleroma: PleromaPrefs {
                hide_followers: Flag::new(true),
                hide_follows: Flag::new(true),
                hide_followers_count: Flag::new(true),
                hide_follows_count: Flag::new(true),
                hide_favorites: Flag::new(false),
                show_role: Flag::new(false),
                skip_thread_containment: Flag::new(true),
                allow_following_move: Flag::new(false),
                accepts_chat_messages: Flag::new(false),
                no_rich_text: Flag::new(true),
                show_birthday: Flag::new(true),
            },
            birthday: NaiveDate::from_ymd_opt(1990, 1, 31),
            settings_store: json!({"pleroma_fe": {"theme": "dark"}, "bookmarks": true}),
            actor_type: "Service".to_owned(),
        }
    }

    /// Every column [`update_profile`] was handed, on the row it returned —
    /// and the `updated_at` the write moved on.
    fn assert_profile_written(updated: &User, before: &User) {
        assert_eq!(updated.name, "Alice Adams", "the returned row is updated");
        assert_eq!(updated.bio, "<p>hi</p>", "the html note");
        assert_eq!(updated.raw_bio, "hi", "the source note");
        assert!(updated.locked.get(), "locked set");
        assert!(!updated.bot.get(), "bot left off");
        assert!(updated.discoverable.get(), "discoverable set");
        assert_eq!(
            updated.fields,
            json!([{"name": "site", "value": "<p><a href=\"https://alice.example\" rel=\"ugc nofollow noopener noreferrer\">alice.example</a></p>"}]),
            "the rendered fields are written"
        );
        assert_eq!(
            updated.raw_fields,
            json!([{"name": "site", "value": "https://alice.example"}]),
            "the raw fields are written beside them"
        );
        assert_eq!(
            updated.avatar_url.as_deref(),
            Some("https://test.example/media/alice/avatar.png"),
            "the avatar URL is written"
        );
        assert_eq!(
            updated.banner_url.as_deref(),
            Some("https://test.example/media/alice/banner.png"),
            "the banner URL is written"
        );
        assert_eq!(
            updated.background_image_url.as_deref(),
            Some("https://test.example/media/alice/bg.png"),
            "the background URL is written"
        );
        assert_eq!(updated.default_scope, "private", "the posting default");
        assert!(updated.sensitive_default.get(), "the sensitive default");
        assert_eq!(updated.language.as_deref(), Some("pt-BR"), "the language");
        assert_eq!(updated.actor_type, "Service", "the actor type");
        assert_eq!(
            updated.birthday,
            NaiveDate::from_ymd_opt(1990, 1, 31),
            "the birthday"
        );
        assert_eq!(
            updated.settings_store,
            json!({"pleroma_fe": {"theme": "dark"}, "bookmarks": true}),
            "the settings store is written as handed in"
        );
        assert!(
            updated.hide_followers.get()
                && updated.hide_follows.get()
                && updated.hide_followers_count.get()
                && updated.hide_follows_count.get()
                && updated.show_birthday.get()
                && updated.skip_thread_containment.get()
                && updated.no_rich_text.get(),
            "every set flag round trips"
        );
        assert!(
            !updated.hide_favorites.get()
                && !updated.show_role.get()
                && !updated.allow_following_move.get()
                && !updated.accepts_chat_messages.get(),
            "and so does every cleared one"
        );
        assert!(
            updated.updated_at > before.updated_at,
            "the write refreshes updated_at"
        );
    }

    #[tokio::test]
    async fn update_profile_writes_every_profile_column_and_returns_them() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        assert_eq!(alice.name, "", "the fixture has no display name");
        assert!(
            !alice.locked.get() && !alice.bot.get() && !alice.discoverable.get(),
            "the option flags default off"
        );
        assert!(
            alice.avatar_url.is_none()
                && alice.banner_url.is_none()
                && alice.background_image_url.is_none(),
            "the fixture has no profile images"
        );
        let fields = all_profile_fields();
        let updated = update_profile(&pool, alice.id, &fields)
            .await
            .unwrap()
            .expect("the row exists");
        assert_profile_written(&updated, &alice);

        let stored = fetch(&pool, alice.id).await;
        assert_eq!(stored.name, updated.name, "the row is what reads back");
        assert_eq!(stored.bio, updated.bio, "stored bio");
        assert_eq!(stored.raw_bio, updated.raw_bio, "stored raw bio");
        assert_eq!(stored.fields, updated.fields, "stored fields");
        assert_eq!(stored.raw_fields, updated.raw_fields, "stored raw fields");
        assert_eq!(stored.avatar_url, updated.avatar_url, "stored avatar url");

        // An unknown id matches no row and is not an error.
        assert!(
            update_profile(&pool, new_id(), &fields)
                .await
                .unwrap()
                .is_none(),
            "no row, no update"
        );

        // The columns are plain nullable text: the merged value the caller
        // hands in is what stands, so `None` clears one image while the
        // other two keep what the caller merged over the row.
        let cleared = ProfileFields {
            avatar_url: None,
            ..fields
        };
        let after = update_profile(&pool, alice.id, &cleared)
            .await
            .unwrap()
            .expect("the row exists");
        assert!(after.avatar_url.is_none(), "the avatar column is cleared");
        assert_eq!(
            after.banner_url, stored.banner_url,
            "and the untouched ones keep their value"
        );
        assert_eq!(
            after.background_image_url, stored.background_image_url,
            "the background keeps its value"
        );
    }
}
