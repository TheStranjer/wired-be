//! Row fixtures: users, statuses, notifications, reactions and attachments.

use std::ops::Deref;

use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    models::{
        activitypub_actor::{self, ActivityPubActor, ActivityPubActorWrite},
        media_attachment::{self, MediaAttachment, NewMediaAttachment},
        notification::{self, NewNotification, Notification},
        status::{self, NewStatus, Status, Visibility},
        user::{
            NewUser, User, followers, following, get_by_id, inbox, insert, local_ap_id, outbox,
        },
    },
    util::{
        keys::{KeyPair, generate_keypair_async},
        password::hash_password,
    },
};

use super::test_config;

/// Inserts a local [`User`] named `nickname` straight through
/// [`wired_be::models::user`], with no password, and returns its row.
///
/// The actor id is [`local_ap_id`] on the [`test_config`] instance, so
/// fixtures and federation-shaped code agree on the URL.
///
/// # Panics
///
/// If the insert fails, e.g. because the nickname is already taken in the
/// test database.
pub async fn create_local_user(pool: &PgPool, nickname: &str) -> User {
    let config = test_config();
    let ap_id = local_ap_id(&config.base_url, nickname);
    insert(pool, &NewUser::local(nickname, &ap_id))
        .await
        .expect("local user fixture inserts")
}

/// User `id` as it stands now (counters and all).
///
/// # Panics
///
/// If the read fails or no user has that id.
pub async fn reload_user(pool: &PgPool, id: Uuid) -> User {
    get_by_id(pool, id)
        .await
        .expect("the user reads")
        .expect("the user exists")
}

/// Inserts a local [`User`] named `nickname` that can log in with
/// `password`.
///
/// The row carries an Argon2id hash made with
/// [`hash_password`](wired_be::util::password::hash_password) and the
/// derived email `<nickname>@example.org`, so both lookup paths of
/// [`authenticate`] (nickname and email) find it, and only `password`
/// does.
///
/// This deliberately bypasses
/// [`create_local_user`](wired_be::models::user::register::create_local_user)
/// and its validation: fixtures need a login-capable row cheaply, so no
/// RSA key pair is generated and no registration rules are re-checked.
/// Use the register path itself when a test is about registration.
///
/// [`authenticate`]: wired_be::models::user::register::authenticate
///
/// # Panics
///
/// If hashing or the insert fails, e.g. because the nickname or derived
/// email is already taken in the test database.
pub async fn create_user_with_password(pool: &PgPool, nickname: &str, password: &str) -> User {
    let config = test_config();
    let new = NewUser {
        email: Some(format!("{nickname}@example.org")),
        password_hash: Some(
            hash_password(password).expect("password hashing succeeds for fixtures"),
        ),
        ..NewUser::local(nickname, &local_ap_id(&config.base_url, nickname))
    };
    insert(pool, &new)
        .await
        .expect("local user fixture with password inserts")
}

/// Inserts a local [`User`] named `nickname` whose columns `customize`
/// adjusts on top of [`NewUser::local`], and returns its row.
///
/// The profile and federation tests need accounts that differ from the
/// bare [`create_local_user`] fixture — locked, a bot, with an avatar —
/// without restating the actor id every time.
///
/// # Panics
///
/// If the insert fails, e.g. because the nickname is already taken in the
/// test database.
pub async fn create_local_user_with<F>(pool: &PgPool, nickname: &str, customize: F) -> User
where
    F: FnOnce(NewUser) -> NewUser,
{
    let config = test_config();
    let ap_id = local_ap_id(&config.base_url, nickname);
    let new = customize(NewUser::local(nickname, &ap_id));
    insert(pool, &new)
        .await
        .expect("local user fixture inserts")
}

/// Inserts a mirror of the remote actor `nickname` on `host`: a non-local
/// [`User`], the shape a fetch of `https://{host}/users/{nickname}` stores.
///
/// The nickname carries `@host` (decision 7) and the actor id, domain and
/// endpoints are the ones the fetched document would have named: the
/// endpoints and key are on the account's `activitypub_actors` row.
///
/// # Panics
///
/// If either insert fails, e.g. because the handle is already taken in the
/// test database.
pub async fn create_remote_user(pool: &PgPool, nickname: &str, host: &str) -> User {
    let ap_id = format!("https://{host}/users/{nickname}");
    let new = NewUser {
        nickname: format!("{nickname}@{host}"),
        ap_id: ap_id.clone(),
        domain: Some(host.to_owned()),
        name: nickname.to_owned(),
        ..NewUser::default()
    };
    let user = insert(pool, &new)
        .await
        .expect("remote user fixture inserts");
    let _row = write_actor(
        pool,
        &ActivityPubActorWrite {
            inbox: Some(inbox(&ap_id)),
            shared_inbox: Some(format!("https://{host}/inbox")),
            outbox: Some(outbox(&ap_id)),
            followers_url: Some(followers(&ap_id)),
            following_url: Some(following(&ap_id)),
            public_key: Some(
                "-----BEGIN PUBLIC KEY-----\nREMOTE-DATA\n-----END PUBLIC KEY-----\n".to_owned(),
            ),
            ..ActivityPubActorWrite::bare(user.id)
        },
    )
    .await;
    user
}

/// Writes `write` as the `activitypub_actors` row of the account it names,
/// and returns the stored row.
///
/// # Panics
///
/// If the write fails, e.g. because the account has no row in the test
/// database.
pub async fn write_actor(pool: &PgPool, write: &ActivityPubActorWrite) -> ActivityPubActor {
    activitypub_actor::upsert(pool, write)
        .await
        .expect("the actor row writes")
}

/// Inserts `new` and gives the account the `activitypub_actors` row of a
/// federating local account, with the signing keys `PEM` and `PUB`.
///
/// The endpoints are the ones registration derives from the actor id (see
/// [`ActivityPubActorWrite::for_local_account`]).
///
/// # Panics
///
/// If either insert fails.
pub async fn insert_federating(pool: &PgPool, new: &NewUser) -> User {
    let user = insert(pool, new).await.expect("local user fixture inserts");
    let keys = KeyPair {
        private_pem: "PEM".to_owned(),
        public_pem: "PUB".to_owned(),
    };
    let _row = write_actor(
        pool,
        &ActivityPubActorWrite::for_local_account(&user, &test_config().base_url, keys),
    )
    .await;
    user
}

/// Inserts a local user named `nickname`, shaped by `customize`, and gives it
/// the `activitypub_actors` row [`insert_federating`] writes.
///
/// # Panics
///
/// If either insert fails.
pub async fn create_federating_user_with<F>(pool: &PgPool, nickname: &str, customize: F) -> User
where
    F: FnOnce(NewUser) -> NewUser,
{
    let ap_id = local_ap_id(&test_config().base_url, nickname);
    let new = customize(NewUser::local(nickname, &ap_id));
    insert_federating(pool, &new).await
}

/// A local account holding a real RSA key pair on its `activitypub_actors`
/// row, so it can sign an authorized fetch.
///
/// It derefs to its [`User`] row; the private key is kept beside it because
/// that row no longer carries one.
#[derive(Debug)]
pub struct Signer {
    /// The account's `users` row.
    pub user: User,
    /// The private key of the account's actor row.
    pub private_pem: String,
}

impl Deref for Signer {
    type Target = User;

    fn deref(&self) -> &User {
        &self.user
    }
}

/// Inserts a local [`Signer`] named `nickname`, with a freshly generated key
/// pair on its actor row.
///
/// # Panics
///
/// If key generation or either insert fails.
pub async fn create_signer(pool: &PgPool, nickname: &str) -> Signer {
    let keys = generate_keypair_async()
        .await
        .expect("the fixture key pair generates");
    let user = create_local_user(pool, nickname).await;
    let private_pem = keys.private_pem.clone();
    let _row = write_actor(
        pool,
        &ActivityPubActorWrite::for_local_account(&user, &test_config().base_url, keys),
    )
    .await;
    Signer { user, private_pem }
}

/// The `activitypub_actors` row of `user`.
///
/// # Panics
///
/// If the read fails or the account has no row.
pub async fn actor_of(pool: &PgPool, user: &User) -> ActivityPubActor {
    activitypub_actor::get(pool, user.id)
        .await
        .expect("the actor row reads")
        .expect("the account has an actor row")
}

/// Inserts a local [`Status`] posted by `user` — `text` as a plain-text
/// status of `visibility` — straight through
/// [`wired_be::models::status`], and returns its row.
///
/// The model derives everything a local post owns from its minted id on the
/// [`test_config`] instance: the `{base_url}/objects/{uuid}` uri, the
/// `{base_url}/notice/{uuid}` url, the conversation (its own id, since this
/// replies to nothing) and the author's `statuses_count`, so a fixture made
/// here looks exactly like a post the API would have written. A status that
/// replies, reblogs or quotes needs more of [`NewStatus`] than this passes:
/// build it and call the model directly.
///
/// # Panics
///
/// If the insert fails, e.g. because `user` has no row in the test database.
pub async fn create_status(
    pool: &PgPool,
    user: &User,
    text: &str,
    visibility: Visibility,
) -> Status {
    let new = NewStatus {
        visibility,
        ..NewStatus::local(user, text)
    };
    status::insert(pool, &test_config().base_url, &new)
        .await
        .expect("status fixture inserts")
}

/// Inserts a notification of `recipient` caused by `actor`.
///
/// Straight through [`wired_be::models::notification`]: of
/// `notification_type`, about `status_row` when the type names one, unseen
/// and undismissed, with the module-computed group key — exactly like a
/// notification a causing endpoint will create when those tickets land.
/// Fixtures that need another state (`seen`, `dismissed`, a marker-like
/// position) adjust the returned row's columns or the `markers` table
/// themselves. A reaction carries an emoji too; use [`create_reaction`].
///
/// # Panics
///
/// If the insert fails, e.g. because the recipient, actor or status has no
/// row in the test database.
pub async fn create_notification(
    pool: &PgPool,
    recipient: &User,
    actor: &User,
    notification_type: &str,
    status_row: Option<&Status>,
) -> Notification {
    notification::create(
        pool,
        &NewNotification {
            user_id: recipient.id,
            from_account_id: actor.id,
            notification_type,
            status_id: status_row.map(|found| found.id),
            emoji: None,
            report_id: None,
            chat_message_id: None,
        },
    )
    .await
    .expect("notification fixture inserts")
}

/// Inserts a `pleroma:emoji_reaction` notification of `recipient` by
/// `actor` about `status_row`, reacting with `emoji`.
///
/// [`create_notification`] for the one type that carries an extra part;
/// the reaction's group key includes the emoji, so two emojis on one
/// status are two groups.
///
/// # Panics
///
/// As [`create_notification`].
pub async fn create_reaction(
    pool: &PgPool,
    recipient: &User,
    actor: &User,
    status_row: &Status,
    emoji: &str,
) -> Notification {
    notification::create(
        pool,
        &NewNotification {
            user_id: recipient.id,
            from_account_id: actor.id,
            notification_type: "pleroma:emoji_reaction",
            status_id: Some(status_row.id),
            emoji: Some(emoji),
            report_id: None,
            chat_message_id: None,
        },
    )
    .await
    .expect("reaction fixture inserts")
}

/// Attaches one media file to `status` and returns its row: of
/// `content_type`, served at `url`, sitting at `position` within the
/// status, uploaded by the status's author.
///
/// The media-shaped tests need a status that actually carries
/// attachments without restating the whole [`NewMediaAttachment`];
/// everything they do not read (preview, description, dimensions) keeps
/// its default.
///
/// # Panics
///
/// If the insert fails, e.g. because `status` has no row in the test
/// database.
pub async fn create_status_attachment(
    pool: &PgPool,
    status: &Status,
    content_type: &str,
    url: &str,
    position: i32,
) -> MediaAttachment {
    media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(status.user_id),
            status_id: Some(status.id),
            position,
            content_type: content_type.to_owned(),
            url: url.to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("attachment fixture inserts")
}
