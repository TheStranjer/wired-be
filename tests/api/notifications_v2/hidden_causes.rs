//! The grouped reads hide what the recipient refused, like v1: a blocked
//! actor, a blocked domain, a notification-muted actor and a muted thread
//! never show in the listing, the single group, its accounts or the unread
//! count; `with_muted=true` brings the muted ones back and never the blocked.

use axum::http::StatusCode;
use sqlx::PgPool;
use wired_be::models::{
    block, conversation_mute, domain_block,
    mute::{self, MuteOptions},
    status::Visibility,
    user::User,
};

use super::support::{
    LIST, NOT_FOUND, READ, encoded, favourite, fixture, group_keys, list, unread,
};
use crate::common;

/// The seed: alice's token and the ids of the notifications she must see
/// or not, one mention per refused cause plus one plain mention.
#[derive(Debug)]
struct Refused {
    /// Alice's `read:notifications` token.
    token: String,
    /// The blocked actor, carol.
    carol: User,
    /// The notification from carol.
    from_blocked: String,
    /// The notification from the muted actor, dave.
    from_muted: String,
    /// The notification from the actor of the blocked domain.
    from_domain: String,
    /// The plain notification, from frank.
    plain: String,
}

/// Seeds [`Refused`]: carol blocked, dave muted with notifications hidden,
/// an actor of a blocked domain, a muted thread, and frank, who is fine.
async fn seed(pool: &PgPool) -> Refused {
    let (alice, token) = fixture(pool, "alice", READ).await;
    let carol = common::create_local_user(pool, "carol").await;
    let dave = common::create_local_user(pool, "dave").await;
    let erin = common::create_local_user(pool, "erin").await;
    let frank = common::create_local_user(pool, "frank").await;
    let mallory = common::create_remote_user(pool, "mallory", "evil.example").await;
    let thread = common::create_status(pool, &erin, "a muted thread", Visibility::Public).await;
    let _blocked = block::block(pool, &common::test_config().base_url, &alice, &carol)
        .await
        .expect("alice blocks carol");
    let _muted = mute::mute(pool, &alice, &dave, MuteOptions::default())
        .await
        .expect("alice mutes dave");
    domain_block::block_domain(pool, alice.id, "evil.example")
        .await
        .expect("alice blocks the domain");
    conversation_mute::mute(pool, alice.id, thread.conversation_id)
        .await
        .expect("alice mutes the thread");
    let from_blocked = common::create_notification(pool, &alice, &carol, "mention", None).await;
    let from_muted = common::create_notification(pool, &alice, &dave, "mention", None).await;
    let from_domain = common::create_notification(pool, &alice, &mallory, "mention", None).await;
    let _in_thread = favourite(pool, &alice, &erin, &thread).await;
    let plain = common::create_notification(pool, &alice, &frank, "mention", None).await;
    Refused {
        token,
        carol,
        from_blocked: from_blocked.id.to_string(),
        from_muted: from_muted.id.to_string(),
        from_domain: from_domain.id.to_string(),
        plain: plain.id.to_string(),
    }
}

/// `ungrouped-<id>`, the key of an ungrouped notification.
fn ungrouped(id: &str) -> String {
    format!("ungrouped-{id}")
}

/// The listing hides every refused cause by default; `with_muted=true`
/// returns the muted ones (actor and thread) and still not the blocked.
#[tokio::test]
async fn the_listing_hides_blocked_and_muted_causes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seeded = seed(&pool).await;
    let app = common::test_app(pool);

    let default_keys = group_keys(&list(&app, &seeded.token, "").await);
    assert_eq!(
        default_keys,
        vec![ungrouped(&seeded.plain)],
        "only the plain notification is listed"
    );

    let muted_keys = group_keys(&list(&app, &seeded.token, "?with_muted=true").await);
    assert!(
        muted_keys.contains(&ungrouped(&seeded.from_muted)),
        "the muted actor returns: {muted_keys:?}"
    );
    assert!(
        muted_keys.iter().any(|key| key.starts_with("favourite-")),
        "the muted thread returns: {muted_keys:?}"
    );
    assert!(
        muted_keys.contains(&ungrouped(&seeded.plain)),
        "the plain one stays"
    );
    for hidden in [&seeded.from_blocked, &seeded.from_domain] {
        assert!(
            !muted_keys.contains(&ungrouped(hidden)),
            "blocked causes stay hidden: {muted_keys:?}"
        );
    }
    assert_eq!(
        muted_keys.len(),
        3,
        "exactly plain, muted actor and muted thread"
    );
}

/// The unread count leaves the refused causes out, and `with_muted=true`
/// adds back only the muted ones.
#[tokio::test]
async fn the_unread_count_excludes_blocked_and_muted_causes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seeded = seed(&pool).await;
    let app = common::test_app(pool);

    assert_eq!(
        unread(&app, &seeded.token, "").await,
        1,
        "only the plain one"
    );
    assert_eq!(
        unread(&app, &seeded.token, "?with_muted=true").await,
        3,
        "the muted actor and thread count, the blocked do not"
    );
}

/// A blocked actor's group answers 404 and its accounts list omits them, while
/// a plain notification's group still reads.
#[tokio::test]
async fn the_group_routes_hide_the_blocked_actor() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let seeded = seed(&pool).await;
    let app = common::test_app(pool);

    let blocked_key = encoded(&ungrouped(&seeded.from_blocked));
    let group = fetch(&app, &seeded.token, &format!("{LIST}/{blocked_key}")).await;
    common::assert_answer(&group, StatusCode::NOT_FOUND, "the blocked actor's group");
    assert!(
        group.text().contains(NOT_FOUND),
        "the shared not-found body"
    );

    let path = format!("{LIST}/{blocked_key}/accounts");
    let accounts = fetch(&app, &seeded.token, &path).await;
    common::assert_answer(&accounts, StatusCode::OK, "the blocked group's accounts");
    let body = accounts.json_value();
    let ids: Vec<&str> = body
        .as_array()
        .expect("an account list")
        .iter()
        .filter_map(|account| account["id"].as_str())
        .collect();
    assert!(
        !ids.contains(&seeded.carol.id.to_string().as_str()),
        "carol is omitted: {ids:?}"
    );

    let plain_key = encoded(&ungrouped(&seeded.plain));
    let plain = fetch(&app, &seeded.token, &format!("{LIST}/{plain_key}")).await;
    common::assert_answer(&plain, StatusCode::OK, "the plain group still reads");
}

/// `GET path` with `token`.
async fn fetch(app: &axum::Router, token: &str, path: &str) -> common::TestResponse {
    common::TestRequest::get(path).bearer(token).send(app).await
}
