//! Visibility matrix tests: `can_view`, the SQL clause and `filter_visible` agree.

use sqlx::{PgPool, Postgres, QueryBuilder};

use super::*;
use crate::{
    config::TEST_BASE_URL,
    models::{
        block, follow,
        status::{NewStatus, Visibility, insert as insert_status, soft_delete},
        user::{Flag, NewUser, insert as insert_user, local_ap_id},
    },
};

/// The instance origin the fixtures pretend to serve.
const BASE: &str = TEST_BASE_URL;

/// Inserts a local user named `nickname`.
async fn local(pool: &PgPool, nickname: &str) -> User {
    insert_user(
        pool,
        &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
    )
    .await
    .expect("the local fixture inserts")
}

/// Inserts a local user named `nickname` whose account is locked, so a
/// follow of it waits for an answer and never grants one.
async fn locked_local(pool: &PgPool, nickname: &str) -> User {
    insert_user(
        pool,
        &NewUser {
            locked: Flag::new(true),
            ..NewUser::local(nickname, &local_ap_id(BASE, nickname))
        },
    )
    .await
    .expect("the locked fixture inserts")
}

/// Inserts a mirror of the remote actor `nickname` on `remote.example`.
async fn remote(pool: &PgPool, nickname: &str) -> User {
    insert_user(
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

/// Inserts `text` as `author`'s status of `visibility`.
async fn post(pool: &PgPool, author: &User, visibility: Visibility, text: &str) -> Status {
    insert_status(
        pool,
        BASE,
        &NewStatus {
            visibility,
            ..NewStatus::local(author, text)
        },
    )
    .await
    .expect("a plain local post inserts")
}

/// Inserts a `direct` status of `author` mentioning `mentioned`.
async fn direct(pool: &PgPool, author: &User, mentioned: &[&User]) -> Status {
    let mentioned_user_ids = mentioned.iter().map(|user| user.id).collect();
    insert_status(
        pool,
        BASE,
        &NewStatus {
            visibility: Visibility::Direct,
            mentioned_user_ids,
            ..NewStatus::local(author, "a direct message")
        },
    )
    .await
    .expect("a direct post with mentions inserts")
}

/// Inserts a `private` status of `author` mentioning `mentioned`.
async fn private_mentioning(pool: &PgPool, author: &User, mentioned: &[&User]) -> Status {
    let mentioned_user_ids = mentioned.iter().map(|user| user.id).collect();
    insert_status(
        pool,
        BASE,
        &NewStatus {
            visibility: Visibility::Private,
            mentioned_user_ids,
            ..NewStatus::local(author, "a followers-only reply")
        },
    )
    .await
    .expect("a private post with mentions inserts")
}

/// Inserts `reblogger`'s reblog of `original`, itself public.
async fn reblog(pool: &PgPool, reblogger: &User, original: &Status) -> Status {
    insert_status(
        pool,
        BASE,
        &NewStatus {
            reblog_of_id: Some(original.id),
            ..NewStatus::local(reblogger, "")
        },
    )
    .await
    .expect("a reblog inserts")
}

/// Every stored row, tombstone included, oldest id first.
async fn stored_statuses(pool: &PgPool) -> Vec<Status> {
    sqlx::query_as::<_, Status>("SELECT * FROM statuses ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("the table reads")
}

/// The ids of `statuses`, in order.
fn ids_of(statuses: &[&Status]) -> Vec<Uuid> {
    statuses.iter().map(|status| status.id).collect()
}

/// A set of ids, for comparing one reader's answer against another's
/// whatever order each came back in.
fn id_set(ids: &[Uuid]) -> HashSet<Uuid> {
    ids.iter().copied().collect()
}

/// The ids among `statuses` that [`can_view`] admits for `viewer`.
async fn can_view_ids(pool: &PgPool, statuses: &[Status], viewer: Option<&User>) -> Vec<Uuid> {
    let mut visible = Vec::new();
    for status in statuses {
        if can_view(pool, status, viewer).await.unwrap() {
            visible.push(status.id);
        }
    }
    visible
}

/// The ids [`push_visibility_clause`] admits for `viewer`, selected
/// straight off the table, oldest first.
async fn clause_ids(pool: &PgPool, viewer: Option<&User>) -> Vec<Uuid> {
    let mut builder = QueryBuilder::<Postgres>::new("SELECT s.id FROM statuses s WHERE true");
    push_visibility_clause(&mut builder, viewer, "s");
    let rows = builder
        .build_query_as::<(Uuid,)>()
        .fetch_all(pool)
        .await
        .expect("the clause selects");
    rows.into_iter().map(|(id,)| id).collect()
}

/// The seeded matrix: five accounts and twelve statuses, each one a
/// case of the rules — see [`seed`] for which is which.
#[derive(Debug)]
struct Matrix {
    /// The local author of everything blockable; blocks carol.
    alice: User,
    /// The local accepted follower of alice, pending follower of dave.
    bob: User,
    /// The local account alice blocked; author of two of her own.
    carol: User,
    /// The locked local author of one private status.
    dave: User,
    /// The remote mirror, tested as a viewer struct only: remote
    /// users never log in (decision 7).
    mallory: User,
    /// alice's `public` post.
    public: Status,
    /// alice's `unlisted` post.
    unlisted: Status,
    /// alice's `local` post: instance viewers only.
    local_post: Status,
    /// alice's `private` post: alice and bob only.
    private_post: Status,
    /// alice's `private` post mentioning mallory, a non-follower.
    private_to_mallory: Status,
    /// alice's `direct` post mentioning bob.
    dm_to_bob: Status,
    /// alice's `direct` post mentioning nobody but its author.
    dm_to_nobody: Status,
    /// dave's `private` post, with bob's request still pending.
    dave_private: Status,
    /// bob's reblog of alice's private post.
    reblog_of_private: Status,
    /// alice's public post, soft-deleted: visible to nobody.
    deleted_public: Status,
    /// carol's public post, seen past alice's block of her.
    carol_public: Status,
    /// carol's reblog of the deleted public post: hidden by rule 7.
    reblog_of_deleted: Status,
}

/// Writes the matrix: the accounts, their follow and block edges, and
/// one status per interesting case (the [`Matrix`] fields name them).
async fn seed(pool: &PgPool) -> Matrix {
    let alice = local(pool, "alice").await;
    let bob = local(pool, "bob").await;
    let carol = local(pool, "carol").await;
    let dave = locked_local(pool, "dave").await;
    let mallory = remote(pool, "mallory").await;

    let following = follow::follow(pool, BASE, &bob, &alice)
        .await
        .expect("bob's follow of open alice stores");
    assert_eq!(
        following.state,
        follow::FollowState::Accepted,
        "alice is unlocked, so the follow is granted"
    );
    let pending = follow::follow(pool, BASE, &bob, &dave)
        .await
        .expect("bob's request of locked dave stores");
    assert_eq!(
        pending.state,
        follow::FollowState::Pending,
        "dave is locked, so the request waits"
    );
    let blocked = block::block(pool, BASE, &alice, &carol)
        .await
        .expect("alice's block of carol stores");
    assert_eq!(
        blocked.blocked_id, carol.id,
        "the fixture block is alice's on carol"
    );

    let public = post(pool, &alice, Visibility::Public, "public").await;
    let unlisted = post(pool, &alice, Visibility::Unlisted, "unlisted").await;
    let local_post = post(pool, &alice, Visibility::Local, "local").await;
    let private_post = post(pool, &alice, Visibility::Private, "private").await;
    let private_to_mallory = private_mentioning(pool, &alice, &[&mallory]).await;
    let dm_to_bob = direct(pool, &alice, &[&bob]).await;
    let dm_to_nobody = post(pool, &alice, Visibility::Direct, "to nobody").await;
    let dave_private = post(pool, &dave, Visibility::Private, "dave private").await;
    let reblog_of_private = reblog(pool, &bob, &private_post).await;
    let tombstoned = post(pool, &alice, Visibility::Public, "deleted").await;
    let deleted_public = soft_delete(pool, tombstoned.id)
        .await
        .expect("the soft delete runs")
        .expect("the post was live");
    let carol_public = post(pool, &carol, Visibility::Public, "carol public").await;
    let reblog_of_deleted = reblog(pool, &carol, &deleted_public).await;

    Matrix {
        alice,
        bob,
        carol,
        dave,
        mallory,
        public,
        unlisted,
        local_post,
        private_post,
        private_to_mallory,
        dm_to_bob,
        dm_to_nobody,
        dave_private,
        reblog_of_private,
        deleted_public,
        carol_public,
        reblog_of_deleted,
    }
}

/// The matrix's single-row edge cases through `can_view`: tombstones,
/// a pending follow, an author's own private post and a reblog whose
/// original was deleted.
async fn assert_edge_cases(pool: &PgPool, m: &Matrix) {
    let cases = [
        (
            &m.deleted_public,
            &m.alice,
            false,
            "not even the author sees a tombstone",
        ),
        (
            &m.dave_private,
            &m.dave,
            true,
            "dave sees his own private post",
        ),
        (
            &m.dave_private,
            &m.bob,
            false,
            "bob's pending request is not a follow",
        ),
        (
            &m.reblog_of_deleted,
            &m.carol,
            false,
            "a reblog falls with its original",
        ),
    ];
    for (status, viewer, want, why) in cases {
        let seen = can_view(pool, status, Some(viewer)).await.unwrap();
        assert_eq!(seen, want, "{why}");
    }
}

/// The full matrix, answered through `can_view`, the SQL clause and
/// `filter_visible` for five viewers — author, follower, blocked,
/// remote mirror and anonymous — with every row and every rule in
/// play, including pending follows, mention-scoped directs,
/// tombstones, and reblogs whose original fell out of view.
#[tokio::test]
async fn the_matrix_agrees_through_every_reader() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let m = seed(&pool).await;
    let all = stored_statuses(&pool).await;
    assert_eq!(all.len(), 12, "every fixture row is stored");
    assert_edge_cases(&pool, &m).await;

    let for_alice = ids_of(&[
        &m.public,
        &m.unlisted,
        &m.local_post,
        &m.private_post,
        &m.private_to_mallory,
        &m.dm_to_bob,
        &m.dm_to_nobody,
        &m.reblog_of_private,
        &m.carol_public,
    ]);
    let for_bob = ids_of(&[
        &m.public,
        &m.unlisted,
        &m.local_post,
        &m.private_post,
        &m.private_to_mallory,
        &m.dm_to_bob,
        &m.reblog_of_private,
        &m.carol_public,
    ]);
    let for_carol = ids_of(&[&m.carol_public]);
    let for_mallory = ids_of(&[
        &m.public,
        &m.unlisted,
        &m.private_to_mallory,
        &m.carol_public,
    ]);
    let for_anonymous = ids_of(&[&m.public, &m.unlisted, &m.carol_public]);

    for (name, viewer, want) in [
        ("alice", Some(&m.alice), for_alice),
        ("bob", Some(&m.bob), for_bob),
        ("carol", Some(&m.carol), for_carol),
        ("mallory", Some(&m.mallory), for_mallory),
        ("anonymous", None, for_anonymous),
    ] {
        let via_can_view = can_view_ids(&pool, &all, viewer).await;
        assert_eq!(id_set(&via_can_view), id_set(&want), "can_view, {name}");
        let via_clause = clause_ids(&pool, viewer).await;
        assert_eq!(id_set(&via_clause), id_set(&want), "clause, {name}");
        let filtered = filter_visible(&pool, all.clone(), viewer)
            .await
            .expect("the batch filter runs");
        let via_filter: Vec<Uuid> = filtered.iter().map(|status| status.id).collect();
        assert_eq!(id_set(&via_filter), id_set(&want), "filter_visible, {name}");
        let mut in_id_order = via_filter.clone();
        in_id_order.sort();
        assert_eq!(
            via_filter, in_id_order,
            "filter_visible, {name}, keeps the input (id) order"
        );
    }

    let empty = filter_visible(&pool, Vec::new(), Some(&m.alice))
        .await
        .expect("the empty batch needs no database");
    assert!(empty.is_empty(), "no rows in answers no rows out");
}

/// Rule 7 through both readers: bob's public reblog of alice's
/// private post travels exactly as far as the original does — to
/// alice and bob, and nowhere else.
#[tokio::test]
async fn a_reblog_is_visible_only_with_its_original() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let carol = local(&pool, "carol").await;
    let following = follow::follow(&pool, BASE, &bob, &alice)
        .await
        .expect("bob follows alice");
    assert_eq!(
        following.state,
        follow::FollowState::Accepted,
        "the fixture follow is granted"
    );
    let secret = post(&pool, &alice, Visibility::Private, "followers only").await;
    let reblogged = reblog(&pool, &bob, &secret).await;

    for viewer in [Some(&bob), Some(&alice)] {
        assert!(
            can_view(&pool, &reblogged, viewer).await.unwrap(),
            "whoever may see the original may see the reblog"
        );
    }
    for viewer in [Some(&carol), None] {
        assert!(
            !can_view(&pool, &reblogged, viewer).await.unwrap(),
            "whoever may not see the original may not see the reblog"
        );
    }

    assert_eq!(
        id_set(&clause_ids(&pool, Some(&bob)).await),
        id_set(&[secret.id, reblogged.id]),
        "the clause admits both rows to the follower"
    );
    assert_eq!(
        id_set(&clause_ids(&pool, Some(&alice)).await),
        id_set(&[secret.id, reblogged.id]),
        "and both to the original's author"
    );
    assert!(
        clause_ids(&pool, Some(&carol)).await.is_empty(),
        "the stranger gets neither row"
    );
    assert!(
        clause_ids(&pool, None).await.is_empty(),
        "and neither does the anonymous request"
    );
}

/// Rule 4 through both readers: a `local` status answers a local
/// viewer, and loses to the anonymous request — remote users never
/// log in, so that is the audience the rule can ever be asked about
/// (a remote mirror passed as a viewer struct is refused the same
/// way).
#[tokio::test]
async fn local_visibility_needs_a_local_viewer() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let mallory = remote(&pool, "mallory").await;
    let kept_local = post(&pool, &alice, Visibility::Local, "instance only").await;

    assert!(
        can_view(&pool, &kept_local, Some(&bob)).await.unwrap(),
        "a local member of the audience sees it"
    );
    assert!(
        !can_view(&pool, &kept_local, Some(&mallory)).await.unwrap(),
        "a remote mirror does not, even as a viewer struct"
    );
    assert!(
        !can_view(&pool, &kept_local, None).await.unwrap(),
        "the anonymous request does not"
    );

    assert_eq!(
        clause_ids(&pool, Some(&bob)).await,
        vec![kept_local.id],
        "the clause lets the local viewer through"
    );
    assert!(
        clause_ids(&pool, Some(&mallory)).await.is_empty(),
        "and shuts the remote viewer out"
    );
    assert!(
        clause_ids(&pool, None).await.is_empty(),
        "and shuts the anonymous request out"
    );
}

/// Rule 2 through both readers: alice's block shuts carol out of
/// everything alice posted — public posts and a mention-scoped direct
/// alike, because the block is judged before the visibility — while
/// carol keeps her own posts and alice keeps carol's.
#[tokio::test]
async fn a_blocked_viewer_sees_nothing_of_the_author() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let carol = local(&pool, "carol").await;
    let blocked = block::block(&pool, BASE, &alice, &carol)
        .await
        .expect("alice's block of carol stores");
    assert_eq!(
        blocked.blocked_id, carol.id,
        "the fixture block is alice's on carol"
    );
    let open_post = post(&pool, &alice, Visibility::Public, "public anyway").await;
    let dm = direct(&pool, &alice, &[&carol]).await;
    let hers = post(&pool, &carol, Visibility::Public, "her own").await;

    assert!(
        !can_view(&pool, &open_post, Some(&carol)).await.unwrap(),
        "the block outranks public visibility"
    );
    assert!(
        !can_view(&pool, &dm, Some(&carol)).await.unwrap(),
        "and it outranks the mention that would have admitted her"
    );
    assert!(
        can_view(&pool, &hers, Some(&carol)).await.unwrap(),
        "she still sees her own"
    );
    assert!(
        can_view(&pool, &hers, Some(&alice)).await.unwrap(),
        "the rule hides the author from the blocked, not the author's view back"
    );

    assert_eq!(
        id_set(&clause_ids(&pool, Some(&carol)).await),
        id_set(&[hers.id]),
        "the clause keeps carol to her own row"
    );
    assert_eq!(
        id_set(&clause_ids(&pool, Some(&alice)).await),
        id_set(&[open_post.id, dm.id, hers.id]),
        "alice's own view is narrowed by nothing here"
    );
}

/// Rule 5's mentionee arm through every reader: bob does not follow
/// alice, but her followers-only reply mentions him.
#[tokio::test]
async fn a_private_status_is_visible_to_a_mentioned_non_follower() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let reply = private_mentioning(&pool, &alice, &[&bob]).await;

    assert!(
        can_view(&pool, &reply, Some(&bob)).await.unwrap(),
        "the mentioned account sees it without following"
    );
    let kept = filter_visible(&pool, vec![reply.clone()], Some(&bob))
        .await
        .expect("the batch filter runs");
    assert_eq!(ids_of(&kept.iter().collect::<Vec<_>>()), vec![reply.id]);
    assert_eq!(clause_ids(&pool, Some(&bob)).await, vec![reply.id]);
    assert!(
        can_view(&pool, &reply, Some(&alice)).await.unwrap(),
        "the author still sees it"
    );
    assert!(
        !can_view(&pool, &reply, None).await.unwrap(),
        "an anonymous request is not mentioned"
    );
}

/// Regression: a private status that mentions someone else stays hidden
/// from a non-follower, in both readers.
#[tokio::test]
async fn a_private_status_stays_hidden_from_an_unmentioned_non_follower() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let carol = local(&pool, "carol").await;
    let reply = private_mentioning(&pool, &alice, &[&bob]).await;

    assert!(
        !can_view(&pool, &reply, Some(&carol)).await.unwrap(),
        "carol is neither a follower nor mentioned"
    );
    let kept = filter_visible(&pool, vec![reply.clone()], Some(&carol))
        .await
        .expect("the batch filter runs");
    assert!(kept.is_empty(), "the batch filter drops it too");
    assert!(
        clause_ids(&pool, Some(&carol)).await.is_empty(),
        "the clause hides it from carol too"
    );
}

/// Rule 2 still outranks the mention: alice blocked bob, so her private
/// post mentioning him is invisible to him.
#[tokio::test]
async fn a_mentioned_viewer_blocked_by_the_author_still_sees_nothing() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let _blocked = block::block(&pool, BASE, &alice, &bob)
        .await
        .expect("alice's block of bob stores");
    let reply = private_mentioning(&pool, &alice, &[&bob]).await;

    assert!(!can_view(&pool, &reply, Some(&bob)).await.unwrap());
    let kept = filter_visible(&pool, vec![reply], Some(&bob))
        .await
        .expect("the batch filter runs");
    assert!(kept.is_empty(), "the block wins in the batch filter too");
}

/// Rule 7 with a mentioned viewer: carol's public reblog of alice's
/// private post, which mentions bob, reaches bob and nobody unmentioned.
#[tokio::test]
async fn a_reblog_rule_applies_to_a_mentioned_viewer() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let carol = local(&pool, "carol").await;
    let dave = local(&pool, "dave").await;
    let original = private_mentioning(&pool, &alice, &[&bob]).await;
    let boosted = reblog(&pool, &carol, &original).await;

    assert!(can_view(&pool, &boosted, Some(&bob)).await.unwrap());
    let for_bob = filter_visible(&pool, vec![boosted.clone()], Some(&bob))
        .await
        .expect("the batch filter runs");
    assert_eq!(
        ids_of(&for_bob.iter().collect::<Vec<_>>()),
        vec![boosted.id]
    );
    assert_eq!(
        id_set(&clause_ids(&pool, Some(&bob)).await),
        id_set(&[original.id, boosted.id])
    );

    assert!(!can_view(&pool, &boosted, Some(&dave)).await.unwrap());
    let for_dave = filter_visible(&pool, vec![boosted], Some(&dave))
        .await
        .expect("the batch filter runs");
    assert!(
        for_dave.is_empty(),
        "an unmentioned viewer loses the reblog"
    );
}
