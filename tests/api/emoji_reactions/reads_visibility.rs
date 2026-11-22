//! The reactions read's visibility rules and edges: muted and deactivated
//! reactors, the summary cap, token gates and the batch render.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::models::{
    emoji_reaction::MAX_SUMMARY_REACTORS,
    mute::{self, MuteOptions},
    status::Visibility,
    user::User,
};

use super::support::{
    BAD_PARAM, BAD_TOKEN, CROWD, NO_READ, PARTY, THUMB, entry_keys, field, get, known, listed,
    names, public_post, react_ok, reader_and_writer, refused, shown, summary, writer_only,
};
use crate::common;

#[tokio::test]
async fn muted_reactors_need_an_explicit_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (bob, bob_token) = reader_and_writer(&pool, "bob").await;
    let (carol, carol_token) = reader_and_writer(&pool, "carol").await;
    let status_id = public_post(&pool, "dave").await;
    react_ok(&app, &alice_token, &status_id, THUMB).await;
    react_ok(&app, &bob_token, &status_id, THUMB).await;
    react_ok(&app, &carol_token, &status_id, PARTY).await;

    let silence = mute::mute(&pool, &carol, &bob, MuteOptions::default())
        .await
        .expect("carol mutes bob");
    assert_eq!(silence.muted_id, bob.id, "the edge `with_muted` lifts");

    let plain = listed(&app, Some(&carol_token), &status_id, None).await;
    assert_eq!(
        (names(&plain), shown(&plain, 0)),
        (
            vec![THUMB.to_owned(), PARTY.to_owned()],
            vec![known(&alice)],
        ),
        "by default the muted reactor is out of the thumb group"
    );
    for spelling in ["with_muted=true", "with_muted=1", "with_muted=yes"] {
        let asked = get(
            &app,
            Some(&carol_token),
            &status_id.to_string(),
            None,
            spelling,
        )
        .await;
        common::assert_answer(&asked, StatusCode::OK, spelling);
        assert_eq!(
            shown(&asked.json_value(), 0),
            vec![known(&alice), known(&bob)],
            "{spelling} asks for muted reactors and gets bob back"
        );
    }
    assert_eq!(
        shown(&plain, 0),
        vec![known(&alice)],
        "the flag is per request: the earlier answer stands as it was"
    );
    assert_eq!(
        shown(&listed(&app, Some(&alice_token), &status_id, None).await, 0),
        vec![known(&alice), known(&bob)],
        "and it belongs to the muter's view only"
    );
}

#[tokio::test]
async fn the_status_field_caps_ids_while_the_read_lists_everyone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let status_id = public_post(&pool, "author").await;
    let mut crowd: Vec<(User, String)> = Vec::new();
    for n in 0..CROWD {
        let nickname = format!("reactor{n}");
        let member = reader_and_writer(&pool, &nickname).await;
        react_ok(&app, &member.1, &status_id, THUMB).await;
        crowd.push(member);
    }
    let total = i64::try_from(crowd.len()).expect("the crowd is countable");
    let onlooker = crowd.first().expect("the crowd has a first member");

    let body = listed(&app, Some(&onlooker.1), &status_id, None).await;
    assert_eq!(body[0]["count"], json!(total), "the endpoint counts all");
    assert_eq!(
        shown(&body, 0),
        crowd
            .iter()
            .map(|(who, _)| known(who))
            .collect::<Vec<Value>>(),
        "and lists every one of them as an account, in reaction order"
    );
    assert_eq!(body[0]["me"], json!(true), "the asker is among them");

    let capped = field(&app, None, &status_id).await;
    assert_eq!(
        entry_keys(&capped, 0),
        vec!["account_ids", "count", "me", "name", "url"],
        "the status-field entry is the same five keys with ids instead"
    );
    assert_eq!(capped[0]["count"], json!(total), "the count is the whole");
    assert_eq!(
        capped[0]["account_ids"],
        json!(
            crowd
                .iter()
                .take(MAX_SUMMARY_REACTORS)
                .map(|(who, _)| who.id.to_string())
                .collect::<Vec<String>>()
        ),
        "and the ids are the first {MAX_SUMMARY_REACTORS} reactors"
    );
    assert_eq!(
        capped[0]["me"],
        json!(false),
        "an anonymous field render flags nobody, however many reacted"
    );
}

#[tokio::test]
async fn a_deactivated_reactor_leaves_its_emoji_s_list_and_count() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (ghost, ghost_token) = reader_and_writer(&pool, "ghost").await;
    let status_id = public_post(&pool, "bob").await;
    react_ok(&app, &alice_token, &status_id, THUMB).await;
    react_ok(&app, &ghost_token, &status_id, THUMB).await;
    react_ok(&app, &ghost_token, &status_id, PARTY).await;

    let gone = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(ghost.id)
        .execute(&pool)
        .await
        .expect("the fixture deactivates the reactor")
        .rows_affected();
    assert_eq!(gone, 1, "the account was there to suspend");

    let body = listed(&app, Some(&alice_token), &status_id, None).await;
    assert_eq!(
        (names(&body), shown(&body, 0)),
        (
            vec![THUMB.to_owned(), PARTY.to_owned()],
            vec![known(&alice)],
        ),
        "the suspended account is out of its groups, Pleroma's `fetch_users` \
         keeping only the active ones"
    );
    assert_eq!(body[0]["count"], json!(1), "so the shared emoji counts one");
    assert_eq!(body[1]["count"], json!(0), "and the ghost's own emoji none");
    assert_eq!(
        body[1]["accounts"],
        json!([]),
        "the emoji is still named, with nothing behind it"
    );
}

#[tokio::test]
async fn the_reads_need_no_token_and_a_presented_one_needs_read_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, alice_token) = reader_and_writer(&pool, "alice").await;
    let (_writer, write_only) = writer_only(&pool, "writer").await;
    let app_token = common::create_app_only_token(&pool, "read:statuses").await;
    let status_id = public_post(&pool, "bob").await;
    react_ok(&app, &alice_token, &status_id, THUMB).await;
    let status_text = status_id.to_string();

    for variant in [None, Some(THUMB)] {
        let anonymous = get(&app, None, &status_text, variant, "").await;
        common::assert_answer(&anonymous, StatusCode::OK, "an anonymous read");
        assert_eq!(
            anonymous.json_value()[0]["me"],
            json!(false),
            "and `me` is false without a viewer"
        );
    }
    assert_eq!(
        listed(&app, Some(&app_token), &status_id, None).await[0]["me"],
        json!(false),
        "an app-only token proceeds unauthenticated"
    );
    for variant in [None, Some(THUMB)] {
        refused(
            &get(&app, Some(&write_only), &status_text, variant, "").await,
            StatusCode::FORBIDDEN,
            NO_READ,
            "a user token without read:statuses",
        );
        refused(
            &get(&app, Some("not-a-token"), &status_text, variant, "").await,
            StatusCode::UNAUTHORIZED,
            BAD_TOKEN,
            "a token that does not resolve",
        );
    }
    assert_eq!(
        listed(&app, Some(&alice_token), &status_id, Some(THUMB)).await[0]["me"],
        json!(true),
        "the reactor's own token is the one that sees `me`"
    );
    assert_eq!(
        field(&app, Some(&alice_token), &status_id).await,
        json!([summary(THUMB, 1, true, &Value::Null, &[&alice.id])]),
        "in the status field as well"
    );
}

#[tokio::test]
async fn a_with_muted_that_is_not_a_boolean_is_a_four_two_two() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;

    for spelling in ["with_muted=maybe", "with_muted=2"] {
        let response = get(&app, Some(&token), &status_id.to_string(), None, spelling).await;
        common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, spelling);
        let message = response.json_value()["error"]
            .as_str()
            .expect("an error body")
            .to_owned();
        assert!(
            message.starts_with(BAD_PARAM),
            "the parameters extractor states what it could not read: {message}"
        );
    }
}

#[tokio::test]
async fn the_field_rides_on_the_batch_render_and_a_nested_original() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let author = common::create_local_user(&pool, "author").await;
    let original = common::create_status(&pool, &author, "original", Visibility::Public)
        .await
        .id;
    let reblogger = common::create_local_user(&pool, "reblogger").await;
    let reblog = common::create_status(&pool, &reblogger, "", Visibility::Public)
        .await
        .id;
    let linked = sqlx::query("UPDATE statuses SET reblog_of_id = $1 WHERE id = $2")
        .bind(original)
        .bind(reblog)
        .execute(&pool)
        .await
        .expect("the fixture reblogs the original")
        .rows_affected();
    assert_eq!(linked, 1, "both rows were there to link");
    react_ok(&app, &token, &original, THUMB).await;

    let response =
        common::TestRequest::get(&format!("/api/v1/statuses?id[]={original}&id[]={reblog}"))
            .bearer(&token)
            .send(&app)
            .await;
    common::assert_answer(&response, StatusCode::OK, "the batch read");
    let body = response.json_value();
    let mine = json!([summary(THUMB, 1, true, &Value::Null, &[&alice.id])]);
    assert_eq!(
        body[0]["pleroma"]["emoji_reactions"], mine,
        "the original's roll-up rides on the batch render"
    );
    assert_eq!(
        body[1]["pleroma"]["emoji_reactions"],
        json!([]),
        "the reblog row itself was reacted to by nobody"
    );
    assert_eq!(
        body[1]["reblog"]["pleroma"]["emoji_reactions"], mine,
        "and the nested original carries its own"
    );
}
