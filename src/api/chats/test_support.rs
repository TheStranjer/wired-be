//! Test fixtures shared by the chat unit tests: the local and remote
//! users the gating and hook tests pair up, the test [`AppState`] the
//! hooks publish through, and the message write both create paths stand
//! on.

use crate::{
    config::{Config, TEST_BASE_URL},
    models::{
        chat,
        chat::{ChatMessageReference, NewChatMessage},
        user::{self, User},
    },
    state::AppState,
    test_fixtures::{local_new, remote_new},
};

// A local user named `nickname` that accepts chats, and its remote
// counterpart on `remote.example`: the pair a delivered or addressed
// message runs between.
pub(super) use crate::test_fixtures::{local_user as chatter, remote_user as remote_chatter};

/// Inserts a local [`User`] named `nickname` whose chat flag is down.
pub(super) async fn quiet(pool: &sqlx::PgPool, nickname: &str) -> User {
    user::insert(
        pool,
        &user::NewUser {
            accepts_chat_messages: user::Flag::new(false),
            ..local_new(nickname)
        },
    )
    .await
    .expect("fixture user inserts")
}

/// Inserts a remote mirror named `nickname` whose chat flag is down: the
/// shape a fetched `Person` that said `acceptsChatMessages: false` has.
pub(super) async fn quiet_remote(pool: &sqlx::PgPool, nickname: &str) -> User {
    user::insert(
        pool,
        &user::NewUser {
            accepts_chat_messages: user::Flag::new(false),
            ..remote_new(nickname)
        },
    )
    .await
    .expect("fixture remote user inserts")
}

/// Test state over `pool` with the fixed test config: the hooks under
/// test publish through the hub it owns, which the tests subscribe to.
pub(super) fn app_state(pool: &sqlx::PgPool) -> AppState {
    AppState::for_tests(pool.clone(), Config::for_tests())
}

/// Writes `sender`'s message to `recipient` the way both create paths
/// do — [`chat::add_message`] over one connection — and answers the two
/// sides' references, the recipient's `None` when they are remote and
/// so own no row here.
pub(super) async fn message_pair(
    pool: &sqlx::PgPool,
    sender: &User,
    recipient: &User,
) -> (ChatMessageReference, Option<ChatMessageReference>) {
    let mut conn = pool.acquire().await.expect("a fixture connection");
    let (_, sender_reference, recipient_reference) = chat::add_message(
        &mut conn,
        TEST_BASE_URL,
        sender,
        recipient,
        &NewChatMessage::text("hello there"),
    )
    .await
    .expect("the message fixture writes");
    (sender_reference, recipient_reference)
}
