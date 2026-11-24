//! The chat lists over HTTP (WBE-146): v1 and v2 answering `Chat` entities
//! newest activity first under a `Link` header keyed on chat ids, the
//! `pinned` and `with_muted` filters, v1's 30-row cap against v2's larger
//! page, and the `max_id`/`min_id`/`since_id` keyset walks.

use axum::{Router, http::StatusCode};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::TEST_BASE_URL,
    models::{block, chat, mute, user::User},
};

use super::support::{
    CHAT_KEYS, READ_SCOPE, ROOT, V2_ROOT, fixture, get_as, open_chat, parse, send,
};
use crate::common;

/// Alice's three chats: one hour old, two hours old and brand new, in that
/// order, as `opened` is indexed by the assertions below.
async fn three_chats(pool: &PgPool, alice: &User) -> Vec<chat::Chat> {
    let mut opened = Vec::new();
    for (nickname, hours) in [("bob", 1_i64), ("carol", 2), ("dave", 0)] {
        let partner = common::create_local_user(pool, nickname).await;
        let chat = open_chat(pool, alice, &partner).await;
        if hours > 0 {
            backdate_chat(pool, chat.id, hours).await;
        }
        opened.push(chat);
    }
    opened
}

/// The chat ids a `limit=1` `max_id` walk over `ROOT` visits, in the order it
/// visits them: strictly descending, and ending where the list runs out.
async fn walk_chats(app: &Router, token: &str) -> Vec<Uuid> {
    let mut walked = Vec::new();
    let mut cursor: Option<Uuid> = None;
    loop {
        let path = cursor.map_or_else(
            || format!("{ROOT}?limit=1"),
            |oldest| format!("{ROOT}?limit=1&max_id={oldest}"),
        );
        let page = get_as(app, token, &path).await;
        assert_eq!(
            page.status,
            StatusCode::OK,
            "the walk keeps answering: {}",
            page.text()
        );
        let Some(first) = common::ids_of(&page.json_value()).first().cloned() else {
            break;
        };
        let id = parse(&first);
        if let Some(older) = cursor {
            assert!(id < older, "pages walk strictly down the chat ids");
        }
        walked.push(id);
        cursor = Some(id);
    }
    walked
}

/// Moves chat `chat_id`'s `updated_at` `hours` hours into the past, so a
/// list order is fixed no matter how fast the machine is.
async fn backdate_chat(pool: &PgPool, chat_id: Uuid, hours: i64) {
    let _moved: sqlx::postgres::PgQueryResult = sqlx::query(
        "UPDATE chats SET updated_at = now() - make_interval(hours => $2) WHERE id = $1",
    )
    .bind(chat_id)
    .bind(i32::try_from(hours).expect("fixture hours fit"))
    .execute(pool)
    .await
    .expect("the backdate runs");
}

/// `GET /api/v1/pleroma/chats` answers the caller's chats newest activity
/// first — an order no id happens to share — under the `Link` header keyed
/// on the page's first and last chat ids, and the `max_id`/`min_id`/
/// `since_id` cursors walk the chat ids themselves: a `limit=1` walk visits
/// every chat exactly once, descending.
#[tokio::test]
async fn the_v1_list_orders_by_activity_and_pages_by_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ_SCOPE).await;
    let opened = three_chats(&pool, &alice).await;
    let app = common::test_app(pool.clone());

    let response = get_as(&app, &token, ROOT).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the list answers 200: {}",
        response.text()
    );
    let value = response.json_value();
    let ids = common::ids_of(&value);
    assert_eq!(
        ids,
        vec![
            opened[2].id.to_string(),
            opened[0].id.to_string(),
            opened[1].id.to_string()
        ],
        "dave is fresh, then the hour-old bob, then the two-hour-old carol"
    );
    for chat in value.as_array().expect("the list is an array") {
        assert_eq!(
            common::sorted_keys(chat),
            CHAT_KEYS,
            "every entry is the full Chat entity"
        );
    }
    let want = common::expected_link(ROOT, &ids[0], &ids[2]);
    assert_eq!(
        response.header("link"),
        Some(want.as_str()),
        "the cursors key on the page's own chat ids"
    );

    let walked = walk_chats(&app, &token).await;
    assert_eq!(walked.len(), 3, "the limit-one walk visits every chat");
    let mut own: Vec<Uuid> = opened.iter().map(|chat| chat.id).collect();
    let mut seen = walked.clone();
    own.sort_unstable();
    seen.sort_unstable();
    assert_eq!(seen, own, "each exactly once");

    let smallest = walked.last().expect("three visits");
    let newer = get_as(&app, &token, &format!("{ROOT}?since_id={smallest}")).await;
    assert_eq!(
        common::ids_of(&newer.json_value()).len(),
        2,
        "since_id skips the cursor and its past"
    );
    let min = get_as(&app, &token, &format!("{ROOT}?min_id={smallest}")).await;
    assert_eq!(
        common::ids_of(&min.json_value()).len(),
        2,
        "min_id answers the newer chats too"
    );

    let junk = get_as(&app, &token, &format!("{ROOT}?max_id=not-a-uuid")).await;
    assert_eq!(
        junk.status,
        StatusCode::BAD_REQUEST,
        "a malformed cursor is a 400"
    );
    assert_eq!(
        junk.error_field().as_deref(),
        Some("Invalid id"),
        "with the pagination module's wording"
    );
}

/// Both lists take the same filters: `pinned=true`/`false` split the chats
/// (through Pleroma's boolean spellings, and on v2 alike), `with_muted` is
/// accepted and is the switch for muted chats (see below), a
/// non-boolean filter fails validation, and a chatless account gets the bare
/// empty array with no `Link` header.
#[tokio::test]
async fn the_lists_filter_by_pinned_and_tolerate_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ_SCOPE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let with_bob = open_chat(&pool, &alice, &bob).await;
    let with_carol = open_chat(&pool, &alice, &carol).await;
    let _pinned: chat::Chat = chat::set_pinned(&pool, alice.id, with_carol.id, true)
        .await
        .expect("the pin writes")
        .expect("alice owns that chat");
    let app = common::test_app(pool.clone());

    let pinned = get_as(&app, &token, &format!("{ROOT}?pinned=true"))
        .await
        .json_value();
    assert_eq!(
        common::ids_of(&pinned),
        vec![with_carol.id.to_string()],
        "the pinned filter names the one chat"
    );
    let pinned_v2 = get_as(&app, &token, &format!("{V2_ROOT}?pinned=1"))
        .await
        .json_value();
    assert_eq!(
        common::ids_of(&pinned_v2),
        vec![with_carol.id.to_string()],
        "v2 reads the same filter in the form client's spelling"
    );
    let rest = get_as(&app, &token, &format!("{ROOT}?pinned=false"))
        .await
        .json_value();
    assert_eq!(
        common::ids_of(&rest),
        vec![with_bob.id.to_string()],
        "the unpinned half gets the rest"
    );
    let muted = get_as(&app, &token, &format!("{ROOT}?with_muted=true"))
        .await
        .json_value();
    let mut want = vec![with_bob.id.to_string(), with_carol.id.to_string()];
    let mut got = common::ids_of(&muted);
    want.sort();
    got.sort();
    assert_eq!(
        got, want,
        "with_muted lists every chat when nobody is muted"
    );

    let junk = get_as(&app, &token, &format!("{ROOT}?pinned=maybe")).await;
    assert_eq!(
        junk.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a filter that is not a boolean fails: {}",
        junk.text()
    );

    let (_, daves_token) = fixture(&pool, "dave", READ_SCOPE).await;
    let empty = get_as(&app, &daves_token, V2_ROOT).await;
    assert_eq!(
        empty.status,
        StatusCode::OK,
        "a chatless account lists the empty array"
    );
    assert_eq!(empty.json_value(), json!([]), "and nothing else");
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// `GET /api/v2/pleroma/chats` is Pleroma's `index2`: the chat whose last
/// message is newest leads the page (a message bumps it, opening never
/// does), the `Link` header rides along, and the cursors walk chat ids.
#[tokio::test]
async fn the_v2_list_orders_by_the_last_message_and_pages_by_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", READ_SCOPE).await;
    let (bob, token) = fixture(&pool, "bob", READ_SCOPE).await;
    let carol = common::create_local_user(&pool, "carol").await;
    let with_alice = open_chat(&pool, &bob, &alice).await;
    let with_carol = open_chat(&pool, &bob, &carol).await;
    backdate_chat(&pool, with_alice.id, 2).await;
    backdate_chat(&pool, with_carol.id, 1).await;
    // The newest message moves alice's chat back to the front.
    let (_, _, bobs_inbox) = send(&pool, &alice, &bob, "newest").await;
    let app = common::test_app(pool.clone());

    let response = get_as(&app, &token, V2_ROOT).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "v2 answers 200: {}",
        response.text()
    );
    let value = response.json_value();
    let ids = common::ids_of(&value);
    assert_eq!(
        ids,
        vec![with_alice.id.to_string(), with_carol.id.to_string()],
        "the chat whose last message is newest leads the page"
    );
    assert_eq!(
        value[0]["last_message"]["id"],
        bobs_inbox.id.to_string(),
        "and its entity carries that message"
    );
    assert!(
        value[1]["last_message"].is_null(),
        "carol's chat never had a message at all"
    );
    let want = common::expected_link(V2_ROOT, &ids[0], &ids[1]);
    assert_eq!(
        response.header("link"),
        Some(want.as_str()),
        "the Link header keys on the chat ids"
    );

    let older = with_alice.id.min(with_carol.id);
    let newer = if older == with_alice.id {
        with_carol.id
    } else {
        with_alice.id
    };
    let since = get_as(&app, &token, &format!("{V2_ROOT}?since_id={older}"))
        .await
        .json_value();
    assert_eq!(
        common::ids_of(&since),
        vec![newer.to_string()],
        "since_id keeps only the chat with the newer id"
    );
    let tail = get_as(&app, &token, &format!("{V2_ROOT}?max_id={newer}"))
        .await
        .json_value();
    assert_eq!(
        common::ids_of(&tail),
        vec![older.to_string()],
        "and max_id walks back to the older one"
    );
}

/// Both lists default to a page of 20; the legacy one caps at 30 while the
/// v2 page goes on to [`MAX_LIMIT`], and every full page links onward.
#[tokio::test]
async fn the_lists_default_to_twenty_and_v1_caps_at_thirty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ_SCOPE).await;
    for index in 0..32 {
        let partner = common::create_local_user(&pool, &format!("partner{index}")).await;
        let _opened: chat::Chat = open_chat(&pool, &alice, &partner).await;
    }
    let app = common::test_app(pool.clone());

    let legacy = get_as(&app, &token, ROOT).await;
    assert_eq!(
        common::ids_of(&legacy.json_value()).len(),
        20,
        "the v1 default page is 20"
    );
    assert_eq!(
        common::ids_of(&get_as(&app, &token, V2_ROOT).await.json_value()).len(),
        20,
        "and so is v2's"
    );
    let big_legacy = get_as(&app, &token, &format!("{ROOT}?limit=35")).await;
    assert_eq!(
        common::ids_of(&big_legacy.json_value()).len(),
        30,
        "the legacy page stops at its cap of 30"
    );
    assert!(
        big_legacy.header("link").is_some(),
        "a full page links onward"
    );
    let big_v2 = get_as(&app, &token, &format!("{V2_ROOT}?limit=35")).await;
    assert_eq!(
        common::ids_of(&big_v2.json_value()).len(),
        32,
        "v2 lets a page of 35 through — every chat fits under its cap"
    );
}

/// Both lists leave out chats with accounts the caller blocked, whatever
/// `with_muted` says; chats with muted accounts are left out unless
/// `with_muted=true` (an expired mute hides nothing), and an account that
/// blocked the caller keeps its chat in the list.
#[tokio::test]
async fn the_lists_hide_blocked_and_muted_counterparts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ_SCOPE).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let _with_bob = open_chat(&pool, &alice, &bob).await;
    let with_carol = open_chat(&pool, &alice, &carol).await;
    let with_dave = open_chat(&pool, &alice, &dave).await;
    let with_erin = open_chat(&pool, &alice, &erin).await;
    let _blocked = block::block(&pool, TEST_BASE_URL, &alice, &bob)
        .await
        .expect("alice's block is written");
    let _mute = mute::mute(&pool, &alice, &carol, mute::MuteOptions::default())
        .await
        .expect("alice's mute is written");
    let _lapsed: sqlx::postgres::PgQueryResult = sqlx::query(
        "INSERT INTO mutes (id, muter_id, muted_id, expires_at)
         VALUES ($1, $2, $3, now() - interval '1 hour')",
    )
    .bind(Uuid::now_v7())
    .bind(alice.id)
    .bind(dave.id)
    .execute(&pool)
    .await
    .expect("the lapsed mute inserts");
    let _blocked_by = block::block(&pool, TEST_BASE_URL, &erin, &alice)
        .await
        .expect("erin's block is written");
    let app = common::test_app(pool.clone());

    for root in [ROOT, V2_ROOT] {
        let default = get_as(&app, &token, root).await.json_value();
        let mut want = vec![with_dave.id.to_string(), with_erin.id.to_string()];
        let mut got = common::ids_of(&default);
        want.sort();
        got.sort();
        assert_eq!(got, want, "{root}: blocked and live-muted chats are hidden");

        let with_muted = get_as(&app, &token, &format!("{root}?with_muted=true"))
            .await
            .json_value();
        let mut want_all = vec![
            with_carol.id.to_string(),
            with_dave.id.to_string(),
            with_erin.id.to_string(),
        ];
        let mut got_all = common::ids_of(&with_muted);
        want_all.sort();
        got_all.sort();
        assert_eq!(
            got_all, want_all,
            "{root}: with_muted brings back only the muted"
        );
    }
}
