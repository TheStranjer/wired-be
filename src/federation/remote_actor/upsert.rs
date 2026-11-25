//! [`upsert_remote_user`]: storing a parsed actor as the instance's mirror
//! row.

use sqlx::PgPool;

use super::{ActorError, NewRemoteUser};
use crate::{
    federation::json::http_host,
    models::{activitypub_actor, custom_emoji, instance, user::User},
    util::id::new_id,
};

/// The upsert behind [`upsert_remote_user`]: insert keyed by a fresh
/// `UUIDv7`, or, when `ap_id` already has a row, overwrite exactly the
/// profile, key, endpoint, `alsoKnownAs`, `bot` and birthday columns the actor
/// document describes plus
/// `last_fetched_at` and `updated_at`. `local`, the login columns,
/// the moderation roles and the denormalised counters are deliberately not
/// touched: re-fetching a remote actor must not undo anything this instance
/// knows beyond the document itself.
const UPSERT_SQL: &str = "INSERT INTO users (
     id, nickname, ap_id, local, domain, actor_type, name, bio, raw_bio,
     avatar_url, banner_url, profile_url, locked, discoverable, fields,
     inbox, shared_inbox, outbox, followers_url, following_url,
     featured_url, public_key, also_known_as, public_key_id, bot,
     birthday, show_birthday, last_fetched_at
 ) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
     $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26::date, $26::date IS NOT NULL,
     now()
 ) ON CONFLICT (ap_id) DO UPDATE SET
     birthday = EXCLUDED.birthday,
     show_birthday = EXCLUDED.show_birthday,
     nickname = EXCLUDED.nickname,
     domain = EXCLUDED.domain,
     actor_type = EXCLUDED.actor_type,
     bot = EXCLUDED.bot,
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
     also_known_as = EXCLUDED.also_known_as,
     public_key_id = EXCLUDED.public_key_id,
     last_fetched_at = now(),
     updated_at = now()
 RETURNING *";

/// The ownership probe behind [`upsert_remote_user`]: the actor id (if any)
/// of a row whose citext nickname is the one the document claims but which
/// is keyed by a *different* `ap_id`, and whether that row is a stale mirror
/// of the same origin (`local = false` and the same `domain` as `$3`).
/// Matching the same row's own id is the ordinary update path, including
/// when the remote renamed `preferredUsername`. The row is locked so two
/// concurrent upserts cannot both rename and claim the handle.
const NICKNAME_HOLDER_SQL: &str = "SELECT ap_id, COALESCE(NOT local AND domain = $3, false)
     FROM users WHERE nickname = $1::citext AND ap_id <> $2 FOR UPDATE";

/// Frees a nickname held by a stale mirror: prefixes it with the row's id,
/// `<old id>.<nickname>`, the way Pleroma's `maybe_handle_clashing_nickname`
/// does, so the old row keeps existing (its statuses and relationships stay
/// attached) without holding the handle.
const RENAME_HOLDER_SQL: &str = "UPDATE users
     SET nickname = (id::text || '.' || nickname::text)::citext, updated_at = now()
     WHERE ap_id = $1";

/// Frees a key id another row holds before this actor claims it.
///
/// The unique index on `public_key_id` would otherwise fail the whole upsert
/// when a host re-homes a key id onto a different actor. The latest document
/// wins; the loser's next fetch stores its own id again.
const RELEASE_KEY_ID_SQL: &str =
    "UPDATE users SET public_key_id = NULL WHERE public_key_id = $1 AND ap_id <> $2";

/// [`RELEASE_KEY_ID_SQL`] for the `activitypub_actors` row that holds the key id.
///
/// That table has the same unique index, so the key id is freed there too
/// (part of the temporary dual write, see
/// [`activitypub_actor`](crate::models::activitypub_actor)). The keeper is
/// named by `ap_id` because its id is not known before the first insert.
const RELEASE_ACTOR_KEY_ID_SQL: &str = "UPDATE activitypub_actors SET public_key_id = NULL
     WHERE public_key_id = $1
       AND user_id NOT IN (SELECT id FROM users WHERE ap_id = $2)";

/// Stores `new` as this instance's mirror of the actor, exactly once per
/// actor id (decision 7).
///
/// The first fetch inserts a row with a fresh `UUIDv7` (decision 4); later
/// fetches apply to the row with the same `ap_id`, updating the profile
/// fields, keys and inbox endpoints in place and stamping
/// `last_fetched_at = now()`.
///
/// Before writing, the probe `NICKNAME_HOLDER_SQL` checks who owns the
/// nickname. A remote row of the same `domain` under a different `ap_id` is
/// a stale mirror (the origin reinstalled, moved its actor paths or
/// recreated the account): within one transaction it is renamed to
/// `<its id>.<nickname>` and the actor is stored under the handle, as
/// Pleroma does. Any other holder (a local row, or a different domain)
/// keeps the handle and the upsert fails with
/// [`ActorError::NicknameConflict`]. When two actor ids of one origin claim
/// one handle, the document fetched last holds it.
///
/// Once the row is committed, each of [`emojis`](NewRemoteUser::emojis) is
/// written with [`custom_emoji::upsert_remote`] under the actor's domain,
/// so every fetch, refresh and `Update` keeps the profile's emoji current.
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
/// [`ActorError::NicknameConflict`] when another `ap_id` holds this
/// nickname and is not a remote row of the same domain;
/// [`ActorError::Failed`] for any database failure, including the instance
/// record.
pub async fn upsert_remote_user(pool: &PgPool, new: NewRemoteUser) -> Result<User, ActorError> {
    let mut tx = pool.begin().await?;
    let holder: Option<(String, bool)> = sqlx::query_as(NICKNAME_HOLDER_SQL)
        .bind(new.nickname.as_str())
        .bind(new.ap_id.as_str())
        .bind(new.domain.as_str())
        .fetch_optional(&mut *tx)
        .await?;
    if let Some((holder_ap_id, stale_mirror)) = holder {
        if !stale_mirror {
            return Err(ActorError::NicknameConflict);
        }
        let _renamed = sqlx::query(RENAME_HOLDER_SQL)
            .bind(holder_ap_id.as_str())
            .execute(&mut *tx)
            .await?;
    }
    if let Some(key_id) = new.public_key_id.as_deref() {
        let _released = sqlx::query(RELEASE_KEY_ID_SQL)
            .bind(key_id)
            .bind(new.ap_id.as_str())
            .execute(&mut *tx)
            .await?;
        let _released_actor = sqlx::query(RELEASE_ACTOR_KEY_ID_SQL)
            .bind(key_id)
            .bind(new.ap_id.as_str())
            .execute(&mut *tx)
            .await?;
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
        .bind(&new.also_known_as)
        .bind(new.public_key_id.as_deref())
        .bind(new.bot)
        .bind(new.birthday)
        .fetch_one(&mut *tx)
        .await?;
    activitypub_actor::upsert_from_user(&mut *tx, &user, new.public_key_id.as_deref())
        .await
        .map(drop)?;
    tx.commit().await?;
    for emoji in &new.emojis {
        custom_emoji::upsert_remote(pool, &emoji.shortcode, emoji.domain.as_deref(), &emoji.url)
            .await?;
    }
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
        models::{
            activitypub_actor::ActorStatus,
            user::{
                CountDelta, Flag, NewUser, get_by_ap_id, get_by_id, get_by_nickname,
                get_by_public_key_id, insert, update_counts,
            },
        },
        test_fixtures::assert_actor_in_step,
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
    async fn upsert_stores_the_public_key_id_and_rehomes_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap();
        let found = get_by_public_key_id(&pool, "https://mastodon.example/users/alice#main-key")
            .await
            .unwrap()
            .expect("the mirror is found by its key id");
        assert_eq!(found.id, alice.id, "the key id names the stored actor");

        // A second actor on the same host claiming the same key id takes
        // it over instead of failing the unique index.
        let mut other = fixture(MASTODON);
        other["id"] = json!("https://mastodon.example/users/carol");
        other["preferredUsername"] = json!("carol");
        other["inbox"] = json!("https://mastodon.example/users/carol/inbox");
        other["publicKey"]["owner"] = json!("https://mastodon.example/users/carol");
        let carol = upsert_remote_user(&pool, parse_actor(&other).unwrap())
            .await
            .unwrap();
        let moved = get_by_public_key_id(&pool, "https://mastodon.example/users/alice#main-key")
            .await
            .unwrap()
            .expect("the key id still resolves");
        assert_eq!(moved.id, carol.id, "the latest document holds the key id");

        let alice_actor = assert_actor_in_step(&pool, alice.id).await;
        assert!(
            alice_actor.public_key_id.is_none(),
            "the actor row lost the key id too"
        );
        let carol_actor = assert_actor_in_step(&pool, carol.id).await;
        assert_eq!(
            carol_actor.public_key_id.as_deref(),
            Some("https://mastodon.example/users/alice#main-key"),
            "and the new holder's actor row has it"
        );
    }

    /// Both tables hold the same moved columns after the first fetch and
    /// after a re-fetch that changes them, and the actor row is `active`.
    #[tokio::test]
    async fn an_upsert_writes_both_tables_alike() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut document = fixture(MASTODON);
        let first = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();

        let stored = assert_actor_in_step(&pool, first.id).await;
        assert_eq!(
            stored.status,
            ActorStatus::Active,
            "a fetched actor is active"
        );
        assert_eq!(
            stored.inbox.as_deref(),
            Some("https://mastodon.example/users/alice/inbox"),
            "the inbox landed"
        );
        assert_eq!(
            stored.public_key_id.as_deref(),
            Some("https://mastodon.example/users/alice#main-key"),
            "the key id landed"
        );
        assert!(stored.last_fetched_at.is_some(), "the fetch is stamped");

        document["inbox"] = json!("https://mastodon.example/users/alice/new-inbox");
        document["publicKey"]["publicKeyPem"] = json!("-----BEGIN PUBLIC KEY-----\nNEW\n");
        let second = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();

        assert_eq!(second.id, first.id, "the same account is re-fetched");
        let refreshed = assert_actor_in_step(&pool, first.id).await;
        assert_eq!(
            refreshed.inbox.as_deref(),
            Some("https://mastodon.example/users/alice/new-inbox"),
            "the new inbox replaced the old"
        );
        assert_ne!(
            refreshed.public_key, stored.public_key,
            "the new key replaced the old"
        );
        assert_eq!(
            refreshed.public_key, second.public_key,
            "and is the one the user row stores"
        );
        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM activitypub_actors")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 1, "one actor row, updated in place");
    }

    /// A conflict (a local holder of the handle) rolls the whole write back,
    /// the actor table included.
    #[tokio::test]
    async fn a_refused_upsert_leaves_no_actor_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let _squatter = insert(
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

        assert!(matches!(error, ActorError::NicknameConflict), "{error:?}");
        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM activitypub_actors")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 1, "only the squatter's own row exists");
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
    async fn upsert_reports_a_nickname_conflict_with_a_local_holder() {
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

    /// A remote actor recreated under a new id (same origin, same handle)
    /// is mirrored: the stale row gives up the handle, renamed to
    /// `<its id>.<handle>`, and keeps existing.
    #[tokio::test]
    async fn upsert_renames_a_stale_mirror_of_the_same_origin() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let old = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap();
        let mut moved = parse_actor(&fixture(MASTODON)).unwrap();
        moved.ap_id = "https://mastodon.example/u/alice".to_owned();

        let new = upsert_remote_user(&pool, moved).await.unwrap();

        assert_ne!(new.id, old.id, "the recreated actor is a new row");
        assert_eq!(
            new.nickname, "alice@mastodon.example",
            "the new actor holds the handle"
        );
        let holder = get_by_nickname(&pool, "alice@mastodon.example")
            .await
            .unwrap()
            .expect("the handle resolves");
        assert_eq!(holder.ap_id, new.ap_id, "to the new actor id");
        let stale = get_by_id(&pool, old.id)
            .await
            .unwrap()
            .expect("the stale row stands");
        assert_eq!(
            stale.nickname,
            format!("{}.alice@mastodon.example", old.id),
            "the old row is renamed with its id"
        );
        assert_eq!(
            stale.ap_id, "https://mastodon.example/users/alice",
            "and keeps its actor id"
        );
        assert_eq!(
            stale.domain.as_deref(),
            Some("mastodon.example"),
            "and its domain"
        );
    }

    /// Re-fetching the old actor's document after the recreation is the
    /// flip-flop of two live ids on one handle: the document fetched last
    /// holds it, the other row is renamed, and nothing fails.
    #[tokio::test]
    async fn upsert_of_the_old_id_after_a_recreation_swaps_the_handle_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let old = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap();
        let mut moved = parse_actor(&fixture(MASTODON)).unwrap();
        moved.ap_id = "https://mastodon.example/u/alice".to_owned();
        let new = upsert_remote_user(&pool, moved).await.unwrap();

        let again = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap();

        assert_eq!(again.id, old.id, "the old row is updated in place");
        assert_eq!(
            again.nickname, "alice@mastodon.example",
            "the last fetched document holds the handle"
        );
        let displaced = get_by_id(&pool, new.id)
            .await
            .unwrap()
            .expect("the other row stands");
        assert_eq!(
            displaced.nickname,
            format!("{}.alice@mastodon.example", new.id),
            "and the other row is renamed"
        );
        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 2, "no row was lost or duplicated");
    }

    /// A remote row of a *different* domain holding the handle is not a
    /// stale mirror of this origin: it keeps the handle and the upsert is
    /// refused, with nothing renamed or stored.
    #[tokio::test]
    async fn upsert_refuses_a_handle_held_by_a_remote_of_another_domain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let holder = insert(
            &pool,
            &NewUser {
                nickname: "alice@mastodon.example".to_owned(),
                ap_id: "https://other.example/users/alice".to_owned(),
                local: Flag::new(false),
                domain: Some("other.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .unwrap();

        let error = upsert_remote_user(&pool, parse_actor(&fixture(MASTODON)).unwrap())
            .await
            .unwrap_err();

        assert!(
            matches!(error, ActorError::NicknameConflict),
            "a foreign holder is a conflict: {error:?}"
        );
        let still = get_by_id(&pool, holder.id)
            .await
            .unwrap()
            .expect("the holder stands");
        assert_eq!(
            still.nickname, "alice@mastodon.example",
            "the holder keeps the handle"
        );
        assert!(
            get_by_ap_id(&pool, "https://mastodon.example/users/alice")
                .await
                .unwrap()
                .is_none(),
            "the mirror was not stored"
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

    /// The `bot` column follows the document's `type` on every fetch.
    ///
    /// `Service` and `Application` store `true`, `Person` stores `false`,
    /// and a later `Person` document clears the flag again.
    #[tokio::test]
    async fn an_upsert_derives_bot_from_the_actor_type() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut document = fixture(MASTODON);
        let person = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert!(!person.bot.get(), "a Person is not a bot");

        for kind in ["Service", "Application"] {
            document["type"] = json!(kind);
            let stored = upsert_remote_user(&pool, parse_actor(&document).unwrap())
                .await
                .unwrap();
            assert_eq!(stored.id, person.id, "the same row is updated");
            assert!(stored.bot.get(), "a {kind} is a bot");
        }

        document["type"] = json!("Person");
        let back = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert!(!back.bot.get(), "back to Person clears the flag");
        document["type"] = json!("Group");
        let group = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert!(!group.bot.get(), "a Group is not a bot");
    }

    /// The birthday follows `vcard:bday` on every fetch and clears when absent.
    #[tokio::test]
    async fn an_upsert_stores_and_clears_the_vcard_birthday() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut document = fixture(MASTODON);
        let plain = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert!(plain.birthday.is_none(), "no birthday without the key");
        assert!(!plain.show_birthday.get(), "and nothing to show");

        document["vcard:bday"] = json!("1990-05-15");
        let shown = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert_eq!(shown.id, plain.id, "the same row is updated");
        assert_eq!(
            shown.birthday,
            chrono::NaiveDate::from_ymd_opt(1990, 5, 15),
            "the date is stored"
        );
        assert!(shown.show_birthday.get(), "a federated birthday is shown");

        document["vcard:bday"] = Value::Null;
        let cleared = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        assert!(cleared.birthday.is_none(), "a null bday clears the date");
        assert!(!cleared.show_birthday.get(), "and the switch");
    }

    /// A fetched actor's `Emoji` tags land in `custom_emojis`.
    ///
    /// They are filed under the actor's domain, and a refresh rewrites a
    /// moved image instead of adding a row.
    #[tokio::test]
    async fn an_upsert_stores_the_actors_emoji_under_its_domain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut document = fixture(MASTODON);
        document["tag"] = json!([
            {
                "type": "Emoji",
                "name": ":mastofox:",
                "icon": {"type": "Image", "url": "https://mastodon.example/emoji/mastofox.png"}
            },
            {
                "type": "Emoji",
                "name": ":blob-cat:",
                "icon": {"type": "Image", "url": "https://mastodon.example/emoji/blob-cat.png"}
            },
        ]);
        let _first = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();

        let codes = ["mastofox".to_owned(), "blob-cat".to_owned()];
        let rows = custom_emoji::get_by_shortcodes(&pool, &codes, Some("mastodon.example"))
            .await
            .unwrap();
        let stored: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| (row.shortcode.as_str(), row.image_url.as_str()))
            .collect();
        assert_eq!(
            stored,
            [
                ("blob-cat", "https://mastodon.example/emoji/blob-cat.png"),
                ("mastofox", "https://mastodon.example/emoji/mastofox.png"),
            ],
            "both emojis exist under the actor's domain"
        );
        let local = custom_emoji::get_by_shortcodes(&pool, &codes, None)
            .await
            .unwrap();
        assert!(local.is_empty(), "and none leaked into the local set");

        document["tag"][0]["icon"]["url"] = json!("https://mastodon.example/emoji/new.png");
        let _second = upsert_remote_user(&pool, parse_actor(&document).unwrap())
            .await
            .unwrap();
        let refreshed = custom_emoji::get_by_shortcodes(&pool, &codes, Some("mastodon.example"))
            .await
            .unwrap();
        assert_eq!(refreshed.len(), 2, "a refresh adds no row");
        let fox = refreshed
            .iter()
            .find(|row| row.shortcode == "mastofox")
            .expect("the fox is still stored");
        assert_eq!(
            fox.image_url, "https://mastodon.example/emoji/new.png",
            "the refresh rewrote the moved image"
        );
    }
}
