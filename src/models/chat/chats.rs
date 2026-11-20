//! The chat rows: opening one, reading one by owner or by id alone, pinning
//! it, and the bump-or-create write [`add_message`](super::add_message) runs
//! on each side of a conversation.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{error::AppResult, models::user::User, util::id::new_id};

use super::Chat;

/// [`get_or_create`]: one chat row per direction, written once.
///
/// `DO UPDATE SET recipient_id = EXCLUDED.recipient_id` changes nothing and is
/// there for what Pleroma's `Chat.get_or_create` says of the same trick —
/// "need to set something, otherwise we get nothing back at all": a
/// `DO NOTHING` conflict answers no row. Unlike [`BUMP_CHAT_SQL`] this leaves
/// [`updated_at`](Chat::updated_at) alone, so opening a chat does not move it in
/// its owner's list.
const GET_OR_CREATE_SQL: &str = "INSERT INTO chats (id, user_id, recipient_id)
     VALUES ($1, $2, $3)
ON CONFLICT (user_id, recipient_id) DO UPDATE SET recipient_id = EXCLUDED.recipient_id
 RETURNING *";

/// What [`add_message`] does to each side's chat row: create it when this is the
/// pair's first message, or bump it to the front of its owner's list (Pleroma's
/// `Chat.bump_or_create`).
///
/// [`add_message`]: super::add_message
const BUMP_CHAT_SQL: &str = "INSERT INTO chats (id, user_id, recipient_id)
     VALUES ($1, $2, $3)
ON CONFLICT (user_id, recipient_id) DO UPDATE SET updated_at = now()
 RETURNING *";

/// [`get_owned`]: the chat `id` of user `user_id`. The two participants' chats of
/// one conversation have different ids, so a chat somebody else owns simply is
/// not there.
const OWNED_CHAT_SQL: &str = "SELECT * FROM chats WHERE id = $1 AND user_id = $2";

/// [`get_by_id`]: the chat row `id` names, whoever owns it — the admin read,
/// which names a chat by its id alone and so runs [`OWNED_CHAT_SQL`] without
/// its second condition.
const CHAT_BY_ID_SQL: &str = "SELECT * FROM chats WHERE id = $1";

/// [`set_pinned`]: pinning is the chat owner's own ordering choice, and like
/// every update here it refreshes `updated_at` — Pleroma's `Chat.pin` and
/// `Chat.unpin` go through `Repo.update`, which stamps its `updated_at` too.
const SET_PINNED_SQL: &str = "UPDATE chats
      SET pinned = $3, updated_at = now()
    WHERE id = $1 AND user_id = $2
  RETURNING *";

/// The chat row of `user` talking to `recipient`, created when the pair has none
/// yet.
///
/// Only that one row is created: the chat row of `recipient` is theirs, written
/// when they open the conversation or receive a message ([`add_message`]). The
/// direction the arguments name is the direction of the row, so
/// `get_or_create(pool, alice, bob)` and `get_or_create(pool, bob, alice)` are
/// two different chats with two different ids.
///
/// Creating is idempotent and never bumps [`updated_at`](Chat::updated_at): a
/// chat that exists stays where its owner's list has it.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); an unknown `user` or
/// `recipient` is the foreign-key violation on `users(id)`.
///
/// [`add_message`]: super::add_message
pub async fn get_or_create(pool: &PgPool, user: &User, recipient: &User) -> AppResult<Chat> {
    let chat = sqlx::query_as::<_, Chat>(GET_OR_CREATE_SQL)
        .bind(new_id())
        .bind(user.id)
        .bind(recipient.id)
        .fetch_one(pool)
        .await?;
    Ok(chat)
}

/// The chat `id`, provided it belongs to `user_id`. `None` when no chat has that
/// id, and `None` just as well when it belongs to somebody else: one
/// participant's chat id means nothing to another.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_owned(pool: &PgPool, user_id: Uuid, id: Uuid) -> AppResult<Option<Chat>> {
    let chat = sqlx::query_as::<_, Chat>(OWNED_CHAT_SQL)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(chat)
}

/// The chat row with primary key `id`, whatever owner it has.
///
/// This is the admin read behind the `/api/v1/pleroma/admin/chats` routes,
/// which name a chat by its id alone and skip [`get_owned`]'s ownership
/// condition. `None` when no chat has that id.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<Chat>> {
    let chat = sqlx::query_as::<_, Chat>(CHAT_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(chat)
}

/// Pins (`pinned = true`) or unpins `user_id`'s chat `chat_id` and returns the
/// row, `None` when no chat of theirs has that id.
///
/// The flag is per participant: pinning a conversation never touches the other
/// side's row.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_pinned(
    pool: &PgPool,
    user_id: Uuid,
    chat_id: Uuid,
    pinned: bool,
) -> AppResult<Option<Chat>> {
    let chat = sqlx::query_as::<_, Chat>(SET_PINNED_SQL)
        .bind(chat_id)
        .bind(user_id)
        .bind(pinned)
        .fetch_optional(pool)
        .await?;
    Ok(chat)
}

/// The chat of local `recipient` talking to `sender`, or `None` when `recipient`
/// is a remote account, whose side of a conversation this instance does not
/// keep.
pub(super) async fn local_recipient_chat(
    conn: &mut PgConnection,
    sender: &User,
    recipient: &User,
) -> AppResult<Option<Chat>> {
    if !recipient.local.get() {
        return Ok(None);
    }
    let chat = bump_or_create(conn, recipient.id, sender.id).await?;
    Ok(Some(chat))
}

/// The chat of `user_id` talking to `recipient_id`: created if missing, bumped to
/// the front of its owner's list if it was there.
pub(super) async fn bump_or_create(
    conn: &mut PgConnection,
    user_id: Uuid,
    recipient_id: Uuid,
) -> AppResult<Chat> {
    let chat = sqlx::query_as::<_, Chat>(BUMP_CHAT_SQL)
        .bind(new_id())
        .bind(user_id)
        .bind(recipient_id)
        .fetch_one(conn)
        .await?;
    Ok(chat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        error::AppError,
        models::chat::{
            ChatFilter, page_for_user,
            test_support::{
                ALL, chat_ids, chats, count, local, page, send, sqlstate_of, stored_chat,
                stored_message,
            },
        },
    };

    #[tokio::test]
    async fn get_or_create_is_idempotent_and_per_direction() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        let alice_chat = get_or_create(&pool, &alice, &bob).await.unwrap();
        let again = get_or_create(&pool, &alice, &bob).await.unwrap();

        assert!(alice_chat.id.get_version_num() == 7, "the id is a UUIDv7");
        assert_eq!(alice_chat.id, again.id, "the second call writes nothing");
        assert_eq!(alice_chat.user_id, alice.id, "the row belongs to alice");
        assert_eq!(alice_chat.recipient_id, bob.id, "and is about bob");
        assert!(!alice_chat.pinned, "a fresh chat is unpinned");
        assert_eq!(
            alice_chat.inserted_at, alice_chat.updated_at,
            "opening a chat does not bump it"
        );
        assert_eq!(
            get_owned(&pool, alice.id, alice_chat.id).await.unwrap(),
            Some(alice_chat.clone()),
            "get_owned finds a chat for its owner"
        );

        let bob_chat = get_or_create(&pool, &bob, &alice).await.unwrap();
        assert_ne!(
            bob_chat.id, alice_chat.id,
            "the other direction is a second chat row"
        );
        assert_eq!(bob_chat.user_id, bob.id, "belonging to bob");
        assert_eq!(bob_chat.recipient_id, alice.id, "about alice");
        assert_eq!(
            chat_ids(&chats(&pool, alice.id).await),
            vec![alice_chat.id],
            "alice lists one chat"
        );
        assert_eq!(
            chat_ids(&chats(&pool, bob.id).await),
            vec![bob_chat.id],
            "bob lists his own, not alice's"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chats").await,
            2,
            "two rows for one pair"
        );
    }

    #[tokio::test]
    async fn a_chat_is_only_its_owner_s() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let alice_chat = get_or_create(&pool, &alice, &bob).await.unwrap();

        assert!(
            get_owned(&pool, bob.id, alice_chat.id)
                .await
                .unwrap()
                .is_none(),
            "bob cannot read alice's chat row by id"
        );
        assert!(
            get_owned(&pool, alice.id, new_id())
                .await
                .unwrap()
                .is_none(),
            "an unknown id is no chat"
        );
        assert!(
            set_pinned(&pool, bob.id, alice_chat.id, true)
                .await
                .unwrap()
                .is_none(),
            "and only its owner pins it"
        );
        assert!(
            !stored_chat(&pool, alice_chat.id).await.pinned,
            "so alice's chat stayed unpinned"
        );
        assert!(
            set_pinned(&pool, alice.id, new_id(), true)
                .await
                .unwrap()
                .is_none(),
            "an unknown chat is nothing to pin"
        );
    }

    #[tokio::test]
    async fn get_by_id_ignores_ownership() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let alice_chat = get_or_create(&pool, &alice, &bob).await.unwrap();

        assert_eq!(
            get_by_id(&pool, alice_chat.id).await.unwrap(),
            Some(alice_chat.clone()),
            "get_by_id finds a chat by its id alone"
        );
        assert_eq!(
            get_by_id(&pool, alice_chat.id)
                .await
                .unwrap()
                .map(|c| c.user_id),
            Some(alice.id),
            "the row is alice's, and the read does not care who asks"
        );
        assert!(
            get_by_id(&pool, new_id()).await.unwrap().is_none(),
            "an unknown id is no chat"
        );
    }

    #[tokio::test]
    async fn pinning_is_per_participant_and_orders_the_list() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let with_bob = get_or_create(&pool, &alice, &bob).await.unwrap();
        let with_carol = get_or_create(&pool, &alice, &carol).await.unwrap();
        let bobs_side = get_or_create(&pool, &bob, &alice).await.unwrap();

        let pinned = set_pinned(&pool, alice.id, with_carol.id, true)
            .await
            .unwrap()
            .expect("alice owns that chat");

        assert!(pinned.pinned, "the flag is set");
        assert!(
            pinned.updated_at > with_carol.updated_at,
            "pinning stamps the row"
        );
        assert_eq!(
            chat_ids(&chats(&pool, alice.id).await),
            vec![pinned.id, with_bob.id],
            "the pinned chat leads the list"
        );
        assert_eq!(
            chat_ids(
                &page_for_user(
                    &pool,
                    alice.id,
                    page(ALL),
                    ChatFilter {
                        pinned: Some(true),
                        with_muted: false,
                    },
                )
                .await
                .unwrap(),
            ),
            vec![pinned.id],
            "the pinned filter names it"
        );
        assert_eq!(
            chat_ids(
                &page_for_user(
                    &pool,
                    alice.id,
                    page(ALL),
                    ChatFilter {
                        pinned: Some(false),
                        with_muted: true,
                    },
                )
                .await
                .unwrap(),
            ),
            vec![with_bob.id],
            "the unpinned filter gets the rest; with_muted excludes nothing yet"
        );
        assert!(
            !stored_chat(&pool, bobs_side.id).await.pinned,
            "bob's own row is untouched by alice's pin"
        );

        let unpinned = set_pinned(&pool, alice.id, pinned.id, false)
            .await
            .unwrap()
            .expect("the chat is still there");
        assert!(!unpinned.pinned, "unpinning clears the flag");
        assert_eq!(
            chat_ids(&chats(&pool, alice.id).await),
            vec![unpinned.id, with_bob.id],
            "the bump it stamps keeps it first for now"
        );
    }

    #[tokio::test]
    async fn chats_key_their_owners_and_their_pairs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        let unknown =
            sqlx::query("INSERT INTO chats (id, user_id, recipient_id) VALUES ($1, $2, $3)")
                .bind(new_id())
                .bind(alice.id)
                .bind(new_id())
                .execute(&pool)
                .await
                .expect_err("a chat needs two users this instance knows");
        assert_eq!(
            sqlstate_of(AppError::from(unknown)),
            "23503",
            "the refusal is the foreign key on users"
        );

        let first_chat: Chat = get_or_create(&pool, &alice, &bob).await.unwrap();
        assert_eq!(
            first_chat.recipient_id, bob.id,
            "the model's row is the one the key rejects"
        );
        let second =
            sqlx::query("INSERT INTO chats (id, user_id, recipient_id) VALUES ($1, $2, $3)")
                .bind(new_id())
                .bind(alice.id)
                .bind(bob.id)
                .execute(&pool)
                .await
                .expect_err("one participant has one chat per account");
        assert_eq!(
            sqlstate_of(AppError::from(second)),
            "23505",
            "the refusal is the unique (user_id, recipient_id)"
        );
    }

    #[tokio::test]
    async fn deleting_a_sender_takes_their_chats_and_messages() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let (message, _, bobs_ref) = send(&pool, &alice, &bob, "bye").await;
        let bobs_chat = bobs_ref.expect("bob's side of the conversation");

        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .map(drop)
            .expect("alice's account is deleted");

        assert_eq!(
            count(&pool, "SELECT count(*) FROM chats").await,
            0,
            "both sides' chat rows went with the accounts they name"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM chat_message_references").await,
            0,
            "and the references with them"
        );
        assert!(
            stored_message(&pool, message.id).await.is_none(),
            "alice's message went with its sender"
        );
        assert!(
            get_owned(&pool, bob.id, bobs_chat.chat_id)
                .await
                .unwrap()
                .is_none(),
            "bob keeps no view of the conversation"
        );
    }
}
