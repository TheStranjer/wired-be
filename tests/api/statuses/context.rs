//! `GET /api/v1/statuses/{id}/context`: Pleroma's thread read — ancestors
//! root-first, descendants in thread order (depth first, siblings by publish
//! time, never by local id), invisible, deleted and
//! silenced links skipped but walked past, `parent_visible` on every entry,
//! and the optional-auth gate on the route itself. The local helpers below
//! build and read the thread fixtures; the route wrapper and the id reader
//! live in `support` because the quoting tests page through a context too.

use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        block,
        mute::{self, MuteOptions},
        status::{self, NewStatus, Status, Visibility},
        user::User,
    },
};

use super::support::{
    INVALID_TOKEN, NO_SCOPE, NOT_FOUND, SCOPES, UNKNOWN_ID, WRITE, context, context_ids, deleted,
    followed, ids_of, user_with_token,
};
use crate::common;

/// Inserts `author`'s `text` as a reply to `parent` at `visibility`, and
/// returns its row: the thread fixture the context tests walk.
async fn reply_under(
    pool: &PgPool,
    author: &User,
    parent: &Status,
    text: &str,
    visibility: Visibility,
) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            visibility,
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a reply fixture inserts")
}

/// The `pleroma.parent_visible` of the entry `id` of one arm.
fn parent_visible(body: &Value, arm: &str, id: &str) -> bool {
    body[arm]
        .as_array()
        .expect("the arm is an array")
        .iter()
        .find(|status| status["id"] == id)
        .unwrap_or_else(|| panic!("`{id}` is listed under `{arm}`"))["pleroma"]["parent_visible"]
        .as_bool()
        .expect("parent_visible is a bool")
}

/// The answer's shape, checked once on the linear thread: an object of
/// exactly the two arrays.
fn assert_two_arms(body: &Value) {
    let arms: Vec<&String> = body
        .as_object()
        .expect("the answer is an object")
        .keys()
        .collect();
    assert_eq!(arms, vec!["ancestors", "descendants"], "the two arms");
}

/// A linear thread answers its chain in both directions from any entry:
/// ancestors root-first (oldest first), descendants in flat chronological
/// order, each one's `parent_visible` true because its direct parent came
/// back — and a root with no replies at all answers two empty arrays.
#[tokio::test]
async fn a_linear_thread_answers_both_ways_around_its_named_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let second = reply_under(&pool, &bob, &root, "second", Visibility::Public).await;
    let third = reply_under(&pool, &carol, &second, "third", Visibility::Public).await;
    let leaf = reply_under(&pool, &dave, &third, "leaf", Visibility::Public).await;

    // Mid-thread: the whole chain above, the rest below.
    let response = context(&app, None, &third.id.to_string()).await;
    common::assert_answer(&response, StatusCode::OK, "mid-thread, anonymous");
    let body = response.json_value();
    assert_two_arms(&body);
    assert_eq!(
        context_ids(&body, "ancestors"),
        ids_of(&[&root, &second]),
        "root first, the named status itself absent"
    );
    assert_eq!(context_ids(&body, "descendants"), ids_of(&[&leaf]));
    assert!(
        parent_visible(&body, "descendants", &leaf.id.to_string()),
        "the leaf answers the third, which came back"
    );
    assert_eq!(
        body["ancestors"][0]["account"]["acct"], "alice",
        "the arrays carry rendered entities, not bare ids"
    );

    // From the root: nothing above, both later replies below in id order.
    let from_root = context(&app, None, &root.id.to_string()).await.json_value();
    assert!(
        context_ids(&from_root, "ancestors").is_empty(),
        "the root has no thread above it"
    );
    assert_eq!(
        context_ids(&from_root, "descendants"),
        ids_of(&[&second, &third, &leaf]),
        "flat chronological order, not tree order"
    );
    assert!(
        parent_visible(&from_root, "descendants", &second.id.to_string()),
        "the root is shown, so its direct reply has its parent"
    );

    // From the leaf: everything above, nothing below.
    let from_leaf = context(&app, None, &leaf.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&from_leaf, "ancestors"),
        ids_of(&[&root, &second, &third]),
        "and never the named status itself"
    );
    assert!(
        context_ids(&from_leaf, "descendants").is_empty(),
        "the leaf has no thread below it"
    );

    // A root nobody answered: both arrays empty, and still a 200.
    let lonely = common::create_status(&pool, &alice, "lonely", Visibility::Public).await;
    let from_lonely = context(&app, None, &lonely.id.to_string())
        .await
        .json_value();
    assert!(
        context_ids(&from_lonely, "ancestors").is_empty(),
        "nothing above"
    );
    assert!(
        context_ids(&from_lonely, "descendants").is_empty(),
        "nothing below"
    );
}

/// A branching thread answers every branch below the named status in one
/// list in thread order — depth first, so the second hop of the left branch
/// follows its own parent, before the right branch's younger root reply —
/// and each entry's `parent_visible` names whoever it really answered.
#[tokio::test]
async fn a_branching_thread_answers_a_depth_first_descendants_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let left = reply_under(&pool, &bob, &root, "left", Visibility::Public).await;
    let right = reply_under(&pool, &carol, &root, "right", Visibility::Public).await;
    let deep = reply_under(&pool, &dave, &left, "deep", Visibility::Public).await;

    let from_root = context(&app, None, &root.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&from_root, "descendants"),
        ids_of(&[&left, &deep, &right]),
        "depth first: `deep` right after the `left` it answers, then `right`"
    );
    for reply in [&left, &right, &deep] {
        assert!(
            parent_visible(&from_root, "descendants", &reply.id.to_string()),
            "`{}` answers a row that came back",
            reply.id
        );
    }

    // A branch entry sees only its own ancestry and its own descendants.
    let from_left = context(&app, None, &left.id.to_string()).await.json_value();
    assert_eq!(context_ids(&from_left, "ancestors"), ids_of(&[&root]));
    assert_eq!(context_ids(&from_left, "descendants"), ids_of(&[&deep]));
    let from_right = context(&app, None, &right.id.to_string())
        .await
        .json_value();
    assert_eq!(context_ids(&from_right, "ancestors"), ids_of(&[&root]));
    assert!(
        context_ids(&from_right, "descendants").is_empty(),
        "the other branch is not below this reply"
    );
}

/// Inserts `author`'s `text` as a reply to `parent` published at
/// `published`, and returns its row.
async fn reply_published(
    pool: &PgPool,
    author: &User,
    parent: &Status,
    text: &str,
    published: DateTime<Utc>,
) -> Status {
    status::insert(
        pool,
        TEST_BASE_URL,
        &NewStatus {
            in_reply_to_id: Some(parent.id),
            in_reply_to_account_id: Some(parent.user_id),
            inserted_at: Some(published),
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a dated reply fixture inserts")
}

/// A thread stored deepest-first (what backfill does, so the ids run
/// against the thread) still answers root to leaf: the order follows the
/// replies' `inserted_at`, not their ids.
#[tokio::test]
async fn backfilled_replies_with_reversed_ids_answer_in_thread_order() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let base = Utc::now() - Duration::hours(1);
    // Each row is stored before its parent exists, then linked, newest first.
    let mut stored = Vec::new();
    for (minutes, text) in [(30, "r3"), (20, "r2"), (10, "r1")] {
        let row = status::insert(
            &pool,
            TEST_BASE_URL,
            &NewStatus {
                inserted_at: Some(base + Duration::minutes(minutes)),
                ..NewStatus::local(&alice, text)
            },
        )
        .await
        .expect("an unlinked reply fixture inserts");
        stored.push(row);
    }
    let [r3, r2, r1] = <[Status; 3]>::try_from(stored).expect("three rows");
    assert!(r3.id < r2.id && r2.id < r1.id, "ids run against the thread");
    for (child, parent) in [(&r3, &r2), (&r2, &r1), (&r1, &root)] {
        let linked = status::link_backfilled_reply(&pool, child.id, parent)
            .await
            .expect("the link write runs");
        assert!(linked, "the deferred link closes");
    }

    let from_root = context(&app, None, &root.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&from_root, "descendants"),
        ids_of(&[&r1, &r2, &r3]),
        "root to leaf despite the reversed ids"
    );
}

/// Siblings are ordered by publish time and each is followed by its own
/// subtree, whatever order the rows were stored in: `A` (older, stored
/// last), its child `A1`, then `B`.
#[tokio::test]
async fn siblings_answer_by_publish_time_each_followed_by_its_subtree() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let base = Utc::now() - Duration::hours(1);
    let newer = reply_published(&pool, &alice, &root, "B", base + Duration::minutes(20)).await;
    let older = reply_published(&pool, &alice, &root, "A", base + Duration::minutes(10)).await;
    let child = reply_published(&pool, &alice, &older, "A1", base + Duration::minutes(30)).await;
    assert!(newer.id < older.id, "the younger sibling has the older id");

    let from_root = context(&app, None, &root.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&from_root, "descendants"),
        ids_of(&[&older, &child, &newer]),
        "A, then its child A1, then B"
    );
}

/// A `private` reply is the author's and an accepted follower's, not a
/// stranger's — but skipping it never breaks the walk: the stranger's
/// answer still carries the public reply *under* it with
/// `parent_visible: false`, and its ancestors list still reaches the root
/// past the hidden link.
#[tokio::test]
async fn an_invisible_reply_is_skipped_but_the_walk_continues_past_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let (_mallory, mallory_token) = user_with_token(&pool, "mallory", SCOPES).await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let secret = reply_under(&pool, &alice, &root, "secret", Visibility::Private).await;
    followed(&pool, &bob, &alice).await;
    let answer = reply_under(&pool, &bob, &secret, "answer", Visibility::Public).await;

    // A stranger (and the anonymous caller alike): the private link drops
    // out of both arms, the thread past it stays, and its parent shows the
    // hole.
    for (who, token) in [
        ("a stranger", Some(mallory_token.as_str())),
        ("anonymous", None),
    ] {
        let hidden = context(&app, token, &root.id.to_string())
            .await
            .json_value();
        assert_eq!(
            context_ids(&hidden, "descendants"),
            ids_of(&[&answer]),
            "{who}: the private reply is out, its public answer stays"
        );
        assert!(
            !parent_visible(&hidden, "descendants", &answer.id.to_string()),
            "{who}: the hidden parent did not come back"
        );
        let past = context(&app, token, &answer.id.to_string())
            .await
            .json_value();
        assert_eq!(
            context_ids(&past, "ancestors"),
            ids_of(&[&root]),
            "{who}: the walk passes through the hidden link to the root"
        );
    }

    // The accepted follower sees the whole stretch, holes and all.
    let follower_root = context(&app, Some(bob_token.as_str()), &root.id.to_string())
        .await
        .json_value();
    assert_eq!(
        context_ids(&follower_root, "descendants"),
        ids_of(&[&secret, &answer]),
        "the follower's descendants carry the private reply too"
    );
    assert!(
        parent_visible(&follower_root, "descendants", &answer.id.to_string()),
        "and its parent is there for them"
    );
    let follower_answer = context(&app, Some(bob_token.as_str()), &answer.id.to_string())
        .await
        .json_value();
    assert_eq!(
        context_ids(&follower_answer, "ancestors"),
        ids_of(&[&root, &secret])
    );

    // The private reply the stranger may not see is the shared 404 even as
    // the named status of a context read.
    common::assert_all_refused(
        &[
            context(&app, Some(mallory_token.as_str()), &secret.id.to_string()).await,
            context(&app, None, &secret.id.to_string()).await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the hidden reply named directly",
    );
}

/// Statuses by accounts the viewer blocked or muted drop out of
/// `descendants` — while the thread below the silenced reply still arrives,
/// `parent_visible: false` marking its author's absence, and the silenced
/// author's own public post staying in `ancestors`, which the visibility
/// model alone narrows. An anonymous caller, who silences nobody, sees
/// everything public.
#[tokio::test]
async fn a_silenced_author_vanishes_from_the_descendants() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (carol, carol_token) = user_with_token(&pool, "carol", SCOPES).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &carol, "root", Visibility::Public).await;
    let blocked = reply_under(&pool, &bob, &root, "blocked", Visibility::Public).await;
    let muted = reply_under(&pool, &dave, &root, "muted", Visibility::Public).await;
    let own = reply_under(&pool, &carol, &root, "own", Visibility::Public).await;
    let under = reply_under(&pool, &erin, &blocked, "under", Visibility::Public).await;
    let block_row = block::block(&pool, TEST_BASE_URL, &carol, &bob)
        .await
        .expect("carol's block of bob stores");
    assert_eq!(
        block_row.blocked_id, bob.id,
        "the fixture block is carol's on bob"
    );
    let mute_row = mute::mute(&pool, &carol, &dave, MuteOptions::default())
        .await
        .expect("carol's mute of dave stores");
    assert_eq!(
        mute_row.muted_id, dave.id,
        "and the mute is carol's on dave"
    );

    let for_carol = context(&app, Some(carol_token.as_str()), &root.id.to_string())
        .await
        .json_value();
    assert_eq!(
        context_ids(&for_carol, "descendants"),
        ids_of(&[&under, &own]),
        "the blocked and muted authors' rows are out; the viewer's own row \
         stays, and so does the live reply under the silenced one (which \
         follows its parent's place in the thread, ahead of `own`)"
    );
    assert!(
        parent_visible(&for_carol, "descendants", &own.id.to_string()),
        "carol answers the root, which is shown"
    );
    assert!(
        !parent_visible(&for_carol, "descendants", &under.id.to_string()),
        "the row under bob's answer marks its parent missing"
    );

    // Silence is the viewer's own filter, not a visibility rule: bob's
    // public reply still stands in carol's ancestors chain, and an
    // anonymous caller's descendants list is whole.
    let for_under = context(&app, Some(carol_token.as_str()), &under.id.to_string())
        .await
        .json_value();
    assert_eq!(
        context_ids(&for_under, "ancestors"),
        ids_of(&[&root, &blocked]),
        "ancestors are narrowed by visibility alone"
    );
    let anon = context(&app, None, &root.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&anon, "descendants"),
        ids_of(&[&blocked, &under, &muted, &own]),
        "nobody is silenced for the anonymous caller"
    );
    assert!(
        parent_visible(&anon, "descendants", &under.id.to_string()),
        "and its parent came back for them"
    );
}

/// A deleted reply is out of both arms — the visibility model lets a
/// tombstone stand for nobody — while the live reply under it walks past:
/// it arrives in the descendants with `parent_visible: false`, and the
/// root still heads its ancestors. Naming the tombstone itself is the 404.
#[tokio::test]
async fn a_deleted_reply_drops_out_of_both_arms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let middle = reply_under(&pool, &alice, &root, "middle", Visibility::Public).await;
    let leaf = reply_under(&pool, &alice, &middle, "leaf", Visibility::Public).await;
    deleted(&pool, &middle).await;

    let from_root = context(&app, None, &root.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&from_root, "descendants"),
        ids_of(&[&leaf]),
        "the tombstone is out, its live child still arrives"
    );
    assert!(
        !parent_visible(&from_root, "descendants", &leaf.id.to_string()),
        "and the child marks its deleted parent missing"
    );

    let from_leaf = context(&app, None, &leaf.id.to_string()).await.json_value();
    assert_eq!(
        context_ids(&from_leaf, "ancestors"),
        ids_of(&[&root]),
        "the walk passes through the deleted link to the root"
    );

    common::assert_all_refused(
        &[context(&app, None, &middle.id.to_string()).await],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the tombstone named directly",
    );
}

/// The context route runs the show route's optional-auth gate: no token and
/// an app-only token proceed as anonymous, a presented user token without
/// `read:statuses` is the scope 403, a token that does not resolve the 401,
/// and a malformed, unknown, deleted or invisible status is one
/// indistinguishable 404.
#[tokio::test]
async fn the_context_route_runs_the_optional_read_statuses_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _alice_token) = user_with_token(&pool, "alice", SCOPES).await;
    let (_bob, bob_token) = user_with_token(&pool, "bob", SCOPES).await;
    let (_noscope, noscope_token) = user_with_token(&pool, "mallory", WRITE).await;
    let app_only = common::create_app_only_token(&pool, SCOPES).await;
    let app = common::test_app(pool.clone());
    let root = common::create_status(&pool, &alice, "root", Visibility::Public).await;
    let secret = common::create_status(&pool, &alice, "secret", Visibility::Private).await;
    let gone = common::create_status(&pool, &alice, "gone", Visibility::Public).await;
    deleted(&pool, &gone).await;
    let id = root.id.to_string();

    // Unauthenticated reads succeed on public threads, private rows aside.
    common::assert_answer(
        &context(&app, None, &id).await,
        StatusCode::OK,
        "no token at all",
    );
    common::assert_answer(
        &context(&app, Some(&app_only), &id).await,
        StatusCode::OK,
        "an app-only token, which proceeds unauthenticated",
    );
    for (who, token) in [
        ("a stranger's user token", Some(bob_token.as_str())),
        ("the anonymous caller", None),
    ] {
        common::assert_all_refused(
            &[context(&app, token, &secret.id.to_string()).await],
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("{who}, naming a private status"),
        );
    }
    common::assert_all_refused(
        &[context(&app, Some(&noscope_token), &id).await],
        StatusCode::FORBIDDEN,
        NO_SCOPE,
        "a user token without read:statuses",
    );
    let refused = context(&app, Some("token-that-was-never-minted"), &id).await;
    common::assert_answer(&refused, StatusCode::UNAUTHORIZED, "a dead token");
    common::assert_error(&refused, INVALID_TOKEN, "a dead token");

    for (probe, who) in [
        ("not-an-id".to_owned(), "text that is not a UUID"),
        (format!("urn:uuid:{UNKNOWN_ID}"), "an unread id spelling"),
        (UNKNOWN_ID.to_owned(), "an id that names nothing"),
        (gone.id.to_string(), "a deleted status"),
    ] {
        common::assert_all_refused(
            &[context(&app, None, &probe).await],
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            who,
        );
    }
}
