//! The instance-document endpoints: upload, serve, replace, delete and the
//! allow-list.

use super::*;

/// The admin document read of a never-uploaded name and the public JSON
/// read of the same absence: both the 404 `{"error":"Not found"}`, the
/// public one with no auth asked.
#[tokio::test]
async fn absent_documents_are_the_json_404_on_both_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let app = common::test_app_with_instance_docs_dir(pool.clone(), dir.path());
    let read_token = admin(&pool, "reader", "admin:read").await;

    let missing = TestRequest::get(DOC_TOS)
        .bearer(&read_token)
        .send(&app)
        .await;
    common::assert_answer(&missing, StatusCode::NOT_FOUND, "never uploaded is a 404");
    common::assert_error(&missing, NOT_FOUND, "the pinned wording");

    let missing_public = TestRequest::get(JSON_TOS).send(&app).await;
    common::assert_answer(
        &missing_public,
        StatusCode::NOT_FOUND,
        "the public 404 needs no auth",
    );
    common::assert_error(&missing_public, NOT_FOUND, "as JSON like everything else");
    assert_eq!(
        missing_public.header("content-type"),
        Some("application/json"),
        "the public miss is the JSON 404, not an HTML page"
    );
}

/// The retired public HTML route is the app-wide fallback 404 even while
/// the document file exists, and serves no page.
#[tokio::test]
async fn the_retired_html_route_is_the_fallback_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let app = common::test_app_with_instance_docs_dir(pool.clone(), dir.path());
    let token = admin(&pool, "root", "admin:write").await;
    let uploaded = upload_sized(&app, &token, 14).await;
    common::assert_answer(&uploaded, StatusCode::OK, "the upload answers");

    let retired = TestRequest::get(PUBLIC_TOS).send(&app).await;
    common::assert_answer(
        &retired,
        StatusCode::NOT_FOUND,
        "the HTML route is gone while the file exists",
    );
    assert_eq!(
        retired.header("x-wired-fallback"),
        Some("1"),
        "the app-wide fallback answered, no handler"
    );
    assert_ne!(
        retired.header("content-type"),
        Some("text/html; charset=utf-8"),
        "no page is served"
    );
}

/// The document store's full walk: the multipart upload answers the
/// frontend URL, the admin read returns the bytes as `text/html`, the
/// public JSON endpoint answers the same text with no auth at all, and
/// the delete empties both reads behind it.
#[tokio::test]
async fn documents_upload_serve_and_delete_round_trip() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let app = common::test_app_with_instance_docs_dir(pool.clone(), dir.path());
    let token = admin(&pool, "root", "admin:write").await;
    let read_token = admin(&pool, "reader", "admin:read").await;

    let uploaded = TestRequest::patch(DOC_TOS)
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &[MultipartPart::file(
                "file",
                "terms.html",
                "text/html",
                Bytes::from_static(b"<h1>Terms</h1>"),
            )],
        )
        .send(&app)
        .await;
    common::assert_answer(&uploaded, StatusCode::OK, "the upload answers");
    assert_eq!(
        uploaded.json_value()["url"],
        "https://test.example/instance/terms-of-service.html",
        "the public URL of the document"
    );
    assert!(
        dir.path().join("terms-of-service.html").is_file(),
        "it landed"
    );

    let read = TestRequest::get(DOC_TOS)
        .bearer(&read_token)
        .send(&app)
        .await;
    common::assert_answer(&read, StatusCode::OK, "the admin read answers");
    assert_eq!(read.text(), "<h1>Terms</h1>", "the bytes verbatim");
    assert_eq!(
        read.header("content-type"),
        Some("text/html"),
        "as text/html, as Pleroma's show sets it"
    );

    let served = TestRequest::get(JSON_TOS).send(&app).await;
    common::assert_answer(&served, StatusCode::OK, "the public read needs no auth");
    assert_eq!(
        served.header("content-type"),
        Some("application/json"),
        "JSON, never a page"
    );
    let document = served.json_value();
    assert_eq!(document["content"], "<h1>Terms</h1>", "the same text");
    let stamp = document["updated_at"]
        .as_str()
        .expect("the update time is a string");
    let written = wired_be::util::time::parse_time(stamp).expect("a parseable stamp");
    let age = chrono::Utc::now()
        .signed_duration_since(written)
        .num_seconds();
    assert!((-5..=60).contains(&age), "the stamp is the write time");

    let deleted = TestRequest::delete(DOC_TOS).bearer(&token).send(&app).await;
    common::assert_answer(&deleted, StatusCode::OK, "the delete answers 200");
    assert_eq!(deleted.json_value(), json!({}), "with the empty object");
    assert!(
        !dir.path().join("terms-of-service.html").exists(),
        "the file is gone"
    );
    let after_delete = TestRequest::get(JSON_TOS).send(&app).await;
    common::assert_answer(&after_delete, StatusCode::NOT_FOUND, "and both reads 404");
    let delete_again = TestRequest::delete(DOC_TOS).bearer(&token).send(&app).await;
    common::assert_answer(&delete_again, StatusCode::NOT_FOUND, "Pleroma's enoent 404");
}

/// A re-upload replaces the document whole and leaves no staging file:
/// the atomic write swaps bytes, it does not append or linger.
#[tokio::test]
async fn a_reupload_replaces_the_document_whole() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let app = common::test_app_with_instance_docs_dir(pool.clone(), dir.path());
    let token = admin(&pool, "root", "admin:write").await;

    for body in [b"<h1>Terms</h1>".as_slice(), b"<h1>Terms v2</h1>"] {
        let uploaded = TestRequest::patch(DOC_TOS)
            .bearer(&token)
            .multipart(
                BOUNDARY,
                &[MultipartPart::file(
                    "file",
                    "terms.html",
                    "text/html",
                    Bytes::from_static(body),
                )],
            )
            .send(&app)
            .await;
        common::assert_answer(&uploaded, StatusCode::OK, "the upload answers");
    }
    let served = TestRequest::get(JSON_TOS).send(&app).await;
    assert_eq!(
        served.json_value()["content"],
        "<h1>Terms v2</h1>",
        "whole, not appended"
    );

    let strays = std::fs::read_dir(dir.path())
        .expect("the dir reads")
        .filter_map(Result::ok)
        .count();
    assert_eq!(strays, 1, "no staging file survives");
}

/// The allow-list and the traversal gate: an unlisted name is the 404 on
/// every document route, and `../`-shaped names — percent-encoded or
/// raw — never reach the filesystem, on the write or on either read.
#[tokio::test]
async fn unlisted_and_traversing_names_are_the_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let app = common::test_app_with_instance_docs_dir(pool.clone(), dir.path());
    let token = admin(&pool, "root", "admin:read admin:write").await;

    for path in [
        "/api/v1/pleroma/admin/instance_document/privacy",
        "/api/v1/pleroma/admin/instance_document/%2e%2e%2fevil",
        "/api/v1/pleroma/admin/instance_document/..%2f..%2fetc%2fpasswd",
        "/api/v1/pleroma/admin/instance_document/%2e",
        "/api/v1/pleroma/admin/instance_document/terms-of-service.html",
    ] {
        let refused = TestRequest::get(path).bearer(&token).send(&app).await;
        common::assert_answer(&refused, StatusCode::NOT_FOUND, &format!("GET {path}"));
        let upload = TestRequest::patch(path)
            .bearer(&token)
            .multipart(
                BOUNDARY,
                &[MultipartPart::file(
                    "file",
                    "x.html",
                    "text/html",
                    Bytes::from_static(b"<p>nope</p>"),
                )],
            )
            .send(&app)
            .await;
        common::assert_answer(&upload, StatusCode::NOT_FOUND, &format!("PATCH {path}"));
        let removed = TestRequest::delete(path).bearer(&token).send(&app).await;
        common::assert_answer(&removed, StatusCode::NOT_FOUND, &format!("DELETE {path}"));
    }
    for stray in [
        "evil.html",
        "passwd",
        "privacy.html",
        "terms-of-service.html.html",
    ] {
        assert!(
            !dir.path().join(stray).exists(),
            "the store holds no `{stray}` after the refusals"
        );
    }
}

/// The upload's own gates: the body must carry the `file` part, and the
/// file must fit 1 MiB — exactly 1 MiB lands, one byte more is the 413
/// naming the ceiling, and the rejected upload changed nothing.
#[tokio::test]
async fn the_upload_reads_its_file_part_at_its_ceiling() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let app = common::test_app_with_instance_docs_dir(pool.clone(), dir.path());
    let token = admin(&pool, "root", "admin:write").await;

    let no_file = TestRequest::patch(DOC_TOS)
        .bearer(&token)
        .multipart(BOUNDARY, &[MultipartPart::text("not_the_file", "hello")])
        .send(&app)
        .await;
    common::assert_answer(&no_file, StatusCode::BAD_REQUEST, "no file part is a 400");
    common::assert_error(&no_file, "Missing parameter: file", "the named parameter");

    let json_body = TestRequest::patch(DOC_TOS)
        .bearer(&token)
        .json(&json!({"content": "<h1>x</h1>"}))
        .send(&app)
        .await;
    common::assert_answer(
        &json_body,
        StatusCode::BAD_REQUEST,
        "a json body has no file",
    );

    let fits = upload_sized(&app, &token, MAX_DOC).await;
    common::assert_answer(&fits, StatusCode::OK, "exactly 1 MiB uploads");
    assert_eq!(
        std::fs::read(dir.path().join("terms-of-service.html"))
            .expect("the file reads")
            .len(),
        MAX_DOC,
        "the whole file landed"
    );

    let oversized = upload_sized(&app, &token, MAX_DOC + 1).await;
    common::assert_answer(
        &oversized,
        StatusCode::PAYLOAD_TOO_LARGE,
        "1 MiB + 1 is a 413",
    );
    assert!(
        oversized.text().contains("1048576"),
        "the refusal names the ceiling: {}",
        oversized.text()
    );
    assert_eq!(
        std::fs::read(dir.path().join("terms-of-service.html"))
            .expect("the file still reads")
            .len(),
        MAX_DOC,
        "the rejected upload changed nothing"
    );
}

/// A name list is what the config says: an operator's `about` joins the
/// admin routes, the upload answers the `FRONTEND_BASE_URL` address, and
/// the stock default stops existing when replaced — the JSON terms of
/// service with it.
#[tokio::test]
async fn the_configured_names_are_the_allow_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let mut config = common::test_config_with_instance_docs_dir(dir.path());
    config.statics.documents.names = vec!["about".to_owned()];
    config.frontend_base_url = "https://fe.example".to_owned();
    let app = common::test_app_with_config(pool.clone(), config);
    let token = admin(&pool, "root", "admin:write").await;

    let uploaded = TestRequest::patch("/api/v1/pleroma/admin/instance_document/about")
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &[MultipartPart::file(
                "file",
                "about.html",
                "text/html",
                Bytes::from_static(b"<p>about</p>"),
            )],
        )
        .send(&app)
        .await;
    common::assert_answer(&uploaded, StatusCode::OK, "the configured name uploads");
    assert_eq!(
        uploaded.json_value()["url"],
        "https://fe.example/instance/about.html",
        "at its own URL on the frontend origin"
    );
    let served = TestRequest::get("/instance/about.html").send(&app).await;
    common::assert_answer(
        &served,
        StatusCode::NOT_FOUND,
        "the backend serves no page for it",
    );
    assert_eq!(
        served.header("x-wired-fallback"),
        Some("1"),
        "the app-wide fallback answered"
    );
    let terms = TestRequest::get(JSON_TOS).send(&app).await;
    common::assert_answer(
        &terms,
        StatusCode::NOT_FOUND,
        "no terms-of-service name, no JSON terms",
    );

    let stock = TestRequest::patch(DOC_TOS)
        .bearer(&token)
        .multipart(
            BOUNDARY,
            &[MultipartPart::file(
                "file",
                "tos.html",
                "text/html",
                Bytes::from_static(b"<h1>terms</h1>"),
            )],
        )
        .send(&app)
        .await;
    common::assert_answer(
        &stock,
        StatusCode::NOT_FOUND,
        "a replaced list drops the defaults",
    );
}
