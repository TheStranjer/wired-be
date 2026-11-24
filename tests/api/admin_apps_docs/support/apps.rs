//! The OAuth-app listing, create, patch and delete endpoints.

use super::*;

/// The create half of the walk: the entity carries exactly the seven
/// pinned keys, and the listing finds the new app newest-first beside
/// the whole-table count and the 50-row window.
#[tokio::test]
async fn create_answers_the_entity_the_listing_finds_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:write").await;

    let created = create_app(
        &app,
        &token,
        json!({
            "name": "Console",
            "redirect_uris": ["https://a.example/cb", "https://b.example/cb"],
            "scopes": ["read", "write"],
            "website": "https://console.example",
            "trusted": true,
        }),
    )
    .await;
    common::assert_answer(&created, StatusCode::OK, "create answers the entity");
    let entity = created.json_value();
    let keys: Vec<&str> = entity
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "client_id",
            "client_secret",
            "id",
            "name",
            "redirect_uri",
            "trusted",
            "website"
        ],
        "exactly the seven pinned keys, {keys:?}"
    );
    assert_eq!(entity["name"], "Console", "the name");
    assert_eq!(
        entity["redirect_uri"], "https://a.example/cb\nhttps://b.example/cb",
        "the list normalized to the stored newline spelling"
    );
    assert_eq!(entity["trusted"], json!(true), "the flag answers");
    assert_eq!(entity["website"], "https://console.example", "the homepage");
    assert!(
        entity["client_id"]
            .as_str()
            .is_some_and(|id| id.len() == 43)
            && entity["client_secret"]
                .as_str()
                .is_some_and(|secret| secret.len() == 43),
        "the credentials were minted"
    );

    let listed = TestRequest::get(APPS).bearer(&token).send(&app).await;
    common::assert_answer(&listed, StatusCode::OK, "the admin write lists");
    let page = listed.json_value();
    assert_eq!(page["page_size"], json!(50), "the admin default window");
    assert_eq!(
        page["count"].as_i64(),
        Some(total_apps(&pool).await),
        "count is the whole-table total"
    );
    assert_eq!(page["apps"][0]["id"], entity["id"], "newest app first");
}

/// The patch and delete half of the walk: the patch moves only what it
/// sends, the delete answers 204, and every later write to the id is
/// the 404 `{"error":"Not found"}`.
#[tokio::test]
async fn patch_moves_only_the_sent_fields_and_delete_ends_the_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:write").await;

    let created = create_app(
        &app,
        &token,
        json!({
            "name": "Console",
            "redirect_uris": OOB,
            "scopes": "read write",
            "website": "https://console.example",
            "trusted": true,
        }),
    )
    .await;
    let id = entity_id(&created);

    let patched = TestRequest::patch(&format!("{APPS}/{id}"))
        .bearer(&token)
        .json(&json!({"trusted": false}))
        .send(&app)
        .await;
    common::assert_answer(&patched, StatusCode::OK, "the patch answers the entity");
    let patched_app = patched.json_value();
    assert_eq!(patched_app["trusted"], json!(false), "the flag moved");
    assert_eq!(patched_app["name"], "Console", "everything else stayed");
    assert_eq!(
        patched_app["website"], "https://console.example",
        "present-only patching"
    );

    let stored = oauth_app::get_by_id(&pool, Uuid::parse_str(&id).expect("an id"))
        .await
        .expect("the model reads")
        .expect("the row is there");
    assert_eq!(stored.scopes, "read write", "the scopes survived");
    assert!(!stored.trusted, "and the patch landed in the row");

    let deleted = TestRequest::delete(&format!("{APPS}/{id}"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&deleted, StatusCode::NO_CONTENT, "delete answers 204");
    assert!(deleted.body.is_empty(), "with no body");
    assert!(
        oauth_app::get_by_id(&pool, Uuid::parse_str(&id).expect("an id"))
            .await
            .expect("the model reads")
            .is_none(),
        "the row is gone"
    );

    let gone = TestRequest::patch(&format!("{APPS}/{id}"))
        .bearer(&token)
        .json(&json!({"name": "Ghost"}))
        .send(&app)
        .await;
    common::assert_answer(&gone, StatusCode::NOT_FOUND, "the deleted id is a 404");
    common::assert_error(&gone, NOT_FOUND, "the pinned wording");
    let gone_again = TestRequest::delete(&format!("{APPS}/{id}"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&gone_again, StatusCode::NOT_FOUND, "delete 404s too");
}

/// The text filters: `name` and `client_id` are exact matches, not
/// patterns.
#[tokio::test]
async fn index_filters_by_exact_name_and_client_id() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:read admin:write").await;
    let (_alpha_id, beta_client_id, beta_id, _first_body) = alpha_and_beta(&app, &token).await;

    let by_name = TestRequest::get(&format!("{APPS}?name=Beta"))
        .bearer(&token)
        .send(&app)
        .await;
    let page = by_name.json_value();
    assert_eq!(
        common::ids_of(&page["apps"]),
        vec![beta_id.clone()],
        "one exact match"
    );

    let near_miss = TestRequest::get(&format!("{APPS}?name=alph"))
        .bearer(&token)
        .send(&app)
        .await;
    assert!(
        common::ids_of(&near_miss.json_value()["apps"]).is_empty(),
        "the name filter is exact, not a pattern"
    );

    let by_client = TestRequest::get(&format!("{APPS}?client_id={beta_client_id}"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        common::ids_of(&by_client.json_value()["apps"]),
        vec![beta_id],
        "the client id finds its app"
    );
}

/// The boolean filter and the window: `trusted` keeps one kind, absent
/// keeps both, `page` / `page_size` cut the window while `count` stays
/// the whole-table total, and a non-boolean `trusted` is a 400.
#[tokio::test]
async fn index_windows_pages_and_filters_on_trusted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:read admin:write").await;
    let (alpha_id, _beta_client_id, beta_id, _first_body) = alpha_and_beta(&app, &token).await;

    let trusted = TestRequest::get(&format!("{APPS}?trusted=true"))
        .bearer(&token)
        .send(&app)
        .await;
    let trusted_ids = common::ids_of(&trusted.json_value()["apps"]);
    assert!(
        trusted_ids.contains(&beta_id) && !trusted_ids.contains(&alpha_id),
        "the trusted app is listed, the untrusted one is not: {trusted_ids:?}"
    );

    let untrusted = TestRequest::get(&format!("{APPS}?trusted=0"))
        .bearer(&token)
        .send(&app)
        .await;
    let untrusted_ids = common::ids_of(&untrusted.json_value()["apps"]);
    assert!(
        untrusted_ids.contains(&alpha_id) && !untrusted_ids.contains(&beta_id),
        "trusted=0 is the mirror filter: {untrusted_ids:?}"
    );

    let both = TestRequest::get(APPS).bearer(&token).send(&app).await;
    let both_ids = common::ids_of(&both.json_value()["apps"]);
    assert!(
        both_ids.contains(&alpha_id) && both_ids.contains(&beta_id),
        "absent trusted lists every kind"
    );

    let window = TestRequest::get(&format!("{APPS}?page=1&page_size=1"))
        .bearer(&token)
        .send(&app)
        .await;
    let cut = window.json_value();
    assert_eq!(common::ids_of(&cut["apps"]).len(), 1, "one row");
    assert_eq!(cut["page_size"], json!(1), "the echo of the window");
    assert_eq!(
        cut["count"].as_i64(),
        Some(total_apps(&pool).await),
        "the count ignores the window"
    );

    let junk = TestRequest::get(&format!("{APPS}?trusted=maybe"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&junk, StatusCode::BAD_REQUEST, "junk trusted is a 400");
}

/// The create refusals, in the `{"errors": …}` shape the ticket pins:
/// absent and blank `name`, absent and all-blank `redirect_uris` (both
/// in one answer when both miss), an unknown scope, and the defaults —
/// `read` scopes and an untrusted flag — a bare registration gets.
#[tokio::test]
async fn create_names_its_refusals_and_its_defaults() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:write").await;

    let nothing = create_app(&app, &token, json!({})).await;
    common::assert_answer(&nothing, StatusCode::BAD_REQUEST, "nothing is a 400");
    let body = nothing.json_value();
    assert_eq!(body["errors"]["name"], json!(["can't be blank"]), "name");
    assert_eq!(
        body["errors"]["redirect_uris"],
        json!(["can't be blank"]),
        "and redirect_uris, one answer"
    );

    let blank = create_app(
        &app,
        &token,
        json!({"name": "   ", "redirect_uris": ["", "  "]}),
    )
    .await;
    assert_eq!(
        blank.json_value()["errors"]["name"],
        json!(["can't be blank"]),
        "blank is absent here"
    );

    let junk_scope = create_app(
        &app,
        &token,
        json!({"name": "Ok", "redirect_uris": OOB, "scopes": ["bogus"]}),
    )
    .await;
    common::assert_answer(
        &junk_scope,
        StatusCode::BAD_REQUEST,
        "a bogus scope is a 400",
    );
    assert!(
        junk_scope.json_value()["errors"]["scopes"][0]
            .as_str()
            .expect("a message")
            .contains("bogus"),
        "the refusal names the offender: {}",
        junk_scope.text()
    );

    let bare = create_app(&app, &token, json!({"name": "Bare", "redirect_uris": OOB})).await;
    common::assert_answer(&bare, StatusCode::OK, "the minimal body creates");
    let client_id = entity_client_id(&bare);
    assert_eq!(
        bare.json_value()["website"],
        json!(null),
        "no website is null"
    );
    assert_eq!(bare.json_value()["trusted"], json!(false), "and untrusted");
    let stored = oauth_app::get_by_client_id(&pool, &client_id)
        .await
        .expect("the model reads")
        .expect("the row is there");
    assert_eq!(stored.scopes, "read", "the ticket's default scope");

    let form = TestRequest::post(APPS)
        .bearer(&token)
        .form(&[
            ("name", "Form App"),
            ("redirect_uris", OOB),
            ("trusted", "true"),
            ("scopes", "read write"),
        ])
        .send(&app)
        .await;
    common::assert_answer(&form, StatusCode::OK, "the form body creates");
    assert_eq!(
        form.json_value()["trusted"],
        json!(true),
        "flex reads the string"
    );
}

/// The patch's refusals: an unknown id and a non-uuid id are both the
/// 404 `{"error":"Not found"}`, and a sent-but-blank name still refuses
/// with the create's wording.
#[tokio::test]
async fn patch_refuses_the_unknown_id_and_the_blank_name() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:write").await;

    let ghost = Uuid::now_v7();
    let missed = TestRequest::patch(&format!("{APPS}/{ghost}"))
        .bearer(&token)
        .json(&json!({"name": "Ghost"}))
        .send(&app)
        .await;
    common::assert_answer(&missed, StatusCode::NOT_FOUND, "no row is a 404");
    common::assert_error(&missed, NOT_FOUND, "the pinned wording");

    let malformed = TestRequest::patch(&format!("{APPS}/1337"))
        .bearer(&token)
        .json(&json!({"name": "Integer"}))
        .send(&app)
        .await;
    common::assert_answer(
        &malformed,
        StatusCode::NOT_FOUND,
        "Pleroma's integer id 404s here",
    );

    let created = create_app(
        &app,
        &token,
        json!({"name": "Patchable", "redirect_uris": OOB}),
    )
    .await;
    let id = entity_id(&created);
    let blank = TestRequest::patch(&format!("{APPS}/{id}"))
        .bearer(&token)
        .json(&json!({"name": "  "}))
        .send(&app)
        .await;
    common::assert_answer(&blank, StatusCode::BAD_REQUEST, "a present blank refuses");
    assert_eq!(
        blank.json_value()["errors"]["name"],
        json!(["can't be blank"]),
        "the same wording as on create"
    );
}

/// The delete's refusals: an unknown id and an unparsable id both
/// answer the 404 before any row is touched.
#[tokio::test]
async fn delete_answers_the_gate_then_the_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:write").await;

    let ghost = Uuid::now_v7();
    let missed = TestRequest::delete(&format!("{APPS}/{ghost}"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&missed, StatusCode::NOT_FOUND, "no row to delete");
    common::assert_error(&missed, NOT_FOUND, "the pinned wording");
    let malformed = TestRequest::delete(&format!("{APPS}/not-a-uuid"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&malformed, StatusCode::NOT_FOUND, "no id to parse");
}

/// The trusted flag travels through every answer whole — and no route
/// here acts on it: the authorize page still renders the consent form
/// for a trusted app (the documented note in
/// `wired_be::api::oauth::authorize`).
#[tokio::test]
async fn the_trusted_flag_travels_through_every_answer() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = admin(&pool, "root", "admin:read admin:write").await;

    let created = create_app(
        &app,
        &token,
        json!({"name": "Trusted", "redirect_uris": OOB, "trusted": true}),
    )
    .await;
    assert_eq!(
        created.json_value()["trusted"],
        json!(true),
        "create answers it"
    );
    let id = entity_id(&created);

    let listed = TestRequest::get(&format!("{APPS}?trusted=true"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        listed.json_value()["apps"][0]["trusted"],
        json!(true),
        "the listing carries it"
    );

    let flipped = TestRequest::patch(&format!("{APPS}/{id}"))
        .bearer(&token)
        .json(&json!({"trusted": "false"}))
        .send(&app)
        .await;
    assert_eq!(
        flipped.json_value()["trusted"],
        json!(false),
        "the patch flips it, form spelling included"
    );
    let stored = oauth_app::get_by_id(&pool, Uuid::parse_str(&id).expect("an id"))
        .await
        .expect("the model reads")
        .expect("the row is there");
    assert!(!stored.trusted, "and the row says so");
}
