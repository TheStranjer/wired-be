//! The instance-document endpoints: upload, serve, replace, delete and the
//! allow-list.

use super::*;

/// The admin document read of a never-uploaded name and the public read
/// of the same absence: both the 404 `{"error":"Not found"}`, the
/// public one as JSON and with no auth asked.
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

    let missing_public = TestRequest::get(PUBLIC_TOS).send(&app).await;
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

/// The document store's full walk: the multipart upload answers the
/// public URL, the admin read returns the bytes as `text/html`, the
/// public route serves them with the pinned headers and no auth at all,
/// a re-upload replaces the file whole, and the delete empties both
/// routes behind it.
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

    let served = TestRequest::get(PUBLIC_TOS).send(&app).await;
    common::assert_answer(&served, StatusCode::OK, "the public read needs no auth");
    assert_eq!(served.text(), "<h1>Terms</h1>", "the same bytes");
    assert_eq!(
        served.header("content-type"),
        Some("text/html; charset=utf-8"),
        "with the charset"
    );
    assert_eq!(
        served.header("content-security-policy"),
        Some(DOCUMENT_CSP),
        "and the document CSP"
    );

    let no_suffix = TestRequest::get("/instance/terms-of-service")
        .send(&app)
        .await;
    common::assert_answer(
        &no_suffix,
        StatusCode::NOT_FOUND,
        "the suffix is part of the route",
    );
    let wrong_suffix = TestRequest::get("/instance/terms-of-service.htm")
        .send(&app)
        .await;
    common::assert_answer(
        &wrong_suffix,
        StatusCode::NOT_FOUND,
        "and it is exactly .html",
    );

    let deleted = TestRequest::delete(DOC_TOS).bearer(&token).send(&app).await;
    common::assert_answer(&deleted, StatusCode::OK, "the delete answers 200");
    assert_eq!(deleted.json_value(), json!({}), "with the empty object");
    assert!(
        !dir.path().join("terms-of-service.html").exists(),
        "the file is gone"
    );
    let after_delete = TestRequest::get(PUBLIC_TOS).send(&app).await;
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
    let served = TestRequest::get(PUBLIC_TOS).send(&app).await;
    assert_eq!(served.text(), "<h1>Terms v2</h1>", "whole, not appended");

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

    let public_slip = TestRequest::get("/instance/%2e%2e%2f.env").send(&app).await;
    common::assert_answer(
        &public_slip,
        StatusCode::NOT_FOUND,
        "the public traversal 404s",
    );
    let raw_slip = TestRequest::get("/instance/../uploads/x.html")
        .send(&app)
        .await;
    common::assert_answer(
        &raw_slip,
        StatusCode::NOT_FOUND,
        "the raw ../ never matches",
    );
    assert_eq!(raw_slip.json_value()["error"], NOT_FOUND, "as the JSON 404");
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
/// routes and the stock defaults stop existing when replaced.
#[tokio::test]
async fn the_configured_names_are_the_allow_list() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let dir = tempfile::tempdir().expect("a docs dir");
    let mut config = common::test_config_with_instance_docs_dir(dir.path());
    config.statics.documents.names = vec!["about".to_owned()];
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
        "https://test.example/instance/about.html",
        "at its own URL"
    );
    let served = TestRequest::get("/instance/about.html").send(&app).await;
    common::assert_answer(&served, StatusCode::OK, "and the public route serves it");
    assert_eq!(served.text(), "<p>about</p>", "whole");

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
