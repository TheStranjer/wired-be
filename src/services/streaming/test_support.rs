//! The fixtures every streaming child's unit tests share: the test state, the
//! seeded rows — local, remote and deactivated users, posts at every
//! visibility, follows and boost toggles, lists and memberships, attachments
//! and reblogs — and the stream-name, audience and hub-event helpers the
//! assertions read.
//!
//! Only compiled for tests; the fixtures that belong to one child alone — the
//! notification telling the `notification_created` tests seed — sit with that
//! child's own tests.

use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    config::{Config, TEST_BASE_URL},
    http_client::HttpClient,
    models::{
        follow::{self, FollowToggles},
        list::{self, NewList},
        media_attachment::{self, NewMediaAttachment},
        status::{self, NewStatus, Status, Visibility},
        user::{Flag, NewUser, User, insert, local_ap_id},
    },
    services::streaming::targets,
    state::AppState,
    streaming::hub::{Audience, Hub, StreamEvent},
    uploads::UploaderBackend,
    util::id::new_id,
};

/// Test state over `pool` with the fixed test config.
pub(super) fn app_state(pool: &PgPool) -> AppState {
    let config = Arc::new(Config::for_tests());
    AppState {
        db: pool.clone(),
        hub: Hub::new(pool),
        config: Arc::clone(&config),
        http_client: HttpClient::from(config.as_ref()),
        uploader: UploaderBackend::from(config.as_ref()),
        instance_stats_cache: Arc::default(),
        translation_languages_cache: Arc::default(),
        mrf_config_cache: Arc::default(),
        rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
        mailer: Arc::new(crate::mailer::MemoryTransport::new()),
    }
}

/// Inserts a local user named `nickname`.
pub(super) async fn local_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// Inserts a local user named `nickname` whose account is deactivated.
pub(super) async fn inactive_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser {
            is_active: Flag::new(false),
            ..NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
        },
    )
    .await
    .expect("the deactivated fixture inserts")
}

/// Inserts a mirror of `nickname` on `remote.example`.
pub(super) async fn remote_user(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser {
            nickname: format!("{nickname}@remote.example"),
            ap_id: format!("https://remote.example/users/{nickname}"),
            domain: Some("remote.example".to_owned()),
            ..NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts")
}

/// A public local post by `author` of `text`.
pub(super) async fn post(pool: &PgPool, author: &User, text: &str) -> Status {
    status::insert(pool, TEST_BASE_URL, &NewStatus::local(author, text))
        .await
        .expect("the status fixture inserts")
}

/// A local post by `author` of `text` at `visibility`, @-mentioning
/// every user in `mentions`.
pub(super) async fn post_vis(
    pool: &PgPool,
    author: &User,
    text: &str,
    visibility: Visibility,
    mentions: &[Uuid],
) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility,
            mentioned_user_ids: mentions.to_vec(),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("the status fixture inserts")
}

/// A public remote status by `author` of `text`, tagged with `tags`.
pub(super) async fn remote_post(pool: &PgPool, author: &User, text: &str, tags: &[&str]) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            user_id: author.id,
            content: text.to_owned(),
            raw_text: text.to_owned(),
            hashtags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            uri: Some(format!("https://remote.example/posts/{}", new_id())),
            ..NewStatus::default()
        },
    )
    .await
    .expect("the remote status fixture inserts")
}

/// A public local post by `author` answering `parent`, recording its
/// author as the replied-to account the way the create path does.
pub(super) async fn reply_to(pool: &PgPool, author: &User, parent: &Status) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            ..NewStatus::local(author, "in reply")
        },
    )
    .await
    .expect("the reply fixture inserts")
}

/// `follower` follows `target`, the fixture asserting the unlocked
/// local target granted it on the spot.
pub(super) async fn follow_user(pool: &PgPool, follower: &User, target: &User) {
    let row = follow::follow(pool, TEST_BASE_URL, follower, target)
        .await
        .expect("the follow fixture writes");
    assert_eq!(
        row.state,
        follow::FollowState::Accepted,
        "the unlocked local target grants it"
    );
}

/// Turns `follower`'s follow of `target` into a boosts-hidden one.
pub(super) async fn hide_boosts(pool: &PgPool, follower: &User, target: &User) {
    let toggled = follow::set_toggles(
        pool,
        follower.id,
        target.id,
        FollowToggles {
            show_reblogs: Some(false),
            notify: None,
        },
    )
    .await
    .expect("the toggle fixture writes")
    .expect("the relationship exists");
    assert!(!toggled.show_reblogs, "the stored row hides boosts");
}

/// A list of `owner` holding `member`, titled for its reply policy.
pub(super) async fn list_of(
    pool: &PgPool,
    owner: &User,
    member: &User,
    policy: &str,
) -> list::List {
    let created = list::create(
        pool,
        owner.id,
        &NewList {
            title: format!("{policy} list"),
            replies_policy: policy.to_owned(),
            ..NewList::default()
        },
    )
    .await
    .expect("the list fixture creates");
    let added = list::add_members(pool, created.id, &[member.id])
        .await
        .expect("the member fixture adds");
    assert_eq!(added, 1, "the membership is new");
    created
}

/// The three reply policies, owned by `dana` and holding `alice`.
pub(super) async fn all_three_lists(pool: &PgPool, dana: &User, alice: &User) -> Vec<list::List> {
    let mut made = Vec::new();
    for policy in ["none", "list", "followed"] {
        made.push(list_of(pool, dana, alice, policy).await);
    }
    made
}

/// The id of the entry of `lists` carrying this reply policy.
pub(super) fn list_id_of(lists: &[list::List], policy: &str) -> Uuid {
    lists
        .iter()
        .find(|list_row| list_row.replies_policy == policy)
        .expect("the fixture made all three policies")
        .id
}

/// Attaches one image to `row`.
pub(super) async fn attach_media(pool: &PgPool, author: &User, row: &Status) {
    let _fixture = media_attachment::insert(
        pool,
        &NewMediaAttachment {
            user_id: Some(author.id),
            status_id: Some(row.id),
            url: "https://cdn.example/pic.png".to_owned(),
            content_type: "image/png".to_owned(),
            ..NewMediaAttachment::default()
        },
    )
    .await
    .expect("the media fixture attaches");
}

/// `reblogger` boosts `original` as a public status.
pub(super) async fn boost(pool: &PgPool, reblogger: &User, original: &Status) -> Status {
    let (row, created) =
        status::create_reblog(pool, TEST_BASE_URL, reblogger, original, Visibility::Public)
            .await
            .expect("the reblog fixture writes");
    assert!(created, "the boost row is new");
    row
}

/// [`targets`]' unwrapped answer for a status the test expects to
/// publish.
pub(super) async fn published(
    state: &AppState,
    row: &Status,
    author: &User,
) -> (Vec<String>, Audience, bool) {
    targets(state, row, author)
        .await
        .expect("the computation reads")
        .expect("the fixture publishes")
}

/// The `user:<id>` hub name.
pub(super) fn user_stream(id: Uuid) -> String {
    format!("user:{id}")
}

/// The `direct:<id>` hub name.
pub(super) fn direct_stream(id: Uuid) -> String {
    format!("direct:{id}")
}

/// The `list:<id>` hub name.
pub(super) fn list_stream(id: Uuid) -> String {
    format!("list:{id}")
}

/// An expected stream set, spelled in the canonical order [`targets`]
/// answers in.
pub(super) fn canonical(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

/// The ids of a set, sorted, for order-insensitive comparison.
pub(super) fn sorted_ids(mut ids: Vec<Uuid>) -> Vec<Uuid> {
    ids.sort_unstable();
    ids
}

/// The `Audience::Users` ids of a computed audience, which the test
/// asserts are users-gated.
pub(super) fn users_ids(audience: &Audience) -> Vec<Uuid> {
    match audience {
        Audience::Users(ids) => ids.clone(),
        Audience::Public | Audience::Everyone => {
            panic!("a users-gated event was the audience {audience:?}")
        }
    }
}

/// The single event the emitter under test handed to `rx`, which the
/// test subscribed to before making the call: the hub delivers every
/// publish to its local subscribers at once, so one awaited emitter
/// call is already buffered here by the time this reads.
pub(super) async fn emitted(rx: &mut broadcast::Receiver<Arc<StreamEvent>>) -> StreamEvent {
    Arc::unwrap_or_clone(rx.recv().await.expect("the emitter published an event"))
}

/// Asserts `rx` holds no event: the emitter published nothing.
pub(super) fn assert_empty(rx: &mut broadcast::Receiver<Arc<StreamEvent>>) {
    assert!(
        matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "the emitter published an event it owed nothing"
    );
}
