//! The thread tests: the context read, the pinned and reblogged variants,
//! the conversation list and the thread participation of every viewer.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{NOT_FOUND, STATUSES, UNKNOWN_ID, get, world};
use super::thread_support::{
    context, context_arms, own_participation, parent_visible, post_thread,
};
use crate::common;

/// The author reads their own thread whole, from every leg: ancestors
/// root-first, descendants in thread order, and every
/// descendant's `parent_visible` true because the chain's direct parents
/// all came back (or are the named status itself).
#[tokio::test]
async fn the_author_reads_their_whole_thread_back_in_both_arms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;
    let author = Some(world.author.token.as_str());

    {
        let response = context(&app, author, &thread.root).await;
        common::assert_answer(&response, StatusCode::OK, "the author at the root");
        let body = response.json_value();
        let (ancestors, descendants) = context_arms(&body, "the author at the root");
        assert!(
            ancestors.is_empty(),
            "the root answers nothing above itself"
        );
        assert_eq!(
            descendants,
            vec![thread.reply.clone(), thread.deep.clone()],
            "the descent follows the thread: reply, then the answer under it"
        );
        assert!(
            parent_visible(&body, &thread.reply),
            "the reply's parent is the named root"
        );
        assert!(
            parent_visible(&body, &thread.deep),
            "the reply-of-reply's parent came back too"
        );
    }

    {
        let response = context(&app, author, &thread.reply).await;
        common::assert_answer(&response, StatusCode::OK, "the author at the reply");
        let body = response.json_value();
        let (ancestors, descendants) = context_arms(&body, "the author at the reply");
        assert_eq!(ancestors, vec![thread.root.clone()], "root first");
        assert_eq!(
            descendants,
            vec![thread.deep.clone()],
            "and the rest of the chain below"
        );
        assert!(
            parent_visible(&body, &thread.deep),
            "the whole chain is visible to its author"
        );
    }

    {
        let response = context(&app, author, &thread.deep).await;
        common::assert_answer(&response, StatusCode::OK, "the author at the leaf");
        let body = response.json_value();
        let (ancestors, descendants) = context_arms(&body, "the author at the leaf");
        assert_eq!(
            ancestors,
            vec![thread.root.clone(), thread.reply.clone()],
            "the chain above a reply-of-reply arrives oldest first"
        );
        assert!(descendants.is_empty(), "nothing answered the leaf");
    }
}

/// The accepted follower — invisible to neither the root nor the private
/// reply — reads both, and loses the `direct` leg: the reply-of-reply is
/// the shared 404 named directly, and drops out below the private reply
/// without breaking anything above it.
#[tokio::test]
async fn the_follower_reads_the_private_leg_and_loses_the_direct_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;

    let from_root = context(&app, Some(&world.follower.token), &thread.root).await;
    let root_body = from_root.json_value();
    let (_ancestors, descendants) = context_arms(&root_body, "the follower at the root");
    assert_eq!(
        descendants,
        vec![thread.reply.clone()],
        "the private reply is the follower's, the direct one is not"
    );
    assert!(
        parent_visible(&root_body, &thread.reply),
        "whose parent — the named root — came back"
    );

    let reply_read = context(&app, Some(&world.follower.token), &thread.reply).await;
    let reply_body = reply_read.json_value();
    let (reply_ancestors, reply_descendants) =
        context_arms(&reply_body, "the follower at the reply");
    assert_eq!(reply_ancestors, vec![thread.root.clone()], "up to the root");
    assert!(
        reply_descendants.is_empty(),
        "the direct leg below drops for them"
    );

    let hidden = context(&app, Some(&world.follower.token), &thread.deep).await;
    let absent = context(&app, Some(&world.follower.token), UNKNOWN_ID).await;
    assert_eq!(
        hidden.body, absent.body,
        "one body for the hidden leg and a never-stored id"
    );
    common::assert_all_refused(
        &[hidden, absent],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the follower's direct-leg probe",
    );
}

/// The addressed reader of the `direct` reply-of-reply: the `private` link
/// between them and the root is invisible — but the walk passes *through*
/// it. From the root their descendants list carries the direct leg alone
/// with `parent_visible` false, because its own direct parent was dropped;
/// from the leg the ancestors list still reaches the root past the hole.
#[tokio::test]
async fn the_addressed_reader_walks_past_the_hidden_link() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;

    let from_root = context(&app, Some(&world.mentioned.token), &thread.root).await;
    let root_body = from_root.json_value();
    let (_ancestors, descendants) = context_arms(&root_body, "the addressed reader at the root");
    assert_eq!(
        descendants,
        vec![thread.deep.clone()],
        "the private reply drops and the direct leg below it stays"
    );
    assert!(
        !parent_visible(&root_body, &thread.deep),
        "its direct parent was dropped from this answer"
    );

    let deep_read = context(&app, Some(&world.mentioned.token), &thread.deep).await;
    let deep_body = deep_read.json_value();
    let (deep_ancestors, deep_descendants) =
        context_arms(&deep_body, "the addressed reader at the leaf");
    assert_eq!(
        deep_ancestors,
        vec![thread.root.clone()],
        "the walk passes through the hidden link to the root"
    );
    assert!(deep_descendants.is_empty(), "nothing answered the leaf");

    let hidden = context(&app, Some(&world.mentioned.token), &thread.reply).await;
    let absent = context(&app, Some(&world.mentioned.token), UNKNOWN_ID).await;
    assert_eq!(
        hidden.body, absent.body,
        "the hidden middle is the absent id for this reader"
    );
    common::assert_all_refused(
        &[hidden, absent],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the addressed reader's private-leg probe",
    );
}

/// A viewer the thread's private and direct legs are closed to — the
/// stranger, the still-pending requester, the account the author muted
/// (a mute is no visibility rule, but grants nothing either), and the
/// anonymous caller alike — reads the public root's context as two empty
/// arms, and every other leg as the shared 404, byte-identical to a
/// never-stored id.
#[tokio::test]
async fn closed_readers_of_the_thread_reach_only_the_root_arms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;

    for (who, token) in [
        ("the stranger", Some(world.stranger.token.as_str())),
        ("the pending requester", Some(world.pending.token.as_str())),
        ("the muter", Some(world.muter.token.as_str())),
        ("anonymous", None),
    ] {
        let from_root = context(&app, token, &thread.root).await;
        common::assert_answer(&from_root, StatusCode::OK, &format!("{who} at the root"));
        let body = from_root.json_value();
        let (ancestors, descendants) = context_arms(&body, who);
        assert!(ancestors.is_empty(), "{who}: the root stands alone");
        assert!(
            descendants.is_empty(),
            "{who}: the private and direct legs never arrive"
        );
        let mut probes = Vec::new();
        for leg in [&thread.reply, &thread.deep, &thread.dm] {
            probes.push(context(&app, token, leg).await);
        }
        probes.push(context(&app, token, UNKNOWN_ID).await);
        let absent = probes
            .pop()
            .expect("the probe list carries the never-stored id");
        for probe in &probes {
            assert_eq!(
                probe.body, absent.body,
                "{who}: one body for hidden legs and absent ids"
            );
        }
        probes.push(absent);
        common::assert_all_refused(
            &probes,
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("{who} probing the closed legs"),
        );
    }
}

/// The conversation continuation: the `direct` post naming the author's
/// own participation id arrives threaded into the thread (`pleroma
/// .direct_conversation_id`, every participant's own side id) yet answers
/// *no status*, so its context is two empty arms — it is a new root, not a
/// reply — and its listing moved to it for both the author and the
/// addressee, who reads their own distinct side id on it.
#[tokio::test]
async fn a_conversation_post_continues_the_thread_without_replying() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;

    let shown = get(
        &app,
        Some(&world.author.token),
        &format!("{STATUSES}/{}", thread.dm),
    )
    .await;
    common::assert_answer(
        &shown,
        StatusCode::OK,
        "the author reads their conversation post",
    );
    let entity = shown.json_value();
    assert_eq!(
        entity["visibility"],
        json!("direct"),
        "the conversation arm forces the private scope"
    );
    assert_eq!(
        entity["in_reply_to_id"],
        Value::Null,
        "it answers the thread, not a status"
    );
    assert_eq!(
        entity["pleroma"]["direct_conversation_id"],
        json!(&thread.side),
        "the author's read renders the author's side id"
    );
    assert_eq!(
        entity["mentions"][0]["acct"],
        json!("mentioned"),
        "the participation's recipients ride the mentions"
    );

    // The thread's listing moved to the continuation for both sides — and
    // each side names the thread with its OWN participation id.
    let own_row = own_participation(&app, &world.author.token, &thread.dm).await;
    assert_eq!(own_row["id"], json!(&thread.side), "the side keeps its id");
    let their_row = own_participation(&app, &world.mentioned.token, &thread.dm).await;
    assert_ne!(
        their_row["id"],
        json!(&thread.side),
        "the addressee's side of the same thread is a different id"
    );

    // Participants read it as a root of its own: neither walk reaches it.
    for (who, token) in [
        ("the author", Some(world.author.token.as_str())),
        ("the addressee", Some(world.mentioned.token.as_str())),
    ] {
        let arms = context(&app, token, &thread.dm).await.json_value();
        let (ancestors, descendants) = context_arms(&arms, who);
        assert!(
            ancestors.is_empty(),
            "{who}: a conversation continuation hangs from no status"
        );
        assert!(descendants.is_empty(), "{who}: nothing answered it");
    }

    let mut closed = Vec::new();
    for token in [
        Some(world.follower.token.as_str()),
        Some(world.stranger.token.as_str()),
        None,
    ] {
        closed.push(context(&app, token, &thread.dm).await);
    }
    common::assert_all_refused(
        &closed,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the conversation post read by non-participants",
    );
}

/// Rule 2 holds on the context route too: the account the author blocked
/// reads every leg of the thread — the public root included — as the same
/// shared 404, byte-identical to a never-stored id.
#[tokio::test]
async fn the_blocked_viewer_reads_no_context_of_the_thread() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, world) = world(&pool).await;
    let thread = post_thread(&app, &world).await;

    let mut probes = Vec::new();
    for leg in [&thread.root, &thread.reply, &thread.deep, &thread.dm] {
        probes.push(context(&app, Some(world.blocker.token.as_str()), leg).await);
    }
    let absent = context(&app, Some(world.blocker.token.as_str()), UNKNOWN_ID).await;
    for probe in &probes {
        assert_eq!(
            probe.body, absent.body,
            "the thread's own legs answer like a never-stored id"
        );
    }
    probes.push(absent);
    common::assert_all_refused(
        &probes,
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the blocked reader of every thread leg",
    );
}
