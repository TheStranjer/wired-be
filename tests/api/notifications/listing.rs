//! `GET /api/v1/notifications`: newest first with a `Link` header, the
//! show-view entity shape, the cursors and limits, the `types[]` filters,
//! the `account_id` narrowing, the `with_muted` exclusions and the hidden
//! dismissed and deactivated rows.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
use wired_be::util::time::mastodon_time;

use super::support::{
    NOT_FOUND, READ, READ_WRITE, V1, assert_empty_object, assert_listed, expected_link, fixture,
    key_set, ladder, list, listed_ids, seed_listing, seed_mutes, ungrouped,
};
use crate::common;

/// `GET /api/v1/notifications` answers the caller's own notifications,
/// newest first, with the `Link` header keyed on the page's first and last
/// row ids.
#[tokio::test]
async fn the_listing_answers_newest_first_with_a_link_header() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_listing(&pool).await;
    let app = common::test_app(pool);

    let response = list(&app, &seed.token, "").await;
    common::assert_answer(&response, StatusCode::OK, "the flat listing");
    assert_eq!(
        listed_ids(&response),
        vec![
            seed.follow.id.to_string(),
            seed.mention.id.to_string(),
            seed.favourite.id.to_string()
        ],
        "newest first, and only alice's rows"
    );
    assert_eq!(
        response.header("link"),
        Some(expected_link(seed.follow.id, seed.favourite.id).as_str()),
        "the cursors walk the page's own row ids"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the entities are JSON"
    );
}

/// The listing's entities carry Pleroma's show-view shape: the row's `id`,
/// `type`, `group_key` and `created_at`, the actor expanded, the `pleroma`
/// flags fresh off the row, and the named status rendered for the rows
/// that have one.
#[tokio::test]
async fn flat_entities_carry_the_show_view_shape() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_listing(&pool).await;
    let app = common::test_app(pool);

    let body = list(&app, &seed.token, "").await.json_value();
    let entities = body.as_array().expect("an array");
    let (follow, favourite) = (&entities[0], &entities[2]);
    assert_eq!(
        key_set(follow),
        [
            "account",
            "created_at",
            "group_key",
            "id",
            "pleroma",
            "type"
        ],
        "the status-less entity's keys"
    );
    assert_eq!(follow["id"], seed.follow.id.to_string(), "id as text");
    assert_eq!(follow["type"], "follow", "the stored type");
    assert_eq!(
        follow["group_key"],
        ungrouped(&seed.follow),
        "the derived key of an ungroupable row"
    );
    assert_eq!(
        follow["created_at"],
        mastodon_time(&seed.follow.inserted_at),
        "the Mastodon time of the row"
    );
    assert_eq!(
        follow["account"]["id"],
        seed.erin_id.to_string(),
        "the actor expanded to a full account"
    );
    assert_eq!(follow["pleroma"]["is_seen"], false, "an unseen row");
    assert_eq!(follow["pleroma"]["is_muted"], false, "alice mutes nobody");
    assert!(
        follow.get("status").is_none(),
        "a status-less row renders none"
    );
    assert_eq!(
        favourite["group_key"],
        format!("favourite-{}", seed.post_id),
        "the stored key of a favourite"
    );
    assert_eq!(
        favourite["status"]["id"],
        seed.post_id.to_string(),
        "the favourited status rendered"
    );
}

/// The listing pages on row ids: `limit` cuts the page, `max_id`,
/// `since_id` and `min_id` walk it — each newest-first — an exhausted walk
/// answers `[]` with no `Link` header, an unusable `limit` falls back to
/// the default, and a cursor that is no id text is the 400.
#[tokio::test]
async fn the_listing_pages_with_its_cursors() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let notes = ladder(&pool, &alice, &bob).await;
    let app = common::test_app(pool);

    let page = list(&app, &token, "?limit=2").await;
    assert_eq!(
        listed_ids(&page),
        vec![notes[4].id.to_string(), notes[3].id.to_string()],
        "the two newest first"
    );
    assert_eq!(
        page.header("link"),
        Some(expected_link(notes[4].id, notes[3].id).as_str()),
        "and the cursors into the elders"
    );

    let older = list(&app, &token, &format!("?limit=2&max_id={}", notes[3].id)).await;
    assert_eq!(
        listed_ids(&older),
        vec![notes[2].id.to_string(), notes[1].id.to_string()],
        "max_id keeps strictly older rows"
    );

    let gap = list(&app, &token, &format!("?since_id={}", notes[3].id)).await;
    assert_eq!(
        listed_ids(&gap),
        vec![notes[4].id.to_string()],
        "since_id keeps the newer row, newest first"
    );

    let forward = list(&app, &token, &format!("?min_id={}", notes[2].id)).await;
    assert_eq!(
        listed_ids(&forward),
        vec![notes[4].id.to_string(), notes[3].id.to_string()],
        "a min_id page reads forward and still answers newest-first"
    );

    let exhausted = list(&app, &token, &format!("?max_id={}", notes[0].id)).await;
    assert_eq!(
        exhausted.json_value(),
        json!([]),
        "older than everything answers nothing"
    );
    assert!(
        exhausted.header("link").is_none(),
        "and an empty page carries no Link header"
    );

    let all = list(&app, &token, "?limit=0").await;
    assert_eq!(
        listed_ids(&all).len(),
        notes.len(),
        "a limit of none asks the default page"
    );

    let garbage = list(&app, &token, "?max_id=not-an-id").await;
    common::assert_answer(&garbage, StatusCode::BAD_REQUEST, "a cursor that is no id");
    common::assert_error(&garbage, "Invalid id", "a cursor that is no id");
}

/// `limit` defaults to twenty and is capped at forty.
#[tokio::test]
async fn the_listing_default_limit_is_twenty_and_the_cap_forty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    for _ in 0..41 {
        let _note = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    }
    let app = common::test_app(pool);

    assert_eq!(
        listed_ids(&list(&app, &token, "").await).len(),
        20,
        "the default page"
    );
    assert_eq!(
        listed_ids(&list(&app, &token, "?limit=100").await).len(),
        40,
        "and the cap an over-large request falls to"
    );
}

/// `types[]` restricts the page and `exclude_types[]` removes from it, the
/// `include_types[]` alias feeds `types[]` when that is absent, and an
/// invalid type string is no error — it simply matches nothing.
#[tokio::test]
async fn the_listing_filters_the_types_through_every_spelling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_listing(&pool).await;
    let app = common::test_app(pool);

    let kept = list(&app, &seed.token, "?types[]=favourite&types[]=mention").await;
    assert_listed(
        &kept,
        &[&seed.mention, &seed.favourite],
        "types[] keeps the named types",
    );
    let lone = list(&app, &seed.token, "?types=favourite").await;
    assert_listed(&lone, &[&seed.favourite], "and the lone `types=` spelling");

    let dropped = list(&app, &seed.token, "?exclude_types[]=favourite").await;
    assert_listed(
        &dropped,
        &[&seed.follow, &seed.mention],
        "exclude_types[] removes the named type",
    );

    let aliased = list(&app, &seed.token, "?include_types[]=follow").await;
    assert_listed(
        &aliased,
        &[&seed.follow],
        "include_types[] is the alias of types[]",
    );
    let both = list(
        &app,
        &seed.token,
        "?types[]=follow&include_types[]=favourite",
    )
    .await;
    assert_listed(
        &both,
        &[&seed.follow],
        "and `types[]` wins when both arrive, Pleroma's put_new rule",
    );

    let nonsense = list(&app, &seed.token, "?types[]=not-a-type").await;
    common::assert_answer(
        &nonsense,
        StatusCode::OK,
        "a type string that matches nothing",
    );
    assert_eq!(
        nonsense.json_value(),
        json!([]),
        "an invalid type is ignored: it names no row"
    );
}

/// `account_id` keeps one actor's rows; an id naming no readable account
/// is the 404, and a `with_muted` that is no boolean the 422.
#[tokio::test]
async fn the_listing_narrows_to_one_account_and_refuses_stranger_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_listing(&pool).await;
    let app = common::test_app(pool);

    let only_carol = list(&app, &seed.token, &format!("?account_id={}", seed.carol_id)).await;
    assert_eq!(
        listed_ids(&only_carol),
        vec![seed.mention.id.to_string()],
        "account_id keeps one actor's rows"
    );
    let only_bob = list(&app, &seed.token, &format!("?account_id={}", seed.bob_id)).await;
    assert_eq!(
        listed_ids(&only_bob),
        vec![seed.favourite.id.to_string()],
        "and asks answer the same way"
    );

    for query in [
        format!("account_id={}", Uuid::now_v7()),
        "account_id=not-an-id".to_owned(),
    ] {
        let missing = common::TestRequest::get(&format!("{V1}?{query}"))
            .bearer(&seed.token)
            .send(&app)
            .await;
        common::assert_answer(&missing, StatusCode::NOT_FOUND, &query);
        common::assert_error(&missing, NOT_FOUND, &query);
    }

    for spelling in ["with_muted=maybe", "with_muted=2"] {
        let asked = list(&app, &seed.token, &format!("?{spelling}")).await;
        common::assert_answer(&asked, StatusCode::UNPROCESSABLE_ENTITY, spelling);
        let message = asked.json_value()["error"]
            .as_str()
            .expect("an error body")
            .to_owned();
        assert!(
            message.starts_with("Invalid parameters"),
            "a `with_muted` that is no boolean is the parameters 422: {message}"
        );
    }
}

/// Left at its default the listing hides notifications whose cause the
/// caller muted — an actor under a mute that hides notifications, and a
/// status in a muted thread — while a plain timeline-only mute hides
/// nothing; blocked actors and blocked domains stay hidden `with_muted` or
/// not, Pleroma's unconditional clauses.
#[tokio::test]
async fn the_listing_hides_muted_causes_until_asked_otherwise() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seed = seed_mutes(&pool).await;
    let app = common::test_app(pool);

    let by_default = list(&app, &seed.token, "").await;
    assert_eq!(
        listed_ids(&by_default),
        vec![seed.plain.id.to_string(), seed.from_timeline.id.to_string()],
        "the muted actor and the muted thread hide; the timeline-only \
         mute, and everything untouched, stay"
    );

    let asked = list(&app, &seed.token, "?with_muted=true").await;
    assert_eq!(
        listed_ids(&asked),
        vec![
            seed.plain.id.to_string(),
            seed.on_muted_thread.id.to_string(),
            seed.from_timeline.id.to_string(),
            seed.from_muted.id.to_string(),
        ],
        "with_muted brings the muted actor and the muted thread back, \
         and Pleroma's blocks stay out"
    );
    let body = asked.json_value();
    let entities = body.as_array().expect("an array");
    let flag = |wanted: &Uuid| {
        entities
            .iter()
            .find(|entity| entity["id"] == wanted.to_string())
            .unwrap_or_else(|| panic!("{wanted} is in the page"))
            .pointer("/pleroma/is_muted")
            .expect("the mute flag")
            .clone()
    };
    assert_eq!(
        flag(&seed.from_muted.id),
        json!(true),
        "the notification-hiding mute marks its entity"
    );
    assert_eq!(
        flag(&seed.from_timeline.id),
        json!(true),
        "and so does the timeline mute, which hid nothing"
    );
    assert_eq!(
        flag(&seed.on_muted_thread.id),
        json!(false),
        "only the thread was muted, not its author"
    );
}

/// Dismissed rows never come back — not even with `with_muted` — and
/// neither do rows of a deactivated actor.
#[tokio::test]
async fn the_listing_hides_dismissed_and_deactivated_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ_WRITE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let doomed = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let _ghosted = common::create_notification(&pool, &alice, &carol, "follow", None).await;
    let live = common::create_notification(&pool, &alice, &erin, "follow", None).await;
    let app = common::test_app(pool.clone());

    let dismissed = common::TestRequest::post(&format!("{V1}/{}/dismiss", doomed.id))
        .bearer(&token)
        .send(&app)
        .await;
    assert_empty_object(&dismissed, "the fixture dismissal");
    let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(carol.id)
        .execute(&pool)
        .await
        .expect("the deactivation lands");

    assert_eq!(
        listed_ids(&list(&app, &token, "").await),
        vec![live.id.to_string()],
        "the dismissed row and the deactivated actor's row are out"
    );
    assert_eq!(
        listed_ids(&list(&app, &token, "?with_muted=true").await),
        vec![live.id.to_string()],
        "and `with_muted` does not uncover them"
    );
}
