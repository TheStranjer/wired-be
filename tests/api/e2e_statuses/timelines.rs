//! The timelines end to end: the home, public, tag, direct and list
//! timelines read by every viewer, and their scope and credential gates.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{NOT_FOUND, STATUSES, UNKNOWN_ID, entity_id, get, relate, uploads_dir};
use super::thread_support::newest_first;
use super::timeline_support::{
    DIRECT, HOME, LIST, NO_SCOPE, NO_TOKEN, PUBLIC, RESTRICTED, TAG, authorize_request,
    created_list, members_added, page_shows, timelines_matrix,
};
use crate::common;

/// Home, first row: the author's page is their own whole posting — the
/// `private` post, the `direct`s they authored and the conversation
/// continuation all ride it, because the viewer is every row's author —
/// while the follower's page adds the followee's `private` post and the DM
/// that @-names them, and loses the `direct`s that do not; the account the
/// author blocked reads no row of theirs at all.
#[tokio::test]
async fn the_home_timeline_answers_author_follower_and_the_blind_blocker() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;
    let thread = &kinds.thread;

    page_shows(
        &app,
        Some(&world.author.token),
        HOME,
        &kinds.all(),
        "the author",
    )
    .await;

    let for_follower = newest_first(&[
        &kinds.public,
        &kinds.unlisted,
        &kinds.private,
        &kinds.follower_dm,
        &kinds.media,
        &kinds.warning,
        &kinds.poll,
        &kinds.tagpost,
        &kinds.local,
        &kinds.quote,
        &thread.root,
        &thread.reply,
    ]);
    page_shows(
        &app,
        Some(&world.follower.token),
        HOME,
        &for_follower,
        "the follower",
    )
    .await;

    page_shows(
        &app,
        Some(&world.blocker.token),
        HOME,
        &[],
        "the blocked viewer",
    )
    .await;
}

/// Home, second row: the muter's own view. Their follow of the locked
/// author only waits until the author grants it; their mute of the author
/// then silences every one of the author's rows off their home page, and
/// `with_muted=true` restates exactly the audience their account reaches —
/// the granted follow's whole view, `private` included, and only the
/// `direct`s (which they are addressed in nowhere) stay out: those were
/// never the mute's to lift.
#[tokio::test]
async fn the_home_timeline_goes_quiet_for_the_muter_and_returns_with_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;
    let thread = &kinds.thread;

    let author_id = world.author.user.id.to_string();
    let asked = relate(&app, &world.muter.token, &author_id, "follow").await;
    assert_eq!(
        asked["requested"],
        json!(true),
        "the lock files the muter's follow as a request"
    );
    authorize_request(&app, &world.author, &world.muter).await;
    let muting = relate(&app, &world.muter.token, &author_id, "mute").await;
    assert_eq!(muting["muting"], json!(true), "the muter mutes the author");

    page_shows(&app, Some(&world.muter.token), HOME, &[], "the muter").await;

    let restored = newest_first(&[
        &kinds.public,
        &kinds.unlisted,
        &kinds.private,
        &kinds.media,
        &kinds.warning,
        &kinds.poll,
        &kinds.tagpost,
        &kinds.local,
        &kinds.quote,
        &thread.root,
        &thread.reply,
    ]);
    page_shows(
        &app,
        Some(&world.muter.token),
        &format!("{HOME}?with_muted=true"),
        &restored,
        "the muter, reading through their own mute",
    )
    .await;
}

/// The public page keeps only its public rows: the anonymous caller reads
/// the open posts and the thread's public root — never the `unlisted`
/// post, the `private` one, either `direct`, or the `local` mention — every
/// token this instance owns additionally reads the `local` post, the
/// blocked viewer reads none of the author's, and `only_media=true` keeps
/// exactly the post carrying the upload.
#[tokio::test]
async fn the_public_timeline_narrows_the_kinds_to_public_rows_plus_local_for_tokens() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;

    let open = newest_first(&[
        &kinds.public,
        &kinds.media,
        &kinds.warning,
        &kinds.poll,
        &kinds.tagpost,
        &kinds.quote,
        &kinds.thread.root,
    ]);
    page_shows(&app, None, PUBLIC, &open, "the anonymous caller").await;

    let for_token = newest_first(&[
        &kinds.public,
        &kinds.media,
        &kinds.warning,
        &kinds.poll,
        &kinds.tagpost,
        &kinds.local,
        &kinds.quote,
        &kinds.thread.root,
    ]);
    page_shows(
        &app,
        Some(&world.follower.token),
        PUBLIC,
        &for_token,
        "a follower's token",
    )
    .await;
    page_shows(
        &app,
        Some(&world.stranger.token),
        PUBLIC,
        &for_token,
        "a stranger's token",
    )
    .await;
    page_shows(
        &app,
        Some(&world.blocker.token),
        PUBLIC,
        &[],
        "the blocked viewer",
    )
    .await;

    let attached = vec![kinds.media.clone()];
    page_shows(
        &app,
        None,
        &format!("{PUBLIC}?only_media=true"),
        &attached,
        "the anonymous media cut",
    )
    .await;
    page_shows(
        &app,
        Some(&world.follower.token),
        &format!("{PUBLIC}?only_media=true"),
        &attached,
        "a token's media cut",
    )
    .await;
}

/// The documented gate: while `RESTRICT_UNAUTHENTICATED` lists
/// `timelines_federated` and `timelines_local`, every anonymous public
/// read — the plain page and the `local=true` one — is the 403
/// [`RESTRICTED`], while a token still reads the same membership the open
/// instance answered it with.
#[tokio::test]
async fn the_restriction_gate_closes_only_the_anonymous_public_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (_app, world, kinds) = timelines_matrix(&pool, dir.path()).await;

    let gated = common::test_app_with_restricted_areas(
        pool.clone(),
        &["timelines_federated", "timelines_local"],
    );
    for query in ["", "?local=true"] {
        let refused = get(&gated, None, &format!("{PUBLIC}{query}")).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("the gated anonymous `{query}` read"),
        );
        common::assert_error(&refused, RESTRICTED, &format!("the `{query}` refusal"));
    }

    let for_token = newest_first(&[
        &kinds.public,
        &kinds.media,
        &kinds.warning,
        &kinds.poll,
        &kinds.tagpost,
        &kinds.local,
        &kinds.quote,
        &kinds.thread.root,
    ]);
    page_shows(
        &gated,
        Some(&world.follower.token),
        PUBLIC,
        &for_token,
        "a token on the gated instance",
    )
    .await;
}

/// The tag route: the one `#E2ETag` post is the whole page for the author,
/// the follower, a stranger and the anonymous caller alike — the stored
/// lowercase fold answers the path's `E2ETag` spelling, and no other kind
/// of the matrix rides it.
#[tokio::test]
async fn the_tag_timeline_answers_the_one_tagged_post_to_every_named_viewer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;

    let one = vec![kinds.tagpost.clone()];
    for (who, token) in [
        ("the author", Some(world.author.token.as_str())),
        ("the follower", Some(world.follower.token.as_str())),
        ("the stranger", Some(world.stranger.token.as_str())),
        ("the anonymous caller", None),
    ] {
        page_shows(&app, token, &format!("{TAG}/E2ETag"), &one, who).await;
    }
}

/// The direct page is held to its participants: the author reads all four
/// `direct`s they authored, the @-addressed account reads the three naming
/// them (the reply-of-reply and the conversation continuation included),
/// the follower named by the other DM reads exactly that one and none of
/// the `mentioned`'s, and a stranger — no participant of anything — reads
/// the empty page.
#[tokio::test]
async fn the_direct_timeline_stays_inside_its_participants() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;
    let thread = &kinds.thread;

    let authored = newest_first(&[&kinds.direct, &kinds.follower_dm, &thread.deep, &thread.dm]);
    page_shows(
        &app,
        Some(&world.author.token),
        DIRECT,
        &authored,
        "the author",
    )
    .await;

    let addressed = newest_first(&[&kinds.direct, &thread.deep, &thread.dm]);
    page_shows(
        &app,
        Some(&world.mentioned.token),
        DIRECT,
        &addressed,
        "the addressed participant",
    )
    .await;

    let theirs = vec![kinds.follower_dm.clone()];
    page_shows(
        &app,
        Some(&world.follower.token),
        DIRECT,
        &theirs,
        "the follower they addressed",
    )
    .await;

    page_shows(
        &app,
        Some(&world.stranger.token),
        DIRECT,
        &[],
        "the non-participant stranger",
    )
    .await;
}

/// The direct route demands a user token granting `read:statuses`: no
/// token is the 401 [`NO_TOKEN`], a `read:accounts` token the 403
/// [`NO_SCOPE`], and a granted one reads its page.
#[tokio::test]
async fn the_direct_timeline_refuses_the_tokenless_and_the_scopeless() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;

    let anonymous = get(&app, None, DIRECT).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, NO_TOKEN, "no token");

    let noscope = common::create_app_and_token(&pool, &world.stranger.user, "read:accounts").await;
    let unscoped = get(&app, Some(&noscope), DIRECT).await;
    common::assert_answer(
        &unscoped,
        StatusCode::FORBIDDEN,
        "a token without read:statuses",
    );
    common::assert_error(&unscoped, NO_SCOPE, "a token without read:statuses");

    let authored = newest_first(&[
        &kinds.direct,
        &kinds.follower_dm,
        &kinds.thread.deep,
        &kinds.thread.dm,
    ]);
    page_shows(
        &app,
        Some(&world.author.token),
        DIRECT,
        &authored,
        "the author's granted token",
    )
    .await;
}

/// The list page: the author creates a list and adds the author — their own
/// account, listable because the owner is always member-worth of their own
/// list — and the member-watching viewer's page then holds the member's
/// whole posting under the stored `list` replies policy: every reply of the
/// chain answers the member, so none drops.
#[tokio::test]
async fn the_list_timeline_holds_the_member_authors_posting_under_the_stored_policy() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, kinds) = timelines_matrix(&pool, dir.path()).await;

    let list = created_list(&app, &world.author.token, "the matrix circle").await;
    assert_eq!(
        list["replies_policy"],
        json!("list"),
        "the stored default policy"
    );
    let list_id = entity_id(&list);
    let author_id = world.author.user.id.to_string();
    members_added(&app, &world.author.token, &list_id, &author_id).await;

    page_shows(
        &app,
        Some(&world.author.token),
        &format!("{LIST}/{list_id}"),
        &kinds.all(),
        "the member-watching viewer",
    )
    .await;
}

/// A list someone else owns is not the caller's to read: the follower's
/// probe of the author's list id answers the shared 404 [`NOT_FOUND`] —
/// byte-identical to a never-minted list id and to the show route's 404, so
/// the list's existence leaks nowhere, and only the owner reads the page.
#[tokio::test]
async fn another_accounts_list_is_the_shows_shared_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = uploads_dir();
    let (app, world, _kinds) = timelines_matrix(&pool, dir.path()).await;

    let list = created_list(&app, &world.author.token, "someone else's circle").await;
    let route = format!("{LIST}/{}", entity_id(&list));
    page_shows(
        &app,
        Some(&world.author.token),
        &route,
        &[],
        "the owner of an empty-members list",
    )
    .await;

    let hidden = get(&app, Some(&world.follower.token), &route).await;
    let absent = get(
        &app,
        Some(&world.follower.token),
        &format!("{LIST}/{UNKNOWN_ID}"),
    )
    .await;
    let show = get(
        &app,
        Some(&world.follower.token),
        &format!("{STATUSES}/{UNKNOWN_ID}"),
    )
    .await;
    assert_eq!(
        hidden.body, absent.body,
        "one body for the foreign list and a never-minted id"
    );
    assert_eq!(
        hidden.body, show.body,
        "and the show route's 404 is that same shape"
    );
    common::assert_all_refused(
        &[hidden, absent, show],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "the foreign list, the absent list and the absent status",
    );
}
