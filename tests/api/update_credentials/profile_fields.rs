//! The `fields_attributes` profile metadata: every body shape, plain-text
//! names, clearing, and the dropping of both-empty entries.

use serde_json::json;

use super::support::{
    authed_app, field, ok_body, part, patch_form, patch_json, patch_multipart, source_of, stored,
};
use crate::common;

/// `fields_attributes` updates through the JSON array, the JSON object keyed
/// by index, and the `fields_attributes[0][name]` spelling of a form body.
/// Each round answers the top-level `fields` with the rendered HTML and
/// `source.fields` with the raw text; `fields_attributes_arrive_through_multipart`
/// covers the multipart spelling and the pair of jsonb columns left behind.
#[tokio::test]
async fn fields_attributes_update_through_every_body_shape() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, _alice) = authed_app(&pool, "alice").await;

    let section = json!({"fields_attributes": [
        {"name": "Site", "value": "https://example.com/a"},
        {"name": "Motto", "value": "beep boop"},
    ]});
    let via_json = ok_body(&patch_json(&app, &token, &section).await);
    assert_eq!(
        via_json["fields"],
        json!([
            field(
                "Site",
                "<a href=\"https://example.com/a\" rel=\"ugc nofollow noopener noreferrer\">example.com/a</a>"
            ),
            field("Motto", "beep boop"),
        ]),
        "the JSON array renders: bare URL linked with the standard rel, plain text inline, no `<p>`"
    );
    assert_eq!(
        source_of(&via_json)["fields"],
        json!([
            field("Site", "https://example.com/a"),
            field("Motto", "beep boop"),
        ]),
        "the source block hands back exactly what was typed"
    );

    let via_index_map = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": {
                "1": {"name": "second", "value": "two"},
                "0": {"name": "first", "value": "one"},
            }}),
        )
        .await,
    );
    assert_eq!(
        via_index_map["fields"],
        json!([field("first", "one"), field("second", "two")]),
        "the index-object shape orders by the index as a number"
    );

    let via_form = ok_body(
        &patch_form(
            &app,
            &token,
            &[
                ("fields_attributes[0][name]", "Form"),
                ("fields_attributes[0][value]", "https://form.example/x"),
                ("fields_attributes[1][name]", "Bare"),
                ("fields_attributes[1][value]", "just words"),
            ],
        )
        .await,
    );
    assert_eq!(
        via_form["fields"],
        json!([
            field(
                "Form",
                "<a href=\"https://form.example/x\" rel=\"ugc nofollow noopener noreferrer\">form.example/x</a>"
            ),
            field("Bare", "just words"),
        ]),
        "the form's bracketed index keys fold into the same list"
    );
    assert_eq!(
        source_of(&via_form)["fields"],
        json!([
            field("Form", "https://form.example/x"),
            field("Bare", "just words")
        ]),
        "and the raw text round-trips the same"
    );
}

/// The multipart spelling of the same write, and the pair of jsonb columns it
/// leaves behind: `fields` rendered, `raw_fields` verbatim.
#[tokio::test]
async fn fields_attributes_arrive_through_multipart() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;

    let via_multipart = ok_body(
        &patch_multipart(
            &app,
            &token,
            &[
                part("fields_attributes[0][name]", "Multi"),
                part("fields_attributes[0][value]", "hello"),
            ],
        )
        .await,
    );
    assert_eq!(
        via_multipart["fields"],
        json!([field("Multi", "hello")]),
        "multipart text parts structure like urlencoded ones"
    );

    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.fields,
        json!([{"name": "Multi", "value": "hello"}]),
        "the `fields` column holds the rendered pair"
    );
    assert_eq!(
        row.raw_fields,
        json!([{"name": "Multi", "value": "hello"}]),
        "the `raw_fields` column holds the typed pair"
    );
}

/// A submitted field name is stored as plain text — markup is stripped —
/// while `raw_fields` keeps the spelling as typed, and `source.fields`
/// round-trips that raw spelling so the client's textarea sees what it
/// wrote.
#[tokio::test]
async fn field_names_are_stored_as_plain_text() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let body = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [{"name": "<b>Site</b>", "value": "https://example.com"}]}),
        )
        .await,
    );
    assert_eq!(
        body["fields"][0]["name"], "Site",
        "the rendered name is stripped to text"
    );
    assert_eq!(
        source_of(&body)["fields"][0]["name"],
        "<b>Site</b>",
        "the source keeps the name as typed"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.raw_fields,
        json!([{"name": "<b>Site</b>", "value": "https://example.com"}]),
        "raw_fields round-trips both halves verbatim"
    );
}

/// An empty `fields_attributes` — the JSON array and the form index map
/// alike — clears every stored field from both columns and both renders;
/// its absence leaves them alone.
#[tokio::test]
async fn empty_fields_attributes_clears_the_metadata() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let filled = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [{"name": "Site", "value": "https://example.com"}]}),
        )
        .await,
    );
    assert_eq!(filled["fields"].as_array().map(Vec::len), Some(1));

    let cleared = ok_body(&patch_json(&app, &token, &json!({"fields_attributes": []})).await);
    assert_eq!(cleared["fields"], json!([]), "the entity answers none");
    assert_eq!(
        source_of(&cleared)["fields"],
        json!([]),
        "and neither does the source block"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.fields, json!([]), "the rendered column is empty");
    assert_eq!(row.raw_fields, json!([]), "and the raw column");

    let refilled = ok_body(
        &patch_form(
            &app,
            &token,
            &[
                ("fields_attributes[0][name]", "Back"),
                ("fields_attributes[0][value]", "again"),
            ],
        )
        .await,
    );
    assert_eq!(
        refilled["fields"],
        json!([field("Back", "again")]),
        "the metadata can be written again after clearing"
    );
    let empty_map = ok_body(&patch_json(&app, &token, &json!({"fields_attributes": {}})).await);
    assert_eq!(
        empty_map["fields"],
        json!([]),
        "the empty index-object clears like the empty array"
    );

    let untouched = ok_body(&patch_json(&app, &token, &json!({"display_name": "Ada"})).await);
    assert_eq!(
        untouched["fields"],
        json!([]),
        "a body without fields_attributes leaves the (now empty) metadata alone"
    );
}

/// An entry whose name and value are both empty is dropped rather than
/// stored; one with only a name or only a value is kept, so a form that
/// half-fills a row still writes it.
#[tokio::test]
async fn entries_with_both_empty_name_and_value_are_dropped() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let body = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [
                {"name": "", "value": ""},
                {},
                "not even an object",
                {"name": "NameOnly", "value": ""},
                {"value": "value only"},
                {"name": "Kept", "value": "x"},
            ]}),
        )
        .await,
    );
    assert_eq!(
        body["fields"],
        json!([
            field("NameOnly", ""),
            field("", "value only"),
            field("Kept", "x"),
        ]),
        "both-empty entries (and non-objects, which read as empty) go, half-entries stay in order"
    );
    assert_eq!(
        source_of(&body)["fields"],
        json!([
            field("NameOnly", ""),
            field("", "value only"),
            field("Kept", "x"),
        ]),
        "the source block sees the same list raw"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(
        row.raw_fields,
        json!([
            {"name": "NameOnly", "value": ""},
            {"name": "", "value": "value only"},
            {"name": "Kept", "value": "x"},
        ]),
        "and it is exactly what the two columns store"
    );
}

/// An all-empty submission drops to nothing, which is the clear: the
/// stored metadata is replaced by the empty pair, not left behind.
#[tokio::test]
async fn an_all_empty_submission_clears_like_the_empty_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (app, token, alice) = authed_app(&pool, "alice").await;
    let filled = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [{"name": "Site", "value": "https://example.com"}]}),
        )
        .await,
    );
    assert_eq!(filled["fields"].as_array().map(Vec::len), Some(1));

    let cleared = ok_body(
        &patch_json(
            &app,
            &token,
            &json!({"fields_attributes": [{"name": "", "value": ""}, {}]}),
        )
        .await,
    );
    assert_eq!(
        cleared["fields"],
        json!([]),
        "every entry dropped leaves the empty list"
    );
    let row = stored(&pool, alice.id).await;
    assert_eq!(row.fields, json!([]), "stored rendered");
    assert_eq!(row.raw_fields, json!([]), "stored raw");
}
