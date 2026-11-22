//! Tests of the `flex` serde helpers through the body extractor.

use super::*;

#[tokio::test]
async fn flex_booleans_coerce_from_forms_and_json() {
    for (text, want) in [
        ("true", true),
        ("TRUE", true),
        ("1", true),
        ("on", true),
        ("yes", true),
        ("t", true),
        (" false", false),
        ("0", false),
        ("OFF", false),
        ("no", false),
        ("f", false),
    ] {
        let follow: Follow = body_of(Some(FORM_MIME), &format!("reblogs={text}"))
            .await
            .unwrap_or_else(|err| panic!("reblogs={text}: {err}"));
        assert_eq!(follow.reblogs, Some(want), "form boolean {text:?}");
    }

    let follow: Follow = body_of(Some(JSON_MIME), r#"{"reblogs":false}"#)
        .await
        .unwrap();
    assert_eq!(follow.reblogs, Some(false), "a real json boolean");

    let nulled: Follow = body_of(Some(JSON_MIME), r#"{"reblogs":null}"#)
        .await
        .unwrap();
    assert_eq!(nulled.reblogs, None, "null stays None");

    let bool_err = body_of::<Follow>(Some(FORM_MIME), "reblogs=maybe")
        .await
        .expect_err("maybe is not a boolean");
    assert_eq!(
        status_of(bool_err),
        StatusCode::UNPROCESSABLE_ENTITY,
        "bad bool 422"
    );

    let strict: Strict = body_of(Some(FORM_MIME), "flag=on&n=3").await.unwrap();
    assert_eq!(
        strict,
        Strict { flag: true, n: 3 },
        "required flex fields deserialize from strings"
    );

    let strict_err = body_of::<Strict>(Some(FORM_MIME), "flag=on")
        .await
        .expect_err("a required flex field cannot be absent");
    assert_eq!(
        status_of(strict_err),
        StatusCode::UNPROCESSABLE_ENTITY,
        "missing strict field 422"
    );
}

#[tokio::test]
async fn flex_integers_coerce_from_forms_and_json() {
    for (text, count, offset) in [
        ("count=42&offset=-7", 42, -7),
        ("count=+7&offset=0", 7, 0),
        ("count= 8 &offset= 9", 8, 9),
    ] {
        let follow: Follow = body_of(Some(FORM_MIME), text)
            .await
            .unwrap_or_else(|err| panic!("{text}: {err}"));
        assert_eq!(follow.count, Some(count), "u32 from {text}");
        assert_eq!(follow.offset, Some(offset), "i64 from {text}");
    }

    let follow: Follow = body_of(Some(JSON_MIME), r#"{"count":7,"offset":-2}"#)
        .await
        .unwrap();
    assert_eq!(
        follow,
        Follow {
            reblogs: None,
            count: Some(7),
            offset: Some(-2),
        },
        "real json numbers"
    );

    for text in [
        "count=4294967296",
        "offset=9223372036854775808",
        "count=1.5",
    ] {
        let err = body_of::<Follow>(Some(FORM_MIME), text)
            .await
            .expect_err(&format!("{text} must be rejected"));
        assert_eq!(
            status_of(err),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{text} is out of range or not an integer"
        );
    }
}

#[tokio::test]
async fn flex_32_bit_integers_coerce_from_forms_and_json() {
    for (text, want) in [
        ("length=243000", Some(243_000)),
        ("length= 8 ", Some(8)),
        ("length=-1", Some(-1)),
    ] {
        let timed: Timed = body_of(Some(FORM_MIME), text)
            .await
            .unwrap_or_else(|err| panic!("{text}: {err}"));
        assert_eq!(timed.length, want, "i32 from {text:?}");
    }

    let timed: Timed = body_of(Some(JSON_MIME), r#"{"length":7}"#).await.unwrap();
    assert_eq!(timed.length, Some(7), "a real json number");

    for body in ["{}", r#"{"length":null}"#, r#"{"other":1}"#] {
        let parsed: Timed = body_of(Some(JSON_MIME), body)
            .await
            .unwrap_or_else(|err| panic!("{body} is no length: {err}"));
        assert_eq!(parsed.length, None, "`{body}` leaves the field None");
    }

    for text in ["length=abc", "length=1.5", "length=2147483648"] {
        let err = body_of::<Timed>(Some(FORM_MIME), text)
            .await
            .expect_err(&format!("{text} must be rejected"));
        assert_eq!(
            status_of(err),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{text} is not an integer the column could hold"
        );
    }
}
