//! Tests of `multipart/form-data` bodies through `ApiBody` and `ApiParams`.

use super::*;

/// Boundary the test bodies are framed with.
const BOUNDARY: &str = "wired-extract-boundary";

/// The `content-type` announcing [`BOUNDARY`].
const MULTIPART_TYPE: &str = "multipart/form-data; boundary=wired-extract-boundary";

/// A text part of a multipart body.
fn text_part(name: &str, value: &str) -> String {
    format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
}

/// A file part of a multipart body.
fn file_part(name: &str, filename: &str, data: &str) -> String {
    format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; \
         filename=\"{filename}\"\r\nContent-Type: text/plain\r\n\r\n{data}\r\n"
    )
}

/// `parts` concatenated and closed off with the final boundary.
fn multipart_body(parts: &[String]) -> String {
    format!("{}--{BOUNDARY}--\r\n", parts.concat())
}

/// The text parts for `pairs`, framed as a complete multipart body.
fn text_body(pairs: &[(&str, &str)]) -> String {
    let parts: Vec<String> = pairs
        .iter()
        .map(|(name, value)| text_part(name, value))
        .collect();
    multipart_body(&parts)
}

#[tokio::test]
async fn multipart_text_fields_match_the_urlencoded_value() {
    let pairs = [
        ("status", "hello world"),
        ("visibility", "unlisted"),
        ("media_ids[]", "1"),
        ("media_ids[]", "2"),
        ("poll[options][]", "yes"),
        ("poll[options][]", "no"),
        ("poll[expires_in]", "3600"),
        ("a[b][c]", "d"),
        ("fields_attributes[0][name]", "site"),
        ("fields_attributes[0][value]", "example.com"),
        ("fields_attributes[1][name]", "pronouns"),
        ("tag", "x"),
        ("tag", "y"),
    ];
    let urlencoded = "status=hello+world&visibility=unlisted&media_ids[]=1&media_ids[]=2\
         &poll[options][]=yes&poll[options][]=no&poll[expires_in]=3600&a[b][c]=d\
         &fields_attributes[0][name]=site&fields_attributes[0][value]=example.com\
         &fields_attributes[1][name]=pronouns&tag=x&tag=y";

    let from_form: Value = body_of(Some(FORM_MIME), urlencoded).await.unwrap();
    let from_multipart: Value = body_of(Some(MULTIPART_TYPE), &text_body(&pairs))
        .await
        .unwrap();
    assert_eq!(
        from_multipart, from_form,
        "the same keys structure the same way"
    );
    assert_eq!(
        from_multipart["fields_attributes"]["1"]["name"], "pronouns",
        "indexed keys stay object keys"
    );
    assert_eq!(
        from_multipart["media_ids"],
        json!(["1", "2"]),
        "a[] keys make arrays"
    );
}

#[tokio::test]
async fn multipart_scalars_reach_the_typed_parameters() {
    let body = text_body(&[("reblogs", "false"), ("count", "7")]);
    let follow: Follow = body_of(Some(MULTIPART_TYPE), &body).await.unwrap();
    assert_eq!(
        follow,
        Follow {
            reblogs: Some(false),
            count: Some(7),
            offset: None,
        },
        "flex coercion applies to multipart text"
    );

    let upper: Follow = body_of(
        Some("Multipart/Form-Data; boundary=wired-extract-boundary"),
        &body,
    )
    .await
    .unwrap();
    assert_eq!(upper.count, Some(7), "the media type is case-insensitive");
}

#[tokio::test]
async fn a_multipart_file_part_is_unprocessable_and_named() {
    let body = multipart_body(&[
        text_part("status", "with a file"),
        file_part("attachment", "cat.txt", "meow"),
    ]);
    let err = body_of::<Value>(Some(MULTIPART_TYPE), &body)
        .await
        .expect_err("a file has nowhere to go");
    let message = err.to_string();
    assert_eq!(
        status_of(err),
        StatusCode::UNPROCESSABLE_ENTITY,
        "a file part is a 422"
    );
    assert!(
        message.contains("attachment"),
        "the 422 names the part: {message}"
    );

    let params_err = params_of::<Value>("/x?a=1", Some(MULTIPART_TYPE), &body)
        .await
        .expect_err("ApiParams refuses it too");
    assert_eq!(
        status_of(params_err),
        StatusCode::UNPROCESSABLE_ENTITY,
        "422 through ApiParams"
    );
}

#[tokio::test]
async fn a_bad_multipart_boundary_is_a_bad_request() {
    let body = text_body(&[("status", "x")]);
    for content_type in ["multipart/form-data", "multipart/form-data; charset=utf-8"] {
        let err = body_of::<Value>(Some(content_type), &body)
            .await
            .expect_err("no boundary to split on");
        assert_eq!(
            status_of(err),
            StatusCode::BAD_REQUEST,
            "{content_type:?} without a boundary gets 400"
        );
    }
}

#[tokio::test]
async fn a_too_deeply_nested_multipart_key_is_a_bad_request() {
    let name = format!("a{}", "[]".repeat(5000));
    let err = body_of::<Value>(Some(MULTIPART_TYPE), &text_body(&[(name.as_str(), "1")]))
        .await
        .expect_err("a deep key");
    assert_eq!(
        status_of(err),
        StatusCode::BAD_REQUEST,
        "deep keys are refused as in a form"
    );
}

#[tokio::test]
async fn an_empty_multipart_body_is_no_parameters() {
    let none: Value = body_of(Some(MULTIPART_TYPE), "").await.unwrap();
    assert_eq!(none, json!({}), "no bytes at all");

    let closed: Value = body_of(Some(MULTIPART_TYPE), &multipart_body(&[]))
        .await
        .unwrap();
    assert_eq!(closed, json!({}), "only the closing boundary");
}

#[tokio::test]
async fn a_multipart_type_mismatch_is_unprocessable() {
    let err = body_of::<Follow>(Some(MULTIPART_TYPE), &text_body(&[("count", "abc")]))
        .await
        .expect_err("not a number");
    assert_eq!(
        status_of(err),
        StatusCode::UNPROCESSABLE_ENTITY,
        "a mismatch is a 422 as for a form"
    );
}

#[tokio::test]
async fn api_params_merges_multipart_over_query() {
    let body = text_body(&[("count", "9"), ("reblogs", "on")]);
    let follow: Follow = params_of("/x?count=5&offset=1", Some(MULTIPART_TYPE), &body)
        .await
        .unwrap();
    assert_eq!(
        follow,
        Follow {
            reblogs: Some(true),
            count: Some(9),
            offset: Some(1),
        },
        "the multipart fields win shared keys and add their own"
    );

    let kept: Follow = params_of("/x?count=5", Some(MULTIPART_TYPE), "")
        .await
        .unwrap();
    assert_eq!(
        kept.count,
        Some(5),
        "an empty multipart body keeps the query"
    );
}
