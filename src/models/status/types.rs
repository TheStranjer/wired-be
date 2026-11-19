//! The stored shapes of `statuses`: the [`Status`] row, the [`Visibility`]
//! it stores, and the small value types the sibling reads answer with
//! ([`Counter`], [`StatusVisibilityCounts`], [`ReblogPageEntry`]).

use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::user::User;

/// The `text/plain` value of [`content_type`](super::NewStatus::content_type), which
/// is also the column default.
pub const DEFAULT_CONTENT_TYPE: &str = "text/plain";

/// Who may see a [`Status`], stored in the `visibility` text column as its
/// lowercase variant name (`public`, `unlisted`, `private`, `direct` or
/// `local`, which is exactly what the column's `CHECK` constraint allows).
///
/// The four Mastodon visibilities keep their Mastodon meaning;
/// [`Visibility::Local`] is Pleroma's extension for a post that is never
/// federated (`lib/pleroma/activity.ex`, the `visibility` field).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Visibility {
    /// Addressed to everyone and shown in public timelines.
    Public,
    /// Addressed to everyone but left out of public timelines.
    Unlisted,
    /// Addressed to the author's followers only.
    Private,
    /// Addressed to the mentioned accounts only.
    Direct,
    /// Kept on this instance: addressed to everyone, delivered to nobody
    /// else (Pleroma's `local`).
    Local,
}

/// The message [`FromStr`] answers for text the column would not accept.
const UNKNOWN_VISIBILITY: &str = "unknown visibility";

impl Visibility {
    /// All five visibilities, in the order the column's `CHECK` constraint
    /// lists them.
    pub const ALL: [Self; 5] = [
        Self::Public,
        Self::Unlisted,
        Self::Private,
        Self::Direct,
        Self::Local,
    ];

    /// The stored (and wire) text of this visibility: its lowercase name,
    /// which is what the `visibility` column holds and what Mastodon's JSON
    /// reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Unlisted => "unlisted",
            Self::Private => "private",
            Self::Direct => "direct",
            Self::Local => "local",
        }
    }
}

/// [`as_str`](Self::as_str) is the text form, so `format!("{visibility}")`
/// and the database agree.
impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses the stored text back, case-sensitively: the column only ever holds
/// these five spellings and clients send them verbatim.
impl FromStr for Visibility {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "public" => Ok(Self::Public),
            "unlisted" => Ok(Self::Unlisted),
            "private" => Ok(Self::Private),
            "direct" => Ok(Self::Direct),
            "local" => Ok(Self::Local),
            _ => Err(UNKNOWN_VISIBILITY),
        }
    }
}

/// One denormalised counter of a [`Status`], for [`bump_counter`](super::bump_counter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    /// [`replies_count`](Status::replies_count): live answers to this status.
    Replies,
    /// [`reblogs_count`](Status::reblogs_count): statuses reblogging it.
    Reblogs,
    /// [`favourites_count`](Status::favourites_count): favourites of it.
    Favourites,
    /// [`quotes_count`](Status::quotes_count): statuses quoting it.
    Quotes,
}

/// The four live-status counters behind `GET /api/v1/pleroma/admin/stats`'
/// `status_visibility` block: one row of Pleroma's `CounterCache`
/// (`lib/pleroma/counter_cache.ex`), read straight off `statuses`.
#[derive(Debug, sqlx::FromRow)]
pub struct StatusVisibilityCounts {
    /// Live, non-reblog `public` statuses of the selected audience.
    pub public: i64,
    /// Live, non-reblog `unlisted` statuses of the selected audience.
    pub unlisted: i64,
    /// Live, non-reblog `private` statuses of the selected audience.
    pub private: i64,
    /// Live, non-reblog `direct` statuses of the selected audience.
    pub direct: i64,
}

/// One row of `statuses`: one post, in any of the shapes decision 6 allows
/// (root, reply, reblog, quote).
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Status {
    /// `UUIDv7` primary key, minted by [`insert`](super::insert) (decision 4).
    pub id: Uuid,
    /// `ActivityPub` object id: `{BASE_URL}/objects/{uuid}` for a local
    /// status, the remote's own id for a mirrored one. Unique, and it stays
    /// reserved across a soft delete.
    pub uri: String,
    /// Human-facing URL of the post (`{BASE_URL}/notice/{uuid}` locally);
    /// `None` when the origin named none.
    pub url: Option<String>,
    /// Author.
    pub user_id: Uuid,
    /// The post as sanitized HTML; emptied by [`soft_delete`](super::soft_delete).
    pub content: String,
    /// The post as its source text, before formatting.
    pub raw_text: String,
    /// `MIME` type [`raw_text`](Self::raw_text) was written in.
    pub content_type: String,
    /// Content warning subject line; empty means there is none.
    pub spoiler_text: String,
    /// Whether [`spoiler_text`](Self::spoiler_text) hides
    /// [`content`](Self::content) until the reader opens it.
    pub sensitive: bool,
    /// Who may see this post.
    pub visibility: Visibility,
    /// BCP 47 language tag of the content, when known.
    pub language: Option<String>,
    /// Status this one answers; `None` for a root status.
    pub in_reply_to_id: Option<Uuid>,
    /// Author of the answered status, when this instance knows them.
    pub in_reply_to_account_id: Option<Uuid>,
    /// `ActivityPub` id of the answered status while it is not stored yet.
    pub in_reply_to_ap_id: Option<String>,
    /// Thread group id: every status of one conversation shares it, and a
    /// root status's is its own id.
    pub conversation_id: Uuid,
    /// The `ActivityPub` thread collection (`context` / `conversation` /
    /// `ostatus:conversation`) a mirrored status names, `None` for a local
    /// status and for a remote one whose document carried no context (see
    /// [`crate::services::ingest`]).
    pub ap_context: Option<String>,
    /// The `conversations` row this `direct` status posts into, `None` for
    /// every other visibility and for a direct status written outside the
    /// participation hook (see [`crate::models::conversation`]).
    pub direct_conversation_id: Option<Uuid>,
    /// Original this status reblogs; `None` for anything but a reblog.
    pub reblog_of_id: Option<Uuid>,
    /// Original this status quotes; `None` for anything but a quote.
    pub quote_of_id: Option<Uuid>,
    /// Whether this instance owns the post.
    pub local: bool,
    /// Name of the client that posted this status, if it reported one.
    pub application_name: Option<String>,
    /// Website of that client, if it reported one.
    pub application_website: Option<String>,
    /// Live answers to this post; moved by [`insert`](super::insert), [`soft_delete`](super::soft_delete) and
    /// [`bump_counter`](super::bump_counter).
    pub replies_count: i32,
    /// Reblogs of this post; moved by the same three.
    pub reblogs_count: i32,
    /// Favourites of this post; moved by [`bump_counter`](super::bump_counter).
    pub favourites_count: i32,
    /// Quotes of this post; moved by [`insert`](super::insert) and [`soft_delete`](super::soft_delete) with the
    /// quoting row, like [`replies_count`](Self::replies_count), and by
    /// [`bump_counter`](super::bump_counter) like any other.
    pub quotes_count: i32,
    /// When the content last changed; `None` until an edit.
    pub edited_at: Option<DateTime<Utc>>,
    /// When this post was deleted; `None` while it is live.
    pub deleted_at: Option<DateTime<Utc>>,
    /// When this post is due to disappear; `None` means it never does.
    pub expires_at: Option<DateTime<Utc>>,
    /// Creation time: for a remote status, the `published` timestamp it
    /// arrived with.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every update here.
    pub updated_at: DateTime<Utc>,
}

/// One entry of [`reblogger_page`](super::reblogger_page): the [`User`] behind a reblog row, led
/// by that row's id.
///
/// The id is what the endpoint's keyset cursors and `Link` header carry —
/// the reblog row's id, not the reblogger's — exactly as
/// [`crate::models::favourite`] hands the like id over next to the account.
#[derive(Debug, sqlx::FromRow)]
pub struct ReblogPageEntry {
    /// Primary key of the reblog [`Status`] row behind this entry.
    pub reblog_id: Uuid,
    /// The account that reblogged.
    #[sqlx(flatten)]
    pub user: User,
}

#[cfg(test)]
mod tests {
    use chrono::{SubsecRound as _, Utc};

    use super::*;
    use crate::{
        error::AppError,
        models::status::{NewStatus, get_by_id, insert, test_support::*},
        util::id::new_id,
    };

    #[tokio::test]
    async fn every_visibility_round_trips() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        for visibility in Visibility::ALL {
            let status = insert(
                &pool,
                BASE,
                &NewStatus {
                    visibility,
                    ..NewStatus::local(&alice, "text")
                },
            )
            .await
            .unwrap();

            assert_eq!(
                stored(&pool, status.id).await.visibility,
                visibility,
                "the {visibility} enum value comes back"
            );
            let text: (String,) = sqlx::query_as("SELECT visibility FROM statuses WHERE id = $1")
                .bind(status.id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(text.0, visibility.as_str(), "stored as its lowercase name");
            assert_eq!(
                text.0.parse::<Visibility>().ok(),
                Some(visibility),
                "the text parses back to the same variant"
            );
            assert_eq!(
                visibility.to_string(),
                visibility.as_str(),
                "Display is as_str"
            );
            assert!(
                get_by_id(&pool, status.id).await.unwrap().is_some(),
                "no visibility hides a live status from get_by_id"
            );
        }
    }

    #[tokio::test]
    async fn the_column_rejects_a_visibility_it_does_not_know() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = user(&pool, "alice").await;
        let id = new_id();
        let err = sqlx::query(
            "INSERT INTO statuses (id, uri, user_id, conversation_id, local, visibility)
             VALUES ($1, $2, $3, $1, true, 'audience')",
        )
        .bind(id)
        .bind(format!("{BASE}/objects/bogus"))
        .bind(alice.id)
        .execute(&pool)
        .await
        .expect_err("an unknown visibility is not storable");
        assert_eq!(
            sqlstate_of(AppError::from(err)),
            "23514",
            "the failure is the visibility CHECK constraint"
        );
    }

    #[test]
    fn visibility_text_maps_both_ways() {
        assert_eq!(Visibility::Public.as_str(), "public");
        assert_eq!(Visibility::Local.as_str(), "local");
        for text in ["public", "unlisted", "private", "direct", "local"] {
            let parsed: Visibility = text.parse().expect("the five known spellings parse");
            assert_eq!(
                parsed.as_str(),
                text,
                "text -> enum -> text is the identity"
            );
        }
        for text in ["", "Public", "AUDIENCE", "list", "local "] {
            assert!(
                text.parse::<Visibility>().is_err(),
                "{text} is not a visibility"
            );
        }
    }

    #[tokio::test]
    async fn a_mirrored_status_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mallory = remote_user(&pool, "mallory").await;
        let published = (Utc::now() - chrono::Duration::days(3)).trunc_subsecs(6);
        let expires = (Utc::now() + chrono::Duration::days(1)).trunc_subsecs(6);
        let uri = "https://remote.example/posts/42";

        let status = insert(
            &pool,
            BASE,
            &NewStatus {
                url: Some("https://remote.example/notice/42".to_owned()),
                spoiler_text: "cats".to_owned(),
                sensitive: true,
                visibility: Visibility::Local,
                language: Some("de".to_owned()),
                in_reply_to_ap_id: Some("https://remote.example/posts/41".to_owned()),
                conversation_id: Some(new_id()),
                application_name: Some("RemoteClient".to_owned()),
                application_website: Some("https://remote.example/client".to_owned()),
                edited_at: Some(published),
                expires_at: Some(expires),
                inserted_at: Some(published),
                ..mirrored(&mallory, uri)
            },
        )
        .await
        .unwrap();

        assert_eq!(status.uri, uri, "a remote status keeps the remote id");
        assert_eq!(
            status.url.as_deref(),
            Some("https://remote.example/notice/42"),
            "and the remote's human URL"
        );
        assert!(!status.local, "the post stays marked remote");
        assert_eq!(
            status.spoiler_text, "cats",
            "the content warning round trips"
        );
        assert!(status.sensitive, "so does the sensitive flag");
        assert_eq!(
            status.visibility,
            Visibility::Local,
            "Pleroma's local visibility round trips"
        );
        assert_eq!(status.language.as_deref(), Some("de"), "the language tag");
        assert_eq!(
            status.in_reply_to_ap_id.as_deref(),
            Some("https://remote.example/posts/41"),
            "an unsaved reply target is kept by id"
        );
        assert_eq!(
            status.application_name.as_deref(),
            Some("RemoteClient"),
            "the application block round trips"
        );
        assert_eq!(
            status.application_website.as_deref(),
            Some("https://remote.example/client"),
            "both halves of it"
        );
        assert_eq!(status.edited_at, Some(published), "edited_at round trips");
        assert_eq!(status.expires_at, Some(expires), "expires_at round trips");
        assert_eq!(
            status.inserted_at, published,
            "inserted_at is the remote published time"
        );

        let counted = fresh_user(&pool, mallory.id).await;
        assert_eq!(counted.statuses_count, 1, "a mirrored post counts too");
        assert_eq!(
            counted.last_status_at,
            Some(published),
            "and it sets last_status_at to when it was published"
        );
    }
}
