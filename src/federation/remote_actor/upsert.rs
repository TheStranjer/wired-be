//! [`upsert_remote_user`]: storing a parsed actor as the instance's mirror
//! row.

use sqlx::PgPool;

use super::{ActorError, NewRemoteUser};
use crate::{
    federation::json::http_host,
    models::{instance, user::User},
    util::id::new_id,
};

/// The upsert behind [`upsert_remote_user`]: insert keyed by a fresh
/// `UUIDv7`, or, when `ap_id` already has a row, overwrite exactly the
/// profile, key and endpoint columns the actor document describes plus
/// `last_fetched_at` and `updated_at`. `local`, `bot`, the login columns,
/// the moderation roles and the denormalised counters are deliberately not
/// touched: re-fetching a remote actor must not undo anything this instance
/// knows beyond the document itself.
const UPSERT_SQL: &str = "INSERT INTO users (
     id, nickname, ap_id, local, domain, actor_type, name, bio, raw_bio,
     avatar_url, banner_url, profile_url, locked, discoverable, fields,
     inbox, shared_inbox, outbox, followers_url, following_url,
     featured_url, public_key, last_fetched_at
 ) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
     $16, $17, $18, $19, $20, $21, $22, now()
 ) ON CONFLICT (ap_id) DO UPDATE SET
     nickname = EXCLUDED.nickname,
     domain = EXCLUDED.domain,
     actor_type = EXCLUDED.actor_type,
     name = EXCLUDED.name,
     bio = EXCLUDED.bio,
     raw_bio = EXCLUDED.raw_bio,
     avatar_url = EXCLUDED.avatar_url,
     banner_url = EXCLUDED.banner_url,
     profile_url = EXCLUDED.profile_url,
     locked = EXCLUDED.locked,
     discoverable = EXCLUDED.discoverable,
     fields = EXCLUDED.fields,
     inbox = EXCLUDED.inbox,
     shared_inbox = EXCLUDED.shared_inbox,
     outbox = EXCLUDED.outbox,
     followers_url = EXCLUDED.followers_url,
     following_url = EXCLUDED.following_url,
     featured_url = EXCLUDED.featured_url,
     public_key = EXCLUDED.public_key,
     last_fetched_at = now(),
     updated_at = now()
 RETURNING *";

/// The ownership probe behind [`upsert_remote_user`]'s
/// [`ActorError::NicknameConflict`]: the actor id (if any) of a row whose
/// citext nickname is the one the document claims but which is keyed by a
/// *different* `ap_id`. Matching the same row's own id is the ordinary
/// update path, including when the remote renamed `preferredUsername`.
const NICKNAME_HOLDER_SQL: &str =
    "SELECT ap_id FROM users WHERE nickname = $1::citext AND ap_id <> $2";

/// Stores `new` as this instance's mirror of the actor, exactly once per
/// actor id (decision 7).
///
/// The first fetch inserts a row with a fresh `UUIDv7` (decision 4); later
/// fetches apply to the row with the same `ap_id`, updating the profile
/// fields, keys and inbox endpoints in place and stamping
/// `last_fetched_at = now()`.
///
/// Before writing, the probe `NICKNAME_HOLDER_SQL` checks who owns the
/// nickname: a row other than this actor's own wins
/// [`ActorError::NicknameConflict`] instead of being renamed or overwritten.
///
/// After the row is stored, [`instance::touch`] records first sight of the
/// host of [`ap_id`](NewRemoteUser::ap_id) (or notes that a known host was
/// seen again): mirroring a remote user is what makes its instance exist in
/// the ledger. An `ap_id` with no usable host — nothing [`http_host`] can
/// read — names no instance to record, so the touch is skipped and the
/// upsert still answers `Ok`: the user row is stored either way and no
/// host is invented from the nickname.
///
/// # Errors
///
/// [`ActorError::NicknameConflict`] when another `ap_id` already holds this
/// nickname; [`ActorError::Failed`] for any database failure, including the
/// instance record.
pub async fn upsert_remote_user(pool: &PgPool, new: NewRemoteUser) -> Result<User, ActorError> {
    let holder: Option<String> = sqlx::query_scalar(NICKNAME_HOLDER_SQL)
        .bind(new.nickname.as_str())
        .bind(new.ap_id.as_str())
        .fetch_optional(pool)
        .await?;
    if holder.is_some() {
        return Err(ActorError::NicknameConflict);
    }
    let user = sqlx::query_as::<_, User>(UPSERT_SQL)
        .bind(new_id())
        .bind(new.nickname.as_str())
        .bind(new.ap_id.as_str())
        .bind(new.local)
        .bind(new.domain.as_str())
        .bind(new.actor_type.as_str())
        .bind(new.name.as_str())
        .bind(new.bio.as_str())
        .bind(new.raw_bio.as_str())
        .bind(new.avatar_url.as_deref())
        .bind(new.banner_url.as_deref())
        .bind(new.profile_url.as_deref())
        .bind(new.locked)
        .bind(new.discoverable)
        .bind(new.fields)
        .bind(new.inbox.as_str())
        .bind(new.shared_inbox.as_deref())
        .bind(new.outbox.as_deref())
        .bind(new.followers_url.as_deref())
        .bind(new.following_url.as_deref())
        .bind(new.featured_url.as_deref())
        .bind(new.public_key.as_str())
        .fetch_one(pool)
        .await?;
    // First sight of the mirrored user touches its instance; an `ap_id`
    // [`http_host`] cannot read names no host, and the record is skipped
    // (see the function docs).
    if let Some(host) = http_host(&new.ap_id) {
        instance::touch(pool, &host).await?;
    }
    Ok(user)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta, Utc};
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        federation::remote_actor::{
            parse_actor,
            test_support::{MASTODON, PLEROMA, fixture},
        },
        models::user::{
            CountDelta, NewUser, get_by_ap_id, get_by_id, get_by_nickname, insert, update_counts,
        },
    };

    /// The mirror row the first fetch of a Mastodon actor stores: every
    /// column read off the document, and none of the secrets a local account
    /// has.
    fn assert_mirror_of(first: &User, document: &Value) {
        assert_eq!(
            first.nickname, "alice@mastodon.example",
            "the parsed handle"
        );
        assert_eq!(
            first.ap_id, "https://mastodon.example/users/alice",
            "the parsed id"
        );
        assert!(!first.local.get(), "a mirror row is not local");
        assert_eq!(
            first.domain.as_deref(),
            Some("mastodon.example"),
            "the host"
        );
        assert!(
            first.private_key.is_none() && first.email.is_none() && first.password_hash.is_none(),
            "a remote row carries no local secrets"
        );
        assert_eq!(
            first.public_key.as_deref(),
            document["publicKey"]["publicKeyPem"].as_str(),
            "the key is stored"
        );
        assert_eq!(
            first.inbox.as_deref(),
            Some("https://mastodon.example/users/alice/inbox"),
            "the inbox endpoint"
        );
        assert_eq!(
            first.shared_inbox.as_deref(),
            Some("https://mastodon.example/inbox"),
            "the shared inbox endpoint"
        );
        assert!(first.last_fetched_at.is_some(), "the fetch is stamped");
        assert!(
            first.discoverable.get() && !first.locked.get(),
            "the flags moved"
        );
    }

    #[tokio::test]
    async fn upsert_inserts_then_updates_the_same_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut document = fixture(MASTODON);
        let first = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert_mirror_of(&first, &document);

        // Counters move through update_counts only: a re-fetch must not
        // reset them (nor touch the login/role columns, absent here).
        update_counts(
            &pool,
            first.id,
            CountDelta {
                followers: 5,
                following: 0,
                statuses: 0,
            },
        )
        .await
        .unwrap();

        document["name"] = json!("Alice the Second");
        document["summary"] = json!("<p>new <b>bio</b></p>");
        let second = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert_eq!(
            second.id, first.id,
            "the existing row is updated, not replaced"
        );
        assert_eq!(second.name, "Alice the Second", "the new name landed");
        assert_eq!(second.bio, "<p>new <b>bio</b></p>", "the new note landed");
        assert_eq!(second.raw_bio, "new bio\n", "with its source");
        assert_eq!(second.followers_count, 5, "the counter survived the update");
        assert!(
            second.updated_at >= first.inserted_at,
            "updated_at refreshed"
        );

        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 1, "upserting twice leaves exactly one row");
        let by_nickname = get_by_nickname(&pool, "alice@mastodon.example")
            .await
            .unwrap()
            .expect("the handle finds the mirror");
        assert_eq!(by_nickname.id, first.id, "one row, both lookups");
    }

    #[tokio::test]
    async fn upsert_follows_a_rename_of_the_same_actor() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut document = fixture(MASTODON);
        let first = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        document["preferredUsername"] = json!("alice2");
        let renamed = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert_eq!(renamed.id, first.id, "a rename updates, it does not insert");
        assert_eq!(
            renamed.nickname, "alice2@mastodon.example",
            "the handle follows"
        );
        assert!(
            get_by_nickname(&pool, "alice@mastodon.example")
                .await
                .unwrap()
                .is_none(),
            "the old handle is free again"
        );
    }

    #[tokio::test]
    async fn upsert_reports_a_nickname_conflict() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let squatter = insert(
            &pool,
            &NewUser::local(
                "alice@mastodon.example",
                "https://other.example/users/alice",
            ),
        )
        .await
        .unwrap();
        let error = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(error, ActorError::NicknameConflict),
            "another ap_id owns the handle: {error:?}"
        );
        assert!(
            get_by_ap_id(&pool, "https://mastodon.example/users/alice")
                .await
                .unwrap()
                .is_none(),
            "the mirror was not stored"
        );
        let still = get_by_id(&pool, squatter.id)
            .await
            .unwrap()
            .expect("the squatter stands");
        assert_eq!(
            still.ap_id, "https://other.example/users/alice",
            "and was not renamed"
        );
    }

    #[tokio::test]
    async fn upsert_keeps_distinct_actors_distinct() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap();
        let bob = upsert_remote_user(&pool, parse_actor(&fixture(PLEROMA)).unwrap())
            .await
            .unwrap();
        assert_ne!(alice.id, bob.id, "two actors, two rows");
        assert_eq!(bob.actor_type, "Service", "each keeps its document");
        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 2, "and nothing else was inserted");
    }

    /// The `instances` row of `host`: `inserted_at`, `updated_at` and its
    /// mark.
    async fn instance_row(
        pool: &PgPool,
        host: &str,
    ) -> (DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>) {
        sqlx::query_as::<_, (DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>)>(
            "SELECT inserted_at, updated_at, unreachable_since FROM instances WHERE host = $1",
        )
        .bind(host)
        .fetch_one(pool)
        .await
        .expect("the mirrored host has a row")
    }

    /// The first upsert of a remote actor first-sights its instance: the
    /// row exists the moment the user does, and carries no mark — a host
    /// this instance has merely mirrored is reachable; delivery failures
    /// make the marks, not fetches.
    #[tokio::test]
    async fn a_first_upsert_creates_the_instance_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap();
        assert_eq!(
            user.domain.as_deref(),
            Some("mastodon.example"),
            "the mirror row stores"
        );
        let (inserted, updated, mark) = instance_row(&pool, "mastodon.example").await;
        assert!(mark.is_none(), "a mirrored host is first-sighted unmarked");
        assert_eq!(
            inserted, updated,
            "the row is stamped once, by its own insert"
        );
    }

    /// The second upsert of the same actor is a re-sighting: it bumps the
    /// instance row's `updated_at` and writes nothing else — first sight
    /// stands and the row stays unmarked, on the same user row as before.
    #[tokio::test]
    async fn a_second_upsert_only_bumps_the_instance_updated_at() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let first = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap();
        // Age the row's stamps so the re-sighting's bump is an observable
        // move rather than a race with the clock; the comparisons below
        // read the aged values back, which is what the columns hold.
        let _aged =
            sqlx::query("UPDATE instances SET inserted_at = $2, updated_at = $2 WHERE host = $1")
                .bind("mastodon.example")
                .bind(Utc::now() - TimeDelta::days(1))
                .execute(&pool)
                .await
                .expect("the stamps are settable");
        let (inserted, aged, mark) = instance_row(&pool, "mastodon.example").await;
        assert!(mark.is_none(), "nothing marked the aged row");

        let second = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap();

        let (inserted_now, updated_now, mark_now) = instance_row(&pool, "mastodon.example").await;
        assert_eq!(second.id, first.id, "the same mirror row, updated");
        assert_eq!(inserted_now, inserted, "first sight is never rewritten");
        assert!(updated_now > aged, "the re-sighting bumps updated_at");
        assert!(mark_now.is_none(), "and the row stays unmarked");
    }

    /// An `ap_id` [`http_host`] cannot read names no instance: the stored
    /// behavior is to skip the record and answer `Ok` — the user row goes
    /// in like any other and no host is invented from the nickname.
    #[tokio::test]
    async fn an_upsert_with_a_hostless_ap_id_skips_the_instance_record() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut new = parse_actor(&fixture(MASTODON)).unwrap();
        new.ap_id = "mailto:alice@mastodon.example".to_owned();

        let user = upsert_remote_user(&pool, new).await.unwrap();

        assert_eq!(
            user.ap_id, "mailto:alice@mastodon.example",
            "the user row is stored all the same"
        );
        let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instances")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "and the instance record was skipped");
    }
}
