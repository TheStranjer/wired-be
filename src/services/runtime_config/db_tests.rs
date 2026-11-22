//! Tests of the stored-row reads and writes through `RuntimeConfig`.

use serde_json::json;

use super::*;
use crate::{
    config::Config,
    services::config_registry::{GROUP_PLEROMA, KEY_INSTANCE, KEY_MRF, KEY_MRF_SIMPLE},
};

/// The stored row is [`RuntimeConfig::get`]'s answer, and the
/// registry default answers while no row exists.
#[tokio::test]
async fn get_reads_the_row_over_the_env_fallback() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let config = Config::for_tests();
    let store = RuntimeConfig::new(&pool, &config);

    let name = store
        .get(GROUP_PLEROMA, KEY_INSTANCE)
        .await
        .expect("the lookup runs")
        .expect("a registered key always has an effective value");
    assert_eq!(
        name["name"],
        json!(config.instance_name),
        "with no row the env value answers"
    );

    let written = store
        .set(
            GROUP_PLEROMA,
            KEY_INSTANCE,
            &json!([{"tuple": ["name", "Renamed"]}]),
        )
        .await
        .expect("the write runs");
    assert_eq!(
        written,
        json!({"name": "Renamed"}),
        "the upsert hands back the merged row"
    );
    let effective = store
        .get(GROUP_PLEROMA, KEY_INSTANCE)
        .await
        .expect("the lookup runs")
        .expect("registered");
    assert_eq!(effective["name"], json!("Renamed"), "the row wins");
    assert_eq!(
        effective["description"],
        json!(config.instance_description),
        "and the untouched siblings keep the env value"
    );

    assert_eq!(
        store
            .get(GROUP_PLEROMA, ":absent_key")
            .await
            .expect("the lookup runs"),
        None,
        "an unregistered pair with no row is nothing at all"
    );
}

/// [`RuntimeConfig::set`] merges each write into the stored row, so
/// a second subkey never drops the first one.
#[tokio::test]
async fn set_merges_subkeys_into_the_stored_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let config = Config::for_tests();
    let store = RuntimeConfig::new(&pool, &config);

    let first = store
        .set(
            GROUP_PLEROMA,
            KEY_MRF,
            &json!([{"tuple": ["policies", ["simple"]]}]),
        )
        .await
        .expect("the first write runs");
    assert_eq!(
        first,
        json!({"policies": ["simple"]}),
        "the decoded tuple arrives as the stored object"
    );
    let stored = store
        .set(
            GROUP_PLEROMA,
            KEY_MRF_SIMPLE,
            &json!([{"tuple": ["reject", [["evil.example", "bots"]]]}]),
        )
        .await
        .expect("the second write runs");
    assert_eq!(
        stored,
        json!({"reject": [["evil.example", "bots"]]}),
        "the whole :mrf_simple value is stored"
    );
    let rewritten = store
        .set(GROUP_PLEROMA, KEY_MRF, &json!([]))
        .await
        .expect("a plain replace arrives as the empty list");
    assert_eq!(
        rewritten,
        json!({"policies": ["simple"]}),
        "an empty wire list merges nothing away"
    );
}

/// [`RuntimeConfig::delete`] drops subkeys, keeps the row while any
/// remain, and removes it when the last one goes — or wholesale.
#[tokio::test]
async fn delete_drops_subkeys_then_the_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let config = Config::for_tests();
    let store = RuntimeConfig::new(&pool, &config);

    assert_eq!(
        store
            .delete(GROUP_PLEROMA, KEY_INSTANCE, None)
            .await
            .expect("the delete runs"),
        DeleteOutcome::Absent,
        "deleting what was never stored changes nothing"
    );

    let wrote = store
        .set(
            GROUP_PLEROMA,
            KEY_INSTANCE,
            &json!([{"tuple": ["name", "A"]}, {"tuple": ["email", "a@b.c"]}]),
        )
        .await
        .expect("the write runs");
    assert_eq!(
        wrote,
        json!({"email": "a@b.c", "name": "A"}),
        "both tuples arrive in the row"
    );
    let outcome = store
        .delete(GROUP_PLEROMA, KEY_INSTANCE, Some(&["name".to_owned()]))
        .await
        .expect("the subkey delete runs");
    assert_eq!(
        outcome,
        DeleteOutcome::Updated(json!({"email": "a@b.c"})),
        "the listed subkey is gone and the row keeps the rest"
    );
    let emptied = store
        .delete(GROUP_PLEROMA, KEY_INSTANCE, Some(&[":email".to_owned()]))
        .await
        .expect("the last subkey delete runs");
    assert_eq!(
        emptied,
        DeleteOutcome::Removed,
        "the emptied row is removed with its last subkey"
    );
    assert_eq!(
        stored(&pool, GROUP_PLEROMA, KEY_INSTANCE)
            .await
            .expect("the lookup runs"),
        None,
        "and the row is really gone"
    );

    let stored_mrf = store
        .set(
            GROUP_PLEROMA,
            KEY_MRF,
            &json!([{"tuple": ["policies", ["noop"]]}]),
        )
        .await
        .expect("the write runs");
    assert_eq!(stored_mrf, json!({"policies": ["noop"]}));
    assert_eq!(
        store
            .delete(GROUP_PLEROMA, KEY_MRF, None)
            .await
            .expect("the whole delete runs"),
        DeleteOutcome::Removed,
        "delete without subkeys takes the whole row"
    );
}

/// [`RuntimeConfig::effective_config`] applies the stored `:instance`
/// overrides to the nine fields their subkeys name, and a
/// wrong-typed row falls back to env with the rest of the row still
/// applied.
#[tokio::test]
async fn effective_config_overlays_the_instance_row() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let config = Config::for_tests();
    let store = RuntimeConfig::new(&pool, &config);

    let same = store.effective_config().await.expect("the read runs");
    assert_eq!(
        same.instance_name, config.instance_name,
        "no row, no change"
    );

    write(
        &pool,
        GROUP_PLEROMA,
        KEY_INSTANCE,
        &json!({
            "name": "Renamed",
            "description": "Rewritten",
            "email": "staff@example.test",
            "registrations_open": false,
            "account_approval_required": true,
            "account_activation_required": true,
            "invites_enabled": true,
            "limit": 1200,
            "max_media_attachments": 9,
        }),
    )
    .await
    .expect("the row writes");
    let overlaid = store.effective_config().await.expect("the read runs");
    assert_eq!(overlaid.instance_name, "Renamed", "name");
    assert_eq!(overlaid.instance_description, "Rewritten", "description");
    assert_eq!(
        overlaid.contact_email.as_deref(),
        Some("staff@example.test"),
        "email"
    );
    assert!(!overlaid.registrations_open.get(), "registrations_open");
    assert!(
        overlaid.registrations_approval_required.get(),
        "account_approval_required"
    );
    assert!(
        overlaid.account_activation_required.get(),
        "account_activation_required"
    );
    assert!(overlaid.invites_enabled.get(), "invites_enabled");
    assert_eq!(overlaid.max_status_chars, 1200, "limit");
    assert_eq!(overlaid.max_media_attachments, 9, "max_media_attachments");

    write(
        &pool,
        GROUP_PLEROMA,
        KEY_INSTANCE,
        &json!({"name": 42, "invites_enabled": "yes"}),
    )
    .await
    .expect("the broken row writes");
    let fallback = store.effective_config().await.expect("the read runs");
    assert_eq!(
        fallback.instance_name, config.instance_name,
        "a wrong-typed override keeps the env value"
    );
    assert_eq!(
        fallback.invites_enabled.get(),
        config.invites_enabled.get(),
        "and does not poison its siblings"
    );
}

/// [`RuntimeConfig::mrf_document`] composes the pipeline document
/// from the `:mrf` and `:mrf_*` rows, prefers the per-policy rows,
/// and answers `None` for a store holding none of them.
#[tokio::test]
async fn mrf_document_composes_the_mrf_entries() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let config = Config::for_tests();
    let store = RuntimeConfig::new(&pool, &config);

    assert_eq!(
        store.mrf_document().await.expect("the read runs"),
        None,
        "an empty store configures nothing"
    );

    write(
        &pool,
        GROUP_PLEROMA,
        KEY_MRF,
        &json!({"policies": ["simple"], "simple": {"reject": ["from-mrf.example"]}}),
    )
    .await
    .expect("the :mrf row writes");
    write(
        &pool,
        GROUP_PLEROMA,
        KEY_MRF_SIMPLE,
        &json!({"reject": ["from-entry.example"]}),
    )
    .await
    .expect("the :mrf_simple row writes");
    write(&pool, GROUP_PLEROMA, ":instance", &json!({"name": "N"}))
        .await
        .expect("a non-mrf row writes");

    let document = store
        .mrf_document()
        .await
        .expect("the read runs")
        .expect("the store answers");
    assert_eq!(
        document,
        json!({
            "policies": ["simple"],
            "simple": {"reject": ["from-entry.example"]},
        }),
        "the :mrf fields compose the document and the dedicated :mrf_* row wins"
    );

    let gone = remove(&pool, GROUP_PLEROMA, KEY_MRF)
        .await
        .expect("the :mrf row goes");
    assert!(gone, "the row was there and is gone");
    let lone = store
        .mrf_document()
        .await
        .expect("the read runs")
        .expect("the store answers");
    assert_eq!(
        lone,
        json!({"simple": {"reject": ["from-entry.example"]}}),
        "a lone policy-params entry still forms a document"
    );
}
