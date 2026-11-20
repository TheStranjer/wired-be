//! `pleroma_settings_store` merging, the `actor_type` / `bot` pair, the
//! response reflecting every new value, and the profile-federation hook that
//! queues the debounced actor `Update`.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::support::{
    BASE, authed_app, ok_body, part, patch_form, patch_json, patch_multipart, pleroma_of,
    source_of, stored,
};
use crate::common;

/// `pleroma_settings_store` merges into the stored blob key by key: a
/// submitted key wins, an untouched key stays, a nested object is replaced
/// whole, and the owner's `pleroma.settings_store` answers the result.
#[tokio::test]
async fn the_settings_store_merges_key_by_key() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.settings_store = json!({"theme": "dark", "locale": "pt", "fe": {"a": 1, "b": 2}});
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let merged = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"pleroma_settings_store": {"locale": "de", "custom": {"nested": true}}}),
        )
        .await,
    );
    assert_eq!(
        pleroma_of(&merged)["settings_store"],
        json!({"theme": "dark", "locale": "de", "fe": {"a": 1, "b": 2}, "custom": {"nested": true}}),
        "the submitted keys win, the stored ones stay"
    );

    let replaced = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"pleroma_settings_store": {"fe": {"c": 3}}}),
        )
        .await,
    );
    assert_eq!(
        pleroma_of(&replaced)["settings_store"],
        json!({"theme": "dark", "locale": "de", "custom": {"nested": true}, "fe": {"c": 3}}),
        "a nested object replaces the stored one: the merge is shallow, key by key"
    );

    let nulled = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"pleroma_settings_store": {"theme": null}}),
        )
        .await,
    );
    assert_eq!(
        pleroma_of(&nulled)["settings_store"]["theme"],
        Value::Null,
        "unlike the settings endpoint's deep merge, a null is a stored null"
    );

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.settings_store,
        json!({"custom": {"nested": true}, "fe": {"c": 3}, "locale": "de", "theme": null}),
        "the row is what the answer stated"
    );
    assert_eq!(row.name, "", "the settings write left the profile alone");
}

/// A form body carries `pleroma_settings_store` as JSON *text*; so does a
/// multipart text part. Both parse into the object they name and merge like
/// the JSON spelling.
#[tokio::test]
async fn a_form_settings_store_arrives_as_json_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let patch = r#"{"theme": "light", "fe": {"v": 1}}"#;

    let via_form = ok_body(&patch_form(&app, &token, &[("pleroma_settings_store", patch)]).await);
    assert_eq!(
        pleroma_of(&via_form)["settings_store"],
        json!({"theme": "light", "fe": {"v": 1}}),
        "the form's JSON text parses"
    );

    let via_multipart = ok_body(
        &patch_multipart(
            &app,
            &token,
            &[
                part("pleroma_settings_store", "{\"bookmarks\": false}"),
                part("display_name", "Ada"),
            ],
        )
        .await,
    );
    assert_eq!(
        pleroma_of(&via_multipart)["settings_store"],
        json!({"theme": "light", "fe": {"v": 1}, "bookmarks": false}),
        "a multipart text part merges over what the form left"
    );
    assert_eq!(
        via_multipart["display_name"], "Ada",
        "and the text field rode along"
    );

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.settings_store,
        json!({"theme": "light", "fe": {"v": 1}, "bookmarks": false}),
        "stored as answered"
    );
}

/// A settings store that is no object, and a merged blob past 1 MiB, are both
/// the 422 with nothing written.
#[tokio::test]
async fn a_bad_or_oversize_settings_store_is_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.settings_store = json!({"theme": "dark"});
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    for patch in [json!([1, 2]), json!(5), json!("plain text")] {
        let response = patch_json(&app, &token, &json!({"pleroma_settings_store": patch})).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{patch}` has no keys to merge"
        );
        assert_eq!(
            response.text(),
            r#"{"error":"pleroma_settings_store: expected a JSON object"}"#,
            "the shape 422 names the parameter"
        );
    }

    let blob = "x".repeat(1024 * 1024 + 1);
    let too_large = patch_json(
        &app,
        &token,
        &json!({"pleroma_settings_store": {"blob": blob}}),
    )
    .await;
    assert_eq!(
        too_large.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a merged store past one mebibyte is refused"
    );
    assert_eq!(
        too_large.text(),
        r#"{"error":"settings_store: too large"}"#,
        "the same wording the settings endpoint uses"
    );

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.settings_store,
        json!({"theme": "dark"}),
        "and every refusal left the stored blob alone"
    );
}

/// `actor_type` and `bot` are one pair: the stated kind decides the flag, the
/// flag picks the kind when no kind is stated, and a kind outside `Person`
/// and `Service` is the 422.
#[tokio::test]
async fn actor_type_and_bot_are_written_as_one_pair() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let service = ok_body(&patch_json(&app, &token, &json!({"actor_type": "Service"})).await);
    assert_eq!(
        source_of(&service)["pleroma"]["actor_type"],
        "Service",
        "the stated kind is the stored kind"
    );
    assert_eq!(service["bot"], true, "and a Service is a bot");

    let human = ok_body(&patch_form(&app, &token, &[("actor_type", "Person")]).await);
    assert_eq!(
        source_of(&human)["pleroma"]["actor_type"],
        "Person",
        "a form body states it the same way"
    );
    assert_eq!(human["bot"], false, "and a Person is not a bot");

    let automated = ok_body(&patch_json(&app, &token, &json!({"bot": true})).await);
    assert_eq!(
        source_of(&automated)["pleroma"]["actor_type"],
        "Service",
        "the bot flag alone picks the actor kind, as Pleroma does"
    );
    assert_eq!(automated["bot"], true, "and stays set");

    let both = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"bot": false, "actor_type": "Service"}),
        )
        .await,
    );
    assert_eq!(
        source_of(&both)["pleroma"]["actor_type"],
        "Service",
        "an explicit kind wins over the flag"
    );
    assert_eq!(
        both["bot"], true,
        "and the answered flag reads back from the kind"
    );

    for kind in ["Group", "Application", "person", ""] {
        let response = patch_json(&app, &token, &json!({"actor_type": kind})).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{kind}` is no kind this endpoint sets"
        );
        assert_eq!(
            response.text(),
            r#"{"error":"actor_type: is not included in the list"}"#,
            "the 422 names the parameter"
        );
    }

    let row = stored(&pool, alice.id).await;
    assert_eq!(row.actor_type, "Service", "the refusals wrote nothing");
    assert!(row.bot.get(), "and so for the bot flag");
}

/// The new parameters are the owner's own settings: the answer carries them
/// in the `source` block and the `pleroma` block, and a stranger's `show`
/// view keeps Pleroma's restricted render of the same row.
#[tokio::test]
async fn the_response_reflects_every_new_value() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let body = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({
                "source": {"privacy": "private", "sensitive": true, "language": "eo"},
                "birthday": "1990-01-31",
                "show_birthday": true,
                "hide_followers": true,
                "no_rich_text": true,
                "pleroma_settings_store": {"theme": "dark"},
                "actor_type": "Service",
            }),
        )
        .await,
    );
    assert_eq!(
        source_of(&body),
        json!({
            "privacy": "private",
            "sensitive": true,
            "language": "eo",
            "note": "",
            "fields": [],
            "follow_requests_count": 0,
            "pleroma": {
                "show_role": true,
                "no_rich_text": true,
                "discoverable": false,
                "actor_type": "Service",
            },
        }),
        "the source block carries the posting defaults and the pleroma settings"
    );
    assert_eq!(body["bot"], true, "the answered bot flag follows the kind");
    let pleroma = pleroma_of(&body);
    assert_eq!(pleroma["birthday"], "1990-01-31", "the owner sees the date");
    assert_eq!(pleroma["show_birthday"], true, "and the flag");
    assert_eq!(pleroma["hide_followers"], true, "and the hidden list");
    assert_eq!(
        pleroma["settings_store"],
        json!({"theme": "dark"}),
        "and the client settings"
    );

    let other = common::create_local_user(&pool, "bob").await;
    let viewer = common::create_app_and_token(&pool, &other, "read:accounts").await;
    let shown = common::TestRequest::get(&format!("/api/v1/accounts/{}", alice.id))
        .bearer(&viewer)
        .send(&app)
        .await;
    let shown_body = shown.json_value();
    assert_eq!(
        shown_body.get("source"),
        None,
        "a stranger gets no source block"
    );
    assert_eq!(
        shown_body["pleroma"]["birthday"], "1990-01-31",
        "and sees the birthday the account chose to show"
    );
    assert_eq!(
        shown_body["pleroma"]["hide_followers"], true,
        "the privacy flags are public information"
    );
    assert_eq!(
        shown_body["pleroma"].get("settings_store"),
        None,
        "but the client settings are not"
    );
}

/// The queued `federator.publish` jobs: `(args, unique_key)` pairs, in the
/// order they landed — what the profile hook's debounce is visible in.
async fn publish_jobs(pool: &PgPool) -> Vec<(Value, Option<String>)> {
    sqlx::query_as::<_, (Value, Option<String>)>(
        "SELECT args, unique_key FROM jobs WHERE kind = 'federator.publish' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("the publish jobs read")
}

/// An accepted profile write publishes the actor document once per burst:
/// the first `PATCH` queues the `Update` — object the actor document, `cc`
/// the followers collection the fan-out expands at delivery time — under
/// the per-account debounce key, and the follow-up writes that arrive
/// while it is still live are swallowed by the queue's dedup.
#[tokio::test]
async fn profile_writes_publish_one_debounced_update() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.followers_url = Some(format!("{BASE}/users/alice/followers"));
        new.inbox = Some(format!("{BASE}/users/alice/inbox"));
        new.private_key = Some("PEM-PRIV".to_owned());
        new.public_key = Some("PEM-PUB".to_owned());
        new
    })
    .await;
    let token = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let first = ok_body(&patch_json(&app, &token, &json!({"display_name": "One"})).await);
    assert_eq!(first["display_name"], "One", "the first write lands");

    let jobs = publish_jobs(&pool).await;
    assert_eq!(jobs.len(), 1, "and publishes once");
    let (args, unique_key) = &jobs[0];
    let expected_key = format!("profile-update:{}", alice.id);
    assert_eq!(
        unique_key.as_deref(),
        Some(expected_key.as_str()),
        "the job is keyed for the debounce"
    );
    let activity = &args["activity"];
    assert_eq!(activity["type"], "Update");
    assert_eq!(activity["actor"], alice.ap_id);
    assert_eq!(
        activity["object"]["id"], alice.ap_id,
        "the object is the actor document itself"
    );
    assert_eq!(
        activity["object"]["name"], "One",
        "carrying the new profile"
    );
    assert_eq!(
        activity["cc"][0],
        format!("{BASE}/users/alice/followers"),
        "addressed to the followers collection"
    );

    for name in ["Two", "Three"] {
        let again = ok_body(&patch_json(&app, &token, &json!({"display_name": name})).await);
        assert_eq!(again["display_name"], name, "the write lands");
    }
    let after_burst = publish_jobs(&pool).await;
    assert_eq!(
        after_burst.len(),
        1,
        "and the burst stayed one queued activity"
    );
}

/// The guard the hook runs before publishing: a row whose `followers_url`
/// is absent has no collection address for the fan-out to expand, so a
/// profile write for it lands without a queued activity.
#[tokio::test]
async fn a_row_without_a_followers_collection_publishes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, _alice) = authed_app(&pool, "quiet").await;

    let body = ok_body(&patch_json(&app, &token, &json!({"display_name": "Quiet"})).await);
    assert_eq!(body["display_name"], "Quiet", "the write lands");
    assert!(
        publish_jobs(&pool).await.is_empty(),
        "a fixture row without a followers collection has no audience to announce to"
    );
}
