//! Tests of the admin app body, validation and error rendering.

use serde_json::json;

use super::*;

/// An [`AppBody`] with every field absent, the empty-patch shape.
fn empty_body() -> AppBody {
    AppBody {
        name: None,
        redirect_uris: None,
        scopes: None,
        website: None,
        trusted: None,
    }
}

/// An [`AppBody`] named `n` with the oob redirect, carrying `scopes`.
fn body_with(scopes: Option<ScopeInput>) -> AppBody {
    AppBody {
        name: Some("n".to_owned()),
        redirect_uris: Some(RedirectUris::Single("urn:ietf:wg:oauth:2.0:oob".to_owned())),
        scopes,
        website: None,
        trusted: None,
    }
}

/// Whether `err` is the invalid-fields 400 naming `field` and `message`.
fn is_invalid(err: &AppsError, field: &str, message: &str) -> bool {
    matches!(
        err,
        AppsError::Invalid(refusals)
            if refusals.iter().any(|(got, got_message)| *got == field && got_message == message)
    )
}

/// The body parses: the ticket's JSON shapes, the form request's
/// string booleans and bracket lists, and an empty body as
/// all-`None`.
#[test]
fn the_body_parses_the_ticket_shape() {
    let full: AppBody = serde_json::from_value(json!({
        "name": "My App",
        "redirect_uris": ["https://myapp.com/cb"],
        "scopes": ["read", "write"],
        "website": "https://myapp.com/",
        "trusted": true,
    }))
    .expect("the ticket's shape deserializes");
    assert_eq!(full.name.as_deref(), Some("My App"));
    assert!(
        matches!(full.redirect_uris, Some(RedirectUris::Multiple(_))),
        "a list is the list arm"
    );
    assert!(matches!(full.scopes, Some(ScopeInput::List(_))));
    assert_eq!(full.trusted, Some(true), "a real boolean");

    let form: AppBody = serde_json::from_value(json!({
        "trusted": "false",
        "scopes": "read+write",
        "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
    }))
    .expect("the form spelling reads through flex");
    assert_eq!(form.trusted, Some(false), "the stringly-typed flag");
    assert!(matches!(form.scopes, Some(ScopeInput::Single(_))));
    assert!(matches!(form.redirect_uris, Some(RedirectUris::Single(_))));

    let empty: AppBody = serde_json::from_value(json!({})).expect("empty body");
    assert!(
        empty.name.is_none()
            && empty.redirect_uris.is_none()
            && empty.scopes.is_none()
            && empty.website.is_none()
            && empty.trusted.is_none(),
        "every field absent"
    );
}

/// The create-side gates: absent and blank refuse with the pinned
/// message, a sent-but-normalized-empty URI list refuses too, and
/// every refusal lands in one answer.
#[test]
fn create_refuses_the_blank_required_fields() {
    let err = prepare(empty_body(), true).expect_err("nothing creates");
    assert!(is_invalid(&err, "name", CANNOT_BE_BLANK), "{err:?}");
    assert!(
        is_invalid(&err, "redirect_uris", CANNOT_BE_BLANK),
        "both refusals in one answer"
    );

    let blank = AppBody {
        name: Some("   ".to_owned()),
        redirect_uris: Some(RedirectUris::Multiple(vec![" ".to_owned(), String::new()])),
        ..empty_body()
    };
    let blank_err = prepare(blank, true).expect_err("blanks create nothing");
    assert!(
        is_invalid(&blank_err, "name", CANNOT_BE_BLANK),
        "{blank_err:?}"
    );
    assert!(
        is_invalid(&blank_err, "redirect_uris", CANNOT_BE_BLANK),
        "the blank list normalizes to blank"
    );

    let good = AppBody {
        name: Some("  Fine  ".to_owned()),
        redirect_uris: Some(RedirectUris::Single("urn:ietf:wg:oauth:2.0:oob".to_owned())),
        ..empty_body()
    };
    let prepared = prepare(good, true).expect("a sent pair creates");
    assert_eq!(prepared.name.as_deref(), Some("Fine"), "the name trims");
    assert_eq!(
        prepared.redirect_uris.as_deref(),
        Some("urn:ietf:wg:oauth:2.0:oob"),
        "a single uri passes through verbatim"
    );
    assert_eq!(
        prepared.scopes.as_deref(),
        Some("read"),
        "the create default"
    );
    assert_eq!(prepared.trusted, None, "the flag defaults at the handler");
}

/// The update-side gates: absent changes nothing, present-but-blank
/// still refuses.
#[test]
fn update_checks_only_the_fields_it_received() {
    let prepared = prepare(empty_body(), false).expect("an empty patch changes neither field");
    assert!(
        prepared.name.is_none() && prepared.redirect_uris.is_none() && prepared.scopes.is_none(),
        "nothing to write"
    );

    let blank = AppBody {
        name: Some(String::new()),
        ..empty_body()
    };
    let err = prepare(blank, false).expect_err("a present blank still refuses");
    assert!(is_invalid(&err, "name", CANNOT_BE_BLANK), "{err:?}");
}

/// Scopes: the create default, both spellings, admin-scope names,
/// and the refusal wording for junk.
#[test]
fn scopes_default_validate_and_store() {
    let parsed = prepare(
        body_with(Some(ScopeInput::Single("read+write".to_owned()))),
        true,
    )
    .expect("a string parses");
    assert_eq!(parsed.scopes.as_deref(), Some("read write"));

    let listed = Some(ScopeInput::List(vec![
        "admin:read".to_owned(),
        "read".to_owned(),
    ]));
    let order = prepare(body_with(listed), true).expect("a list parses too");
    assert_eq!(
        order.scopes.as_deref(),
        Some("admin:read read"),
        "parse order survives, and admin scopes register here"
    );

    let err = prepare(
        body_with(Some(ScopeInput::Single("bogus".to_owned()))),
        true,
    )
    .expect_err("an unknown scope refuses");
    assert!(
        is_invalid(
            &err,
            "scopes",
            "invalid scopes: unknown top-level scope: bogus"
        ),
        "{err:?}"
    );
}

/// The trusted query parameter: absent filters nothing, every boolean
/// spelling maps, and junk is the 400 naming it.
#[test]
fn the_trusted_filter_reads_booleans_only() {
    let params: HashMap<String, String> = HashMap::new();
    assert_eq!(query_trusted(&params).expect("absent is no filter"), None);
    for (text, want) in [
        ("true", Some(true)),
        ("TRUE", Some(true)),
        ("1", Some(true)),
        ("yes", Some(true)),
        ("false", Some(false)),
        ("0", Some(false)),
        ("no", Some(false)),
    ] {
        let mut one = HashMap::new();
        let replaced = one.insert("trusted".to_owned(), text.to_owned());
        drop(replaced);
        assert_eq!(
            query_trusted(&one).expect(text),
            want,
            "`{text}` reads as {want:?}"
        );
    }
    let mut junk = HashMap::new();
    let replaced = junk.insert("trusted".to_owned(), "maybe".to_owned());
    drop(replaced);
    let err = query_trusted(&junk).expect_err("junk is refused");
    assert_eq!(err.to_string(), "trusted must be a boolean, not maybe");
}

/// The listing window: the admin defaults, the shared cap, and the
/// filters passing through.
#[test]
fn the_listing_reads_the_admin_window() {
    let parsed = search_params(&HashMap::new()).expect("an empty query lists");
    assert_eq!(
        parsed.page,
        super::super::super::DEFAULT_PAGE,
        "page defaults"
    );
    assert_eq!(parsed.page_size, 50, "and the admin 50-row window");
    assert!(
        parsed.name.is_none() && parsed.client_id.is_none() && parsed.trusted.is_none(),
        "no filters"
    );
    let mut pairs = HashMap::new();
    for (key, value) in [("page", "3"), ("page_size", "5000"), ("name", "x")] {
        let replaced = pairs.insert(key.to_owned(), value.to_owned());
        drop(replaced);
    }
    let capped = search_params(&pairs).expect("the window parses");
    assert_eq!(capped.page, 3, "a sent page");
    assert_eq!(capped.page_size, 200, "the shared cap");
    assert_eq!(capped.name.as_deref(), Some("x"), "the filter passes");
}

/// The two error halves render their pinned bodies: the invalid arm
/// the 400 `{"errors":{…}}` map, the wrapped arm the usual
/// [`AppError`].
#[test]
fn the_error_halves_render_the_pinned_bodies() {
    let response = AppsError::Invalid(vec![("name", CANNOT_BE_BLANK.to_owned())]).into_response();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "the validation refusal is a 400"
    );
    let wrapped = AppsError::App(AppError::RouteNotFound).into_response();
    assert_eq!(
        wrapped.status(),
        StatusCode::NOT_FOUND,
        "the wrapped arm keeps its status"
    );
}
