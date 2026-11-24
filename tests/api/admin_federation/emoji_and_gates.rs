//! Emoji reload and the token gates on every federation route.

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use wired_be::{
    config::TEST_BASE_URL,
    federation::delivery::KIND_DELIVER,
    models::user::{Flag, NewUser},
    services::emoji_packs::{self, Pack, PackMeta},
};

use super::support::{
    ALL_ADMIN, INSTANCES, MockRelay, PNG_BYTES, RELAY, RELOAD, admin, federation_app, log_entry,
    queued_args,
};
use crate::common;

/// `POST /reload_emoji` re-scans `EMOJI_DIR`: a pack written after
/// startup appears in `custom_emojis` the moment the endpoint runs, and a
/// pack taken off disk loses its rows on the next reload — the sync pass
/// the boot runs, on demand, and the JSON string `"ok"` for an answer.
#[tokio::test]
async fn reload_emoji_resyncs_the_packs_from_disk() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::TempDir::new().unwrap();
    let config = common::test_config_with_emoji_dir(dir.path());
    let state = common::test_app_state(pool.clone(), config);
    let app = wired_be::app(state);
    let (_root, token) = admin(&pool, "root", "admin:write:emoji").await;

    let stored = || async {
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT shortcode, image_url, category FROM custom_emojis ORDER BY shortcode",
        )
        .fetch_all(&pool)
        .await
        .expect("the emojis read")
    };

    let first = common::TestRequest::post(RELOAD)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&first, StatusCode::OK, "reloading an empty root");
    assert_eq!(
        first.json::<String>(),
        "ok",
        "the body is the string \"ok\""
    );
    assert!(stored().await.is_empty(), "nothing on disk, nothing stored");

    let manifest = Pack {
        pack: PackMeta {
            share_files: true,
            ..PackMeta::default()
        },
        files: [("smile", "smile.png"), ("grin", "grin.png")]
            .into_iter()
            .map(|(code, file)| (code.to_owned(), file.to_owned()))
            .collect(),
    };
    emoji_packs::save_pack(dir.path(), "fun", &manifest)
        .await
        .expect("the fixture pack saves");
    for file in manifest.files.values() {
        std::fs::write(dir.path().join("fun").join(file), PNG_BYTES).expect("the image writes");
    }

    let second = common::TestRequest::post(RELOAD)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&second, StatusCode::OK, "reloading after a new pack");
    assert_eq!(second.json::<String>(), "ok", "again the string \"ok\"");
    assert_eq!(
        stored().await,
        vec![
            (
                "grin".to_owned(),
                format!("{TEST_BASE_URL}/emoji/fun/grin.png"),
                Some("fun".to_owned())
            ),
            (
                "smile".to_owned(),
                format!("{TEST_BASE_URL}/emoji/fun/smile.png"),
                Some("fun".to_owned())
            ),
        ],
        "the newly written pack is synced into custom_emojis"
    );
    let (sentence, _) = log_entry(&pool, "reload_emoji").await;
    assert_eq!(sentence, "@root reloaded emoji", "the reload is logged");

    // The other half of the resync: a pack taken off disk loses its rows.
    emoji_packs::delete_pack(dir.path(), "fun")
        .await
        .expect("the fixture pack deletes");
    let third = common::TestRequest::post(RELOAD)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&third, StatusCode::OK, "reloading after the deletion");
    assert!(
        stored().await.is_empty(),
        "and the sweep drops the stale rows"
    );
}

// --- the gate ---------------------------------------------------------------------------------

/// The tokens the gate tests hand the routes: the admin's roots, an
/// ordinary user carrying the same scopes, a moderator, an app-only
/// token, and the two narrow relay scopes.
#[derive(Debug)]
struct Tokens {
    /// The admin's `admin:read admin:write`.
    all: String,
    /// A non-admin's same roots.
    plain: String,
    /// A moderator's same roots.
    staff: String,
    /// A user-unbound token with the same roots.
    app_only: String,
    /// An admin holding only `admin:read:relays`.
    read_relays: String,
    /// An admin holding only `admin:write:relays`.
    write_relays: String,
    /// An admin holding only `admin:write:follows`.
    write_follows: String,
}

/// Mints the [`Tokens`] fixture.
async fn tokens(pool: &PgPool) -> Tokens {
    let (_root, all) = admin(pool, "root", ALL_ADMIN).await;
    let pleb = common::create_local_user(pool, "pleb").await;
    let plain =
        common::create_app_and_token(pool, &pleb, "read write admin:read admin:write").await;
    let mod_user = common::create_local_user_with(pool, "mod", |u| NewUser {
        is_moderator: Flag::new(true),
        ..u
    })
    .await;
    let staff = common::create_app_and_token(pool, &mod_user, ALL_ADMIN).await;
    let app_only = common::create_app_only_token(pool, ALL_ADMIN).await;
    let (_reader, read_relays) = admin(pool, "reader", "admin:read:relays").await;
    let (_writer, write_relays) = admin(pool, "writer", "admin:write:relays").await;
    let (_follower, write_follows) = admin(pool, "follower", "admin:write:follows").await;
    Tokens {
        all,
        plain,
        staff,
        app_only,
        read_relays,
        write_relays,
        write_follows,
    }
}

/// One request to each of the five routes, for the walk of refusals.
fn routes() -> [fn() -> common::TestRequest; 5] {
    [
        || common::TestRequest::get(RELAY),
        || common::TestRequest::post(RELAY).json(&json!({"relay_url": "relay.example"})),
        || common::TestRequest::delete(RELAY).json(&json!({"relay_url": "relay.example"})),
        || common::TestRequest::delete(&format!("{INSTANCES}/peer.example")),
        || common::TestRequest::post(RELOAD),
    ]
}

/// The role half of the gate on every route: no credentials and an
/// unknown token are the 401s, an app-only token, a non-admin and a
/// moderator are the 403s — Pleroma's `UserIsAdminPlug` order, before
/// any scope is consulted.
#[tokio::test]
async fn every_federation_route_demands_an_admin_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let everyone = tokens(&pool).await;

    for route in routes() {
        let unauthenticated = route().send(&app).await;
        common::assert_answer(&unauthenticated, StatusCode::UNAUTHORIZED, "no token");
        common::assert_error(&unauthenticated, "Invalid credentials", "no token");

        let bogus = route().bearer("not-a-real-token").send(&app).await;
        common::assert_answer(&bogus, StatusCode::UNAUTHORIZED, "a bogus token");
        common::assert_error(&bogus, "The access token is invalid", "a bogus token");

        let unbound = route().bearer(&everyone.app_only).send(&app).await;
        common::assert_answer(&unbound, StatusCode::FORBIDDEN, "an app-only token");
        common::assert_error(
            &unbound,
            "This API requires an authenticated user",
            "an app-only token",
        );

        let non_admin = route().bearer(&everyone.plain).send(&app).await;
        common::assert_answer(&non_admin, StatusCode::FORBIDDEN, "a non-admin user");
        common::assert_error(&non_admin, "User is not an admin.", "a non-admin user");

        let moderator = route().bearer(&everyone.staff).send(&app).await;
        common::assert_answer(&moderator, StatusCode::FORBIDDEN, "a moderator");
        common::assert_error(&moderator, "User is not an admin.", "a moderator");
    }
}

/// The scope half of the gate: the wrong class on the right area, and
/// the right class on the wrong area, are 403s naming the class root.
#[tokio::test]
async fn the_federation_scopes_refuse_the_wrong_class_and_area() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let everyone = tokens(&pool).await;

    let write_with_read = common::TestRequest::post(RELAY)
        .bearer(&everyone.read_relays)
        .json(&json!({"relay_url": "nope/nope"}))
        .send(&app)
        .await;
    common::assert_answer(
        &write_with_read,
        StatusCode::FORBIDDEN,
        "a read scope on a write",
    );
    common::assert_error(
        &write_with_read,
        "Insufficient permissions: admin:write.",
        "a read scope on a write",
    );
    let read_with_write = common::TestRequest::get(RELAY)
        .bearer(&everyone.write_relays)
        .send(&app)
        .await;
    common::assert_answer(
        &read_with_write,
        StatusCode::FORBIDDEN,
        "a write scope on a read",
    );
    common::assert_error(
        &read_with_write,
        "Insufficient permissions: admin:read.",
        "a write scope on a read",
    );
    let relays_on_federation = common::TestRequest::delete(&format!("{INSTANCES}/peer.example"))
        .bearer(&everyone.write_relays)
        .send(&app)
        .await;
    common::assert_answer(
        &relays_on_federation,
        StatusCode::FORBIDDEN,
        "the wrong area",
    );
    common::assert_error(
        &relays_on_federation,
        "Insufficient permissions: admin:write.",
        "relay rights do not delete instances",
    );
    let relays_on_emoji = common::TestRequest::post(RELOAD)
        .bearer(&everyone.write_relays)
        .send(&app)
        .await;
    common::assert_answer(
        &relays_on_emoji,
        StatusCode::FORBIDDEN,
        "relays are not emoji",
    );
}

/// The grant half of the gate: each narrow area scope passes its own
/// routes — the relay read lists, the relay write follows for real —
/// and the class roots pass every route of their class.
#[tokio::test]
async fn the_federation_scopes_pass_their_own_routes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let everyone = tokens(&pool).await;
    let mock = MockRelay::start().await;

    let scoped_read = common::TestRequest::get(RELAY)
        .bearer(&everyone.read_relays)
        .send(&app)
        .await;
    common::assert_answer(
        &scoped_read,
        StatusCode::OK,
        "admin:read:relays on the ledger",
    );
    let scoped_follow = common::TestRequest::post(RELAY)
        .bearer(&everyone.write_follows)
        .json(&json!({"relay_url": mock.actor_ap_id()}))
        .send(&app)
        .await;
    common::assert_answer(
        &scoped_follow,
        StatusCode::OK,
        "admin:write:follows on the follow",
    );
    assert_eq!(
        queued_args(&pool, KIND_DELIVER).await.len(),
        1,
        "and the follow really queued its send"
    );
    let roots = common::TestRequest::post(RELOAD)
        .bearer(&everyone.all)
        .send(&app)
        .await;
    common::assert_answer(&roots, StatusCode::OK, "the admin roots on the reload");
}

/// The relay writes sit on `admin:write:follows`, as Pleroma's
/// `relay_controller.ex` has it: the follow and the unfollow answer a
/// follows-only token (a refusal of the relay itself is not a 403), and
/// the relay area's own write token — and the follows token on the other
/// writes — is refused.
#[tokio::test]
async fn the_relay_follow_writes_demand_the_follows_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, _state) = federation_app(&pool);
    let everyone = tokens(&pool).await;
    let body = json!({"relay_url": "nope/nope"});

    let follow = common::TestRequest::post(RELAY)
        .bearer(&everyone.write_follows)
        .json(&body)
        .send(&app)
        .await;
    assert_ne!(
        follow.status,
        StatusCode::FORBIDDEN,
        "admin:write:follows passes the follow gate"
    );
    let unfollow = common::TestRequest::delete(RELAY)
        .bearer(&everyone.write_follows)
        .json(&body)
        .send(&app)
        .await;
    assert_ne!(
        unfollow.status,
        StatusCode::FORBIDDEN,
        "admin:write:follows passes the unfollow gate"
    );

    for request in [
        common::TestRequest::post(RELAY).json(&body),
        common::TestRequest::delete(RELAY).json(&body),
    ] {
        let refused = request.bearer(&everyone.write_relays).send(&app).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            "admin:write:relays no longer authorizes the relay writes",
        );
        common::assert_error(
            &refused,
            "Insufficient permissions: admin:write.",
            "in the write class's words",
        );
    }

    let listing = common::TestRequest::get(RELAY)
        .bearer(&everyone.write_follows)
        .send(&app)
        .await;
    common::assert_answer(
        &listing,
        StatusCode::FORBIDDEN,
        "the follows scope does not list the ledger",
    );
    let reload = common::TestRequest::post(RELOAD)
        .bearer(&everyone.write_follows)
        .send(&app)
        .await;
    common::assert_answer(&reload, StatusCode::FORBIDDEN, "nor does it reload emoji");
}
