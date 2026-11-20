//! The account status pages: the unfiltered page, every filter and the
//! visibility exclusions, read by every viewer.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{ACCOUNTS, get, relate, uploads_dir};
use super::thread_support::{exhibit, newest_first, page_ids, sweep_page};
use crate::common;

/// The sweep's unfiltered page: the author reads all nine rows; the
/// accepted follower adds nothing over `public` but the `private` reply;
/// the stranger, the muter and the anonymous caller reach exactly the
/// six open rows; the blocked account reads the byte-identical empty
/// page of a never-posting account (rules 3-6, again, by id set).
#[tokio::test]
async fn the_unfiltered_page_answers_every_viewer_exactly_what_visibility_allows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;
    let thread = &show.thread;

    let author = newest_first(&[
        &thread.root,
        &thread.reply,
        &thread.deep,
        &thread.dm,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    let follower = newest_first(&[
        &thread.root,
        &thread.reply,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    let open = newest_first(&[
        &thread.root,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    sweep_page(
        &app,
        &world,
        "",
        [author, follower, open.clone(), vec![], open.clone(), open],
    )
    .await;
}

/// `pinned=true` answers the account's one pin — and ignores the
/// pagination keys it was sent: a `limit` of one and a `max_id` older than
/// the pinned status would page it away on the ordinary route. No `Link`
/// header rides a pinned answer, for any viewer; the blocked reader still
/// gets the shared empty page.
#[tokio::test]
async fn pinned_answers_the_one_pin_ignoring_the_pagination_keys() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;

    let query = format!("?pinned=true&limit=1&max_id={}", show.thread.root);
    let author = Some(world.author.token.as_str());
    let page = get(
        &app,
        author,
        &format!("{ACCOUNTS}/{}/statuses{query}", world.author.user.id),
    )
    .await;
    common::assert_answer(&page, StatusCode::OK, "the author's pinned page");
    assert_eq!(
        page_ids(&page),
        vec![show.extra.clone()],
        "the key that pages the ordinary route away is ignored"
    );
    assert_eq!(
        page.header("link"),
        None,
        "a pinned answer sends no Link header"
    );
    let one = vec![show.extra.clone()];
    sweep_page(
        &app,
        &world,
        &query,
        [
            one.clone(),
            one.clone(),
            one.clone(),
            vec![],
            one.clone(),
            one,
        ],
    )
    .await;
}

/// `only_media=true` keeps exactly the one status carrying an attachment —
/// the warning post — for every viewer that reaches it at all.
#[tokio::test]
async fn only_media_keeps_the_attached_post_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;

    let one = vec![show.media.clone()];
    sweep_page(
        &app,
        &world,
        "?only_media=true",
        [
            one.clone(),
            one.clone(),
            one.clone(),
            vec![],
            one.clone(),
            one,
        ],
    )
    .await;
}

/// `exclude_replies=true` drops the whole reply chain — the private reply
/// and the direct reply-of-reply — while the conversation continuation
/// stays: it threads by conversation id and answers no status, so it is no
/// reply. The boost, the media post, the tag post, the pin, the quote and
/// the root are all roots.
#[tokio::test]
async fn exclude_replies_drops_the_chain_but_not_the_conversation_leg() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;
    let thread = &show.thread;

    let author = newest_first(&[
        &thread.root,
        &thread.dm,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    let open = newest_first(&[
        &thread.root,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    sweep_page(
        &app,
        &world,
        "?exclude_replies=true",
        [
            author,
            open.clone(),
            open.clone(),
            vec![],
            open.clone(),
            open,
        ],
    )
    .await;
}

/// `exclude_reblogs=true` takes the author's boost out and nothing else:
/// the page loses exactly the reblog row's id, original untouched.
#[tokio::test]
async fn exclude_reblogs_drops_the_boost() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;
    let thread = &show.thread;

    let author = newest_first(&[
        &thread.root,
        &thread.reply,
        &thread.deep,
        &thread.dm,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
    ]);
    let follower = newest_first(&[
        &thread.root,
        &thread.reply,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
    ]);
    let open = newest_first(&[
        &thread.root,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
    ]);
    sweep_page(
        &app,
        &world,
        "?exclude_reblogs=true",
        [author, follower, open.clone(), vec![], open.clone(), open],
    )
    .await;
}

/// `with_muted=true` brings back exactly the rows the *viewer's own* mute
/// of the author silenced: the muter's page empties first, then fills
/// again — the author's own read of their page never needed it, and the
/// blocked reader's blindness is not the viewer's silence to lift.
#[tokio::test]
async fn with_muted_returns_the_viewers_own_exclusions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;
    let thread = &show.thread;

    let author_full = newest_first(&[
        &thread.root,
        &thread.reply,
        &thread.deep,
        &thread.dm,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    let follower = newest_first(&[
        &thread.root,
        &thread.reply,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    let open = newest_first(&[
        &thread.root,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);

    // The muter mutes the author: the author's whole page goes silent.
    let account = world.author.user.id.to_string();
    let wrote = relate(&app, &world.muter.token, &account, "mute").await;
    assert_eq!(
        wrote["muting"],
        json!(true),
        "the muter mutes the author in turn"
    );
    sweep_page(
        &app,
        &world,
        "",
        [
            author_full.clone(),
            follower.clone(),
            open.clone(),
            vec![],
            vec![],
            open.clone(),
        ],
    )
    .await;

    // `with_muted=true` restates exactly what the mute silenced.
    sweep_page(
        &app,
        &world,
        "?with_muted=true",
        [
            author_full,
            follower,
            open.clone(),
            vec![],
            open.clone(),
            open,
        ],
    )
    .await;
}

/// `tagged=E2ETag` keeps the one hashtag post for every viewer that
/// reaches it; the folded name answers either spelling, and the blockee's
/// page stays the shared empty one.
#[tokio::test]
async fn tagged_keeps_the_hashtagged_post() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;

    let one = vec![show.tagpost.clone()];
    sweep_page(
        &app,
        &world,
        "?tagged=E2ETag",
        [
            one.clone(),
            one.clone(),
            one.clone(),
            vec![],
            one.clone(),
            one,
        ],
    )
    .await;
}

/// `exclude_visibilities[]`: naming `private` takes the follower-visible
/// reply out of the author's and the follower's pages alike; naming
/// `direct` takes both DM legs out of the author's page and changes
/// nothing for readers who never reached them.
#[tokio::test]
async fn exclude_visibilities_drops_named_scopes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, show) = exhibit(&pool, dir.path()).await;
    let thread = &show.thread;

    let open = newest_first(&[
        &thread.root,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);

    let no_private = newest_first(&[
        &thread.root,
        &thread.deep,
        &thread.dm,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    sweep_page(
        &app,
        &world,
        "?exclude_visibilities[]=private",
        [
            no_private,
            open.clone(),
            open.clone(),
            vec![],
            open.clone(),
            open.clone(),
        ],
    )
    .await;

    let no_direct = newest_first(&[
        &thread.root,
        &thread.reply,
        &show.quote,
        &show.media,
        &show.tagpost,
        &show.extra,
        &show.reblog,
    ]);
    sweep_page(
        &app,
        &world,
        "?exclude_visibilities[]=direct",
        [
            no_direct.clone(),
            no_direct,
            open.clone(),
            vec![],
            open.clone(),
            open,
        ],
    )
    .await;
}

// ---------------------------------------------------------------------------
// Part 3: every posted kind across all five timelines of
// `src/api/timelines.rs`, per viewer, by id set.
// ---------------------------------------------------------------------------
