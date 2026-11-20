//! The posting defaults of the `source` block (`privacy`, `sensitive`,
//! `language`), the eleven Pleroma preference flags and `birthday`.

use axum::http::StatusCode;
use chrono::{NaiveDate, Utc};
use serde_json::{Value, json};
use wired_be::models::user::User;

use super::support::{
    authed_app, ok_body, one_flag, part, patch_form, patch_json, patch_multipart, pleroma_of,
    source_of, stored,
};
use crate::common;

/// The eleven Pleroma preference flags `update_credentials` writes, in the
/// order the entity renders them.
const PREF_FLAGS: [&str; 11] = [
    "hide_followers",
    "hide_follows",
    "hide_followers_count",
    "hide_follows_count",
    "hide_favorites",
    "skip_thread_containment",
    "allow_following_move",
    "accepts_chat_messages",
    "show_birthday",
    "show_role",
    "no_rich_text",
];

/// The two of [`PREF_FLAGS`] Pleroma renders inside `source.pleroma` rather
/// than the top-level `pleroma` block.
const SOURCE_FLAGS: [&str; 2] = ["show_role", "no_rich_text"];

/// The value the owner entity answers for one preference flag, wherever that
/// key lives.
fn flag_of(body: &Value, field: &str) -> Value {
    if SOURCE_FLAGS.contains(&field) {
        return source_of(body)["pleroma"][field].clone();
    }
    pleroma_of(body)[field].clone()
}

/// The stored column behind one preference flag, for the assertions that
/// read the row rather than the answer.
fn stored_flag(row: &User, field: &str) -> bool {
    match field {
        "hide_followers" => row.hide_followers.get(),
        "hide_follows" => row.hide_follows.get(),
        "hide_followers_count" => row.hide_followers_count.get(),
        "hide_follows_count" => row.hide_follows_count.get(),
        "hide_favorites" => row.hide_favorites.get(),
        "skip_thread_containment" => row.skip_thread_containment.get(),
        "allow_following_move" => row.allow_following_move.get(),
        "accepts_chat_messages" => row.accepts_chat_messages.get(),
        "show_birthday" => row.show_birthday.get(),
        "show_role" => row.show_role.get(),
        "no_rich_text" => row.no_rich_text.get(),
        other => panic!("`{other}` is not one of the eleven preference flags"),
    }
}

/// `source[privacy]` sets the posting default, in all three spellings the
/// endpoint takes it in — the nested JSON object, the bracketed form key and
/// Pleroma's flat `default_scope` — with the block winning over the flat
/// name, as `AccountController.update_credentials` applies them.
#[tokio::test]
async fn source_privacy_updates_the_posting_default() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_json =
        ok_body(&patch_json(&app, &token, &json!({"source": {"privacy": "private"}})).await);
    assert_eq!(
        source_of(&via_json)["privacy"],
        "private",
        "the nested JSON object answers the new default"
    );

    let via_form = ok_body(&patch_form(&app, &token, &[("source[privacy]", "unlisted")]).await);
    assert_eq!(
        source_of(&via_form)["privacy"],
        "unlisted",
        "the bracketed form key is the same parameter"
    );

    let via_flat = ok_body(&patch_form(&app, &token, &[("default_scope", "direct")]).await);
    assert_eq!(
        source_of(&via_flat)["privacy"],
        "direct",
        "and so is Pleroma's flat name"
    );

    let both = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"default_scope": "public", "source": {"privacy": "private"}}),
        )
        .await,
    );
    assert_eq!(
        source_of(&both)["privacy"],
        "private",
        "the block wins over the flat name"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.default_scope, "private",
        "the column is the answered privacy"
    );
}

/// A privacy outside the four visibilities is the 422, in either spelling,
/// and the refused body writes nothing at all.
#[tokio::test]
async fn an_unknown_privacy_is_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    for body in [
        json!({"source": {"privacy": "internal"}}),
        json!({"default_scope": "protected"}),
        json!({"source": {"privacy": ""}}),
        json!({"source": {"privacy": "PUBLIC"}}),
    ] {
        let response = patch_json(&app, &token, &body).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{body}` names no visibility"
        );
        assert_eq!(
            response.text(),
            r#"{"error":"privacy: is not included in the list"}"#,
            "the 422 names the parameter"
        );
    }
    let mixed = patch_json(
        &app,
        &token,
        &json!({"display_name": "Ada", "source": {"privacy": "internal"}}),
    )
    .await;
    assert_eq!(
        mixed.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a good field riding with a bad privacy is refused whole"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.default_scope, "public", "nothing was written");
    assert_eq!(row.name, "", "and neither was the name");
}

/// `source[sensitive]` writes the sensitive posting default through every
/// body type, flex booleans and all, and never moves its sibling.
#[tokio::test]
async fn source_sensitive_updates_the_sensitive_default() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_json =
        ok_body(&patch_json(&app, &token, &json!({"source": {"sensitive": true}})).await);
    assert_eq!(
        source_of(&via_json)["sensitive"],
        true,
        "the JSON boolean answers the new default"
    );

    let via_form = ok_body(&patch_form(&app, &token, &[("source[sensitive]", "on")]).await);
    assert_eq!(
        source_of(&via_form)["sensitive"],
        true,
        "a form `on` is the same true"
    );

    let via_multipart =
        ok_body(&patch_multipart(&app, &token, &[part("source[sensitive]", "0")]).await);
    assert_eq!(
        source_of(&via_multipart)["sensitive"],
        false,
        "and a multipart `0` is a false"
    );
    assert_eq!(
        source_of(&via_multipart)["privacy"],
        "public",
        "the privacy the body never named stayed"
    );

    let bad = patch_form(&app, &token, &[("source[sensitive]", "maybe")]).await;
    assert_eq!(
        bad.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unknown boolean string is the extractor's 422"
    );
    let row = stored(&pool, alice.id).await;
    assert!(
        !row.sensitive_default.get(),
        "the stored flag is the answered one"
    );
    assert_eq!(
        row.default_scope, "public",
        "and the refusals wrote nothing"
    );
}

/// `source[language]` writes the language posting default, the empty string
/// clears it back to `null`, and a tag over the ten-character cap is the 422.
#[tokio::test]
async fn source_language_is_capped_and_the_empty_tag_clears() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_json =
        ok_body(&patch_json(&app, &token, &json!({"source": {"language": "pt-BR"}})).await);
    assert_eq!(
        source_of(&via_json)["language"],
        "pt-BR",
        "the JSON object answers the tag"
    );

    let via_form = ok_body(&patch_form(&app, &token, &[("source[language]", "de")]).await);
    assert_eq!(
        source_of(&via_form)["language"],
        "de",
        "the bracketed form key is the same parameter"
    );

    let at_cap = "é".repeat(10);
    let capped = ok_body(&patch_form(&app, &token, &[("source[language]", &at_cap)]).await);
    assert_eq!(
        source_of(&capped)["language"],
        at_cap,
        "ten characters fit, counted as characters, not bytes"
    );

    let over = patch_json(
        &app,
        &token,
        &json!({"source": {"language": "0123456789a"}}),
    )
    .await;
    assert_eq!(
        over.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "eleven characters are over the cap"
    );
    assert_eq!(
        over.text(),
        r#"{"error":"Language is too long (maximum is 10 characters)"}"#,
        "the length message names the cap"
    );

    let cleared = ok_body(&patch_form(&app, &token, &[("source[language]", "")]).await);
    assert_eq!(
        source_of(&cleared)["language"],
        Value::Null,
        "the empty string clears the preference"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.language, None, "the column is NULL again");
    assert_eq!(
        row.default_scope, "public",
        "the language write left the privacy alone"
    );
}

/// Each of the eleven preference flags updates on its own, through JSON,
/// form and multipart bodies, reaching the entity wherever Pleroma renders
/// that key and never moving any of the other ten.
#[tokio::test]
async fn each_pleroma_flag_updates_on_its_own() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let initial = ok_body(&patch_json(&app, &token, &json!({"display_name": "Ada"})).await);
    let mut expect = json!({});
    for field in PREF_FLAGS {
        expect[field] = flag_of(&initial, field);
    }

    for field in PREF_FLAGS {
        expect[field] = json!(true);
        let via_json = ok_body(&patch_json(&app, &token, &one_flag(field, true)).await);
        assert_pref_flags(&via_json, &expect, &format!("a JSON true for {field}"));

        expect[field] = json!(false);
        let via_form = ok_body(&patch_form(&app, &token, &[(field, "false")]).await);
        assert_pref_flags(&via_form, &expect, &format!("a form false for {field}"));

        expect[field] = json!(true);
        let via_multipart = ok_body(
            &patch_multipart(
                &app,
                &token,
                &[part(field, "yes"), part("source[privacy]", "direct")],
            )
            .await,
        );
        assert_pref_flags(
            &via_multipart,
            &expect,
            &format!("a multipart yes for {field}"),
        );
        assert_eq!(
            source_of(&via_multipart)["privacy"],
            "direct",
            "the privacy rode along with the flag"
        );
    }

    let row = stored(&pool, alice.id).await;
    for field in PREF_FLAGS {
        assert!(
            stored_flag(&row, field),
            "{field} is the stored true the answer stated"
        );
    }
    assert_eq!(row.default_scope, "direct", "the stored privacy too");
}

/// Assert every preference flag of the answer against `expect` — the running
/// stored state, so each round also proves the ten flags it did not send
/// stayed where they were.
fn assert_pref_flags(body: &Value, expect: &Value, why: &str) {
    for field in PREF_FLAGS {
        assert_eq!(flag_of(body, field), expect[field], "{why}: {field}");
    }
}

/// `birthday` is stored from a JSON body and a form alike, the empty string
/// clears the column, and the answer renders the date back in the owner's
/// `pleroma` block.
#[tokio::test]
async fn birthday_updates_and_the_empty_string_clears_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_json = ok_body(&patch_json(&app, &token, &json!({"birthday": "1990-01-31"})).await);
    assert_eq!(
        pleroma_of(&via_json)["birthday"],
        "1990-01-31",
        "the owner view answers the stored date"
    );

    let via_form = ok_body(&patch_form(&app, &token, &[("birthday", "2000-02-29")]).await);
    assert_eq!(
        pleroma_of(&via_form)["birthday"],
        "2000-02-29",
        "a form body is the same parameter"
    );
    assert_eq!(
        stored(&pool, alice.id).await.birthday,
        NaiveDate::from_ymd_opt(2000, 2, 29),
        "the leap day is what the row holds"
    );

    let shown = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"show_birthday": true, "birthday": "1990-01-31"}),
        )
        .await,
    );
    assert_eq!(
        pleroma_of(&shown)["show_birthday"],
        true,
        "the flag a client sets beside the date sticks"
    );

    let cleared = ok_body(&patch_form(&app, &token, &[("birthday", "")]).await);
    assert_eq!(
        pleroma_of(&cleared)["birthday"],
        Value::Null,
        "the empty string clears the date"
    );
    assert_eq!(
        stored(&pool, alice.id).await.birthday,
        None,
        "and stores NULL behind it"
    );
}

/// A birthday that is not `YYYY-MM-DD` text or one after today is the 422,
/// and neither writes anything.
#[tokio::test]
async fn an_invalid_or_future_birthday_is_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let stored_date = ok_body(&patch_json(&app, &token, &json!({"birthday": "1990-01-31"})).await);
    assert_eq!(
        pleroma_of(&stored_date)["birthday"],
        "1990-01-31",
        "the fixture date is stored"
    );

    for text in [
        "31-01-1990",
        "1990-1-31",
        "1990-13-01",
        "yesterday",
        "1990-01-31T00:00:00Z",
    ] {
        let response = patch_json(&app, &token, &json!({"birthday": text})).await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{text}` is no `YYYY-MM-DD` date"
        );
        assert_eq!(
            response.text(),
            r#"{"error":"birthday: is not a valid date"}"#,
            "the parse 422 names the parameter"
        );
    }

    let tomorrow = Utc::now()
        .date_naive()
        .succ_opt()
        .expect("tomorrow exists")
        .format("%Y-%m-%d")
        .to_string();
    let future = patch_form(&app, &token, &[("birthday", &tomorrow)]).await;
    assert_eq!(
        future.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "`{tomorrow}` is in the future"
    );
    assert_eq!(
        future.text(),
        r#"{"error":"birthday: must not be in the future"}"#,
        "the future 422 names the parameter"
    );

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.birthday,
        NaiveDate::from_ymd_opt(1990, 1, 31),
        "no refusal moved the stored date"
    );
}
