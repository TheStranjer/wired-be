//! The actor surface over HTTP: the JSON-LD schema document served at
//! `GET /schemas/litepub-0.1.jsonld` and the actor documents served at
//! `GET /users/{nickname}` — the whole Pleroma `user.json` document of a
//! provisioned account, its stable key set, the locked, bot and media
//! variations, the `Accept` negotiation between `ActivityPub` clients,
//! browsers and everyone else, and the 404 and 410 answers for accounts
//! this route does not serve.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::{
    models::{
        custom_emoji::{self, NewCustomEmoji},
        user::{Flag, NewUser},
    },
    util::time::mastodon_time,
};

use super::support::{
    ACTIVITY_JSON, ACTOR_KEYS, BASE, PUBLIC_PEM, fetch_activity_json, fetch_actor, keys_with,
};
use crate::common;

/// `GET /schemas/litepub-0.1.jsonld` — the JSON-LD context document the
/// `@context` of the `Note`/`Question` and activity documents names. It is
/// one fixed document served to anyone, no `Accept` negotiation and no
/// signature, holding the vocabulary terms those documents actually use.
#[tokio::test]
async fn the_litepub_schema_document_is_served() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/schemas/litepub-0.1.jsonld")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the schema is served");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "as JSON, like Pleroma's static file"
    );
    let document = response.json_value();
    let context = document
        .get("@context")
        .and_then(Value::as_object)
        .expect("the document is a term map under @context");
    assert_eq!(
        context.get("conversation"),
        Some(&json!({"@id": "ostatus:conversation", "@type": "@id"})),
        "the ostatus conversation reference every note carries"
    );
    assert_eq!(
        context.get("EmojiReact"),
        Some(&json!("litepub:EmojiReact")),
        "the litepub terms"
    );
    assert_eq!(
        context.get("capabilities"),
        Some(&json!("litepub:capabilities")),
        "…all of them…"
    );
    assert_eq!(
        context.get("quoteUrl"),
        Some(&json!("as:quoteUrl")),
        "…the quote pair…"
    );
    assert_eq!(
        context.get("quoteUri"),
        Some(&json!("fedibird:quoteUri")),
        "…"
    );
    assert_eq!(
        context.get("votersCount"),
        Some(&json!("toot:votersCount")),
        "…the poll count…"
    );
    assert_eq!(
        context.get("sensitive"),
        Some(&json!("as:sensitive")),
        "…the content-warning flag…"
    );
    assert_eq!(
        context.get("Hashtag"),
        Some(&json!("as:Hashtag")),
        "…and the tag type…"
    );
    for (term, definition) in [
        ("blurhash", json!("toot:blurhash")),
        (
            "focalPoint",
            json!({"@container": "@list", "@id": "toot:focalPoint"}),
        ),
        ("formerType", json!("as:formerType")),
        (
            "_misskey_quote",
            json!("https://misskey-hub.net/ns#_misskey_quote"),
        ),
        ("ChatMessage", json!("litepub:ChatMessage")),
        ("discoverable", json!("toot:discoverable")),
        ("PropertyValue", json!("schema:PropertyValue")),
    ] {
        assert_eq!(
            context.get(term),
            Some(&definition),
            "…and the actor and media terms, the one map for every document: {term}"
        );
    }
}

/// The whole document a provisioned local account is served as: every key
/// and value of Pleroma's `user.json`, end to end over HTTP, with the
/// `@context` naming the one served schema instead of inlining terms.
#[tokio::test]
async fn a_local_user_is_served_as_its_actor_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        name: "Alice".to_owned(),
        bio: "<p>hello</p>".to_owned(),
        raw_bio: "hello".to_owned(),
        discoverable: Flag::new(true),
        public_key: Some(PUBLIC_PEM.to_owned()),
        fields: json!([
            {"name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>"}
        ]),
        ..new
    })
    .await;

    let document = fetch_actor(&pool, &alice).await;
    assert_eq!(
        document,
        json!({
            "@context": [
                "https://www.w3.org/ns/activitystreams",
                "https://w3id.org/security/v1",
                format!("{BASE}/schemas/litepub-0.1.jsonld"),
                {"@language": "und"},
            ],
            "id": format!("{BASE}/users/alice"),
            "type": "Person",
            "preferredUsername": "alice",
            "name": "Alice",
            "summary": "<p>hello</p>",
            "url": format!("{BASE}/users/alice"),
            "inbox": format!("{BASE}/users/alice/inbox"),
            "outbox": format!("{BASE}/users/alice/outbox"),
            "followers": format!("{BASE}/users/alice/followers"),
            "following": format!("{BASE}/users/alice/following"),
            "featured": format!("{BASE}/users/alice/collections/featured"),
            "endpoints": {
                "sharedInbox": format!("{BASE}/inbox"),
                "oauthAuthorizationEndpoint": format!("{BASE}/oauth/authorize"),
                "oauthTokenEndpoint": format!("{BASE}/oauth/token"),
                "oauthRegistrationEndpoint": format!("{BASE}/api/v1/apps"),
            },
            "publicKey": {
                "id": format!("{BASE}/users/alice#main-key"),
                "owner": format!("{BASE}/users/alice"),
                "publicKeyPem": PUBLIC_PEM,
            },
            "manuallyApprovesFollowers": false,
            "discoverable": true,
            "attachment": [{
                "type": "PropertyValue",
                "name": "site",
                "value": "<a href=\"https://alice.example\">alice.example</a>",
            }],
            "published": mastodon_time(&alice.inserted_at),
            "tag": [],
            "alsoKnownAs": [],
            "capabilities": {"acceptsChatMessages": true},
        }),
        "the actor document of a provisioned local user"
    );
}

/// A user with nothing filled in still renders every documented key — and
/// neither `icon` nor `image`.
#[tokio::test]
async fn a_bare_user_has_the_documented_key_set() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let document = fetch_actor(&pool, &alice).await;
    assert_eq!(common::sorted_keys(&document), ACTOR_KEYS, "exact key set");
    assert_eq!(
        document["endpoints"],
        json!({
            "sharedInbox": format!("{BASE}/inbox"),
            "oauthAuthorizationEndpoint": format!("{BASE}/oauth/authorize"),
            "oauthTokenEndpoint": format!("{BASE}/oauth/token"),
            "oauthRegistrationEndpoint": format!("{BASE}/api/v1/apps"),
        }),
        "the endpoints are the instance's own"
    );
    assert_eq!(
        document["publicKey"],
        json!({
            "id": format!("{BASE}/users/alice#main-key"),
            "owner": format!("{BASE}/users/alice"),
            "publicKeyPem": Value::Null,
        }),
        "a row without key material keeps the key id and owner"
    );
    assert_eq!(
        document["published"],
        mastodon_time(&alice.inserted_at),
        "published is the insert time in the wire format"
    );
    assert_eq!(
        document["capabilities"],
        json!({"acceptsChatMessages": true}),
        "the litepub capabilities object"
    );
}

/// `locked` is what `manuallyApprovesFollowers` means; it changes no key.
#[tokio::test]
async fn a_locked_user_approves_followers_manually() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        locked: Flag::new(true),
        ..new
    })
    .await;
    let document = fetch_actor(&pool, &alice).await;
    assert_eq!(
        document["manuallyApprovesFollowers"], true,
        "the stored flag is the ActivityPub field"
    );
    assert_eq!(
        common::sorted_keys(&document),
        ACTOR_KEYS,
        "the key set is stable"
    );
}

/// A bot federates as its `actor_type`: Pleroma emits no `bot` key, so the
/// type is what says it.
#[tokio::test]
async fn a_bot_is_federated_by_its_actor_type() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        actor_type: "Service".to_owned(),
        bot: Flag::new(true),
        ..new
    })
    .await;
    let document = fetch_actor(&pool, &alice).await;
    assert_eq!(document["type"], "Service", "the stored actor type");
    assert_eq!(document["preferredUsername"], "alice", "still alice");
    assert!(
        document.get("bot").is_none(),
        "the bot flag itself is not federated"
    );
    assert_eq!(
        common::sorted_keys(&document),
        ACTOR_KEYS,
        "the key set is stable"
    );
}

/// An avatar becomes `icon` and a banner `image`, each an `Image` object;
/// an account with only an avatar gets only `icon`.
#[tokio::test]
async fn an_avatar_user_carries_icon_and_image() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        avatar_url: Some(format!("{BASE}/avatar.png")),
        banner_url: Some(format!("{BASE}/banner.png")),
        ..new
    })
    .await;
    let document = fetch_actor(&pool, &alice).await;
    assert_eq!(
        document["icon"],
        json!({"type": "Image", "url": format!("{BASE}/avatar.png")}),
        "the avatar is an Image"
    );
    assert_eq!(
        document["image"],
        json!({"type": "Image", "url": format!("{BASE}/banner.png")}),
        "the banner is an Image"
    );
    assert_eq!(
        common::sorted_keys(&document),
        keys_with(&["icon", "image"]),
        "media adds its keys"
    );

    let carol = common::create_local_user_with(&pool, "carol", |new| NewUser {
        avatar_url: Some(format!("{BASE}/avatar.png")),
        ..new
    })
    .await;
    let carol_document = fetch_actor(&pool, &carol).await;
    assert_eq!(
        common::sorted_keys(&carol_document),
        keys_with(&["icon"]),
        "an avatar alone adds only icon"
    );
}

/// `application/ld+json` with the `ActivityStreams` profile is an
/// `ActivityPub` client, and so is an `Accept` that names it among others.
#[tokio::test]
async fn ld_json_clients_get_the_actor_too() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    for accept in [
        "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"",
        "application/ld+json",
        "text/html, application/activity+json;q=0.9",
    ] {
        let response = common::TestRequest::get(&format!("/users/{}", alice.nickname))
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{accept} names an ActivityPub media type"
        );
        assert_eq!(
            response.header("content-type"),
            Some(ACTIVITY_JSON),
            "{accept} gets the ActivityPub media type"
        );
        assert_eq!(
            response.header("vary"),
            Some("Accept"),
            "{accept} sees the negotiation"
        );
        assert_eq!(
            response.json_value()["id"],
            format!("{BASE}/users/alice"),
            "{accept} gets alice's document"
        );
    }
}

/// A browser is not an `ActivityPub` client: it gets Pleroma's redirect to
/// the frontend profile page, 302 with `Location: {BASE_URL}/{nickname}`.
/// A client that asks for neither `ActivityPub` nor HTML still gets the
/// router-level 404 (this backend is API-only and serves no HTML, decision
/// 1).
#[tokio::test]
async fn a_browser_is_redirected_to_the_frontend() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    for accept in ["text/html", "text/html,application/xhtml+xml"] {
        let response = common::TestRequest::get(&format!("/users/{}", alice.nickname))
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::FOUND,
            "{accept} is redirected, not served a document"
        );
        assert_eq!(
            response.header("location"),
            Some("https://test.example/alice"),
            "{accept} is sent to the frontend profile URL"
        );
        assert_eq!(
            response.header("vary"),
            Some("Accept"),
            "the redirect for {accept} says it varies on Accept"
        );
    }
    for accept in ["*/*", "application/json", "application/activity+xml"] {
        let response = common::TestRequest::get(&format!("/users/{}", alice.nickname))
            .header("accept", accept)
            .send(&app)
            .await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{accept} is neither an ActivityPub client nor a browser"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the 404 for {accept} is JSON"
        );
        assert_eq!(
            response.header("vary"),
            Some("Accept"),
            "the 404 for {accept} says it varies on Accept"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Not found"}),
            "the fallback wording for {accept}"
        );
    }
}

/// An unknown nickname and a remote mirror are both 404s: this route serves
/// this instance's actors only (decision 7).
#[tokio::test]
async fn unknown_and_remote_users_are_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool.clone());
    for path in ["/users/nobody", &format!("/users/{}", bob.nickname)] {
        let response = fetch_activity_json(&app, path).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{path} is not local"
        );
        assert_eq!(
            response.header("vary"),
            Some("Accept"),
            "{path} says it varies on Accept"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Not found"}),
            "{path} keeps the fallback body"
        );
    }
}

/// A deactivated local account is 410 Gone, so a remote instance stops
/// refetching it; a browser still gets the content-negotiation 404.
#[tokio::test]
async fn a_deactivated_user_is_gone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        is_active: Flag::new(false),
        ..new
    })
    .await;
    let app = common::test_app(pool.clone());
    let response = fetch_activity_json(&app, &format!("/users/{}", alice.nickname)).await;
    assert_eq!(
        response.status,
        StatusCode::GONE,
        "a deactivated local account is gone"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the 410 is JSON like every error"
    );
    assert_eq!(
        response.header("vary"),
        Some("Accept"),
        "the 410 says it varies on Accept"
    );
    assert_eq!(
        response.json_value(),
        json!({"error": "Gone"}),
        "the 410 body follows decision 8"
    );

    let browser = common::TestRequest::get(&format!("/users/{}", alice.nickname))
        .header("accept", "text/html")
        .send(&app)
        .await;
    assert_eq!(
        browser.status,
        StatusCode::FOUND,
        "a browser is redirected to the frontend, gone or not — the \
         frontend decides what to show there"
    );
    assert_eq!(
        browser.header("location"),
        Some("https://test.example/alice"),
        "the redirect is the ordinary frontend profile URL"
    );
}

/// The Atom/RSS feed addresses are not served: the account feed in its three
/// spellings and the tag feed fall through to the app-wide JSON 404 (marked
/// `x-wired-fallback: 1`), whatever `Accept` says, for a user that exists.
#[tokio::test]
async fn the_feed_addresses_fall_through_to_the_fallback() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    for path in [
        "/users/alice/feed.atom",
        "/users/alice/feed.rss",
        "/users/alice/feed",
        "/tags/rust.rss",
    ] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{path} is no route of an API-only backend"
        );
        assert_eq!(
            response.header("x-wired-fallback"),
            Some("1"),
            "{path} is answered by the router fallback, not a handler"
        );
    }
}

/// With the feed subpaths gone the actor address itself still negotiates:
/// `Accept: application/activity+json` on `/users/alice` is the actor
/// document.
#[tokio::test]
async fn the_actor_address_still_serves_activity_json() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());
    let response = common::TestRequest::get("/users/alice")
        .header("accept", "application/activity+json")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the actor is served");
    assert_eq!(
        response.header("content-type"),
        Some(ACTIVITY_JSON),
        "as ActivityPub JSON"
    );
    assert_eq!(
        response.json_value()["id"],
        format!("{BASE}/users/alice"),
        "alice's own document"
    );
}

/// A frontend on its own origin: a browser on `/users/alice` is sent to
/// `FRONTEND_BASE_URL`, not `BASE_URL`, while an `ActivityPub` client still
/// gets the JSON document, and both answers vary on `Accept`.
#[tokio::test]
async fn the_actor_redirect_follows_the_frontend_origin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _alice = common::create_local_user(&pool, "alice").await;
    let mut config = common::test_config();
    config.frontend_base_url = "https://fe.example".to_owned();
    let app = common::test_app_with_config(pool, config);

    let browser = common::TestRequest::get("/users/alice")
        .header("accept", "text/html")
        .send(&app)
        .await;
    assert_eq!(browser.status, StatusCode::FOUND, "a browser redirects");
    assert_eq!(
        browser.header("location"),
        Some("https://fe.example/alice"),
        "to the frontend origin, not the base URL"
    );
    assert_eq!(
        browser.header("vary"),
        Some("Accept"),
        "the redirect varies on Accept"
    );

    let federating = fetch_activity_json(&app, "/users/alice").await;
    assert_eq!(
        federating.status,
        StatusCode::OK,
        "an ActivityPub client still gets the document"
    );
    assert_eq!(
        federating.header("vary"),
        Some("Accept"),
        "the document varies on Accept"
    );
    assert_eq!(
        federating.json_value()["id"],
        format!("{BASE}/users/alice"),
        "whose id stays on the base URL"
    );
}

/// The profile's custom emoji ride the served actor's `tag` as `Emoji`
/// tags: one per local emoji used in the name, bio or a field; a shortcode
/// with no emoji row adds none.
#[tokio::test]
async fn profile_custom_emoji_are_tagged_on_the_served_actor() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let url = format!("{BASE}/emoji/wiredcat.png");
    let _stored = custom_emoji::upsert(&pool, &NewCustomEmoji::local("wiredcat", &url))
        .await
        .expect("the emoji stores");
    let alice = common::create_local_user_with(&pool, "alice", |new| NewUser {
        name: "Alice :wiredcat: A".to_owned(),
        raw_bio: "bio :wiredcat: :nosuch:".to_owned(),
        fields: json!([{"name": "emoji :wiredcat:", "value": ":wiredcat:"}]),
        ..new
    })
    .await;

    let document = fetch_actor(&pool, &alice).await;
    assert_eq!(
        document["tag"],
        json!([{
            "type": "Emoji",
            "id": url,
            "name": ":wiredcat:",
            "icon": {"type": "Image", "url": url},
        }]),
        "one tag for the known emoji, however often it is used"
    );
}
