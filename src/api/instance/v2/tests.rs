//! The v2 document's tests: its key sets, the identity it reads from the
//! config, the groups it shares with v1, and how it follows the settings.

use serde_json::json;

use super::*;
use crate::api::instance::test_support::{TEST_VAPID_KEY, empty_document, keys_of, pleroma_block};

/// The empty-instance v2 document on the test config: zeroed usage,
/// no rules, no contact.
fn empty_document_v2() -> Value {
    let config = Config::for_tests();
    serde_json::to_value(InstanceV2::build(
        &config,
        &[],
        None,
        TEST_VAPID_KEY,
        pleroma_block(&config),
    ))
    .unwrap()
}

#[test]
fn the_v2_document_holds_the_ticket_key_sets() {
    let document = empty_document_v2();
    assert_eq!(
        keys_of(&document),
        vec![
            "configuration",
            "contact",
            "description",
            "domain",
            "languages",
            "pleroma",
            "registrations",
            "rules",
            "source_url",
            "thumbnail",
            "title",
            "usage",
            "version",
        ],
        "the ticket's v2 top-level key set"
    );
    assert_eq!(
        keys_of(&document["configuration"]),
        vec![
            "accounts",
            "media_attachments",
            "polls",
            "statuses",
            "translation",
            "urls",
            "vapid",
        ],
        "the v2 configuration groups"
    );
    assert_eq!(
        keys_of(&document["registrations"]),
        vec!["approval_required", "enabled", "message", "url"],
        "the nested registration block"
    );
    assert_eq!(
        keys_of(&document["contact"]),
        vec!["account", "email"],
        "the nested contact block"
    );
    assert_eq!(
        keys_of(&document["usage"]),
        vec!["users"],
        "the usage block counts users"
    );
    assert_eq!(
        keys_of(&document["usage"]["users"]),
        vec!["active_month"],
        "the monthly-active counter"
    );
    assert_eq!(
        keys_of(&document["thumbnail"]),
        vec!["url"],
        "v2 wraps the thumbnail URL"
    );
}

#[test]
fn the_v2_document_holds_the_instance_identity() {
    let document = empty_document_v2();
    assert_eq!(document["domain"], "test.example", "the config domain");
    assert_eq!(document["title"], "Wired", "the config name");
    assert_eq!(
        document["description"], "A Wired instance, compatible with Pleroma clients",
        "the config description"
    );
    assert_eq!(
        document["source_url"], SOURCE_URL,
        "the shipped repository URL"
    );
    assert_eq!(
        document["source_url"],
        "https://github.com/TheStranjer/wired-be"
    );
    assert_eq!(
        document["thumbnail"]["url"], "https://test.example/images/thumbnail.jpg",
        "the wrapped thumbnail URL"
    );
    assert_eq!(document["languages"], json!(["en"]), "the config languages");
    assert_eq!(
        document["usage"]["users"]["active_month"], 0,
        "no activity counter exists yet"
    );
    assert_eq!(document["rules"], json!([]), "no rules stored");
}

#[test]
fn the_v2_configuration_groups_hold_their_defaults() {
    let document = empty_document_v2();
    assert_eq!(
        document["configuration"]["urls"],
        json!({"streaming": "wss://test.example", "status": null}),
        "the streaming origin and no status page"
    );
    assert_eq!(
        document["configuration"]["vapid"],
        json!({"public_key": TEST_VAPID_KEY}),
        "the v2 configuration advertises the instance VAPID key"
    );
    assert_eq!(
        document["configuration"]["accounts"],
        json!({"max_featured_tags": 10, "max_pinned_statuses": 5}),
        "Mastodon's shipped account limits"
    );
    assert_eq!(
        document["configuration"]["statuses"],
        json!({"max_characters": 5000, "max_media_attachments": 4, "characters_reserved_per_url": 23}),
        "the status limits mirror the config"
    );
    assert_eq!(
        document["configuration"]["translation"],
        json!({"enabled": false}),
        "no translation provider is configured"
    );
    let v1 = empty_document();
    for group in ["media_attachments", "polls"] {
        assert_eq!(
            document["configuration"][group], v1["configuration"][group],
            "the {group} group is the shared v1 builder, unchanged"
        );
    }
    assert_eq!(
        document["registrations"],
        json!({"enabled": true, "approval_required": false, "message": null, "url": null}),
        "the config toggle nested with the two never-set extras"
    );
    assert_eq!(
        document["contact"],
        json!({"email": "", "account": null}),
        "an unconfigured email is empty, no admin fixture"
    );
    assert_eq!(
        document["pleroma"], v1["pleroma"],
        "the pleroma block is the shared v1 builder, unchanged"
    );
}

#[test]
fn the_v2_document_follows_the_config() {
    let mut config = Config::for_tests();
    config.instance_name = "Snatter".to_owned();
    config.instance_description = "Where we all chatter".to_owned();
    config.contact_email = Some("admin@example.org".to_owned());
    config.instance_languages = vec!["en".to_owned(), "de".to_owned()];
    config.base_url = "http://snatter.example:4000".to_owned();
    config.frontend_base_url = "http://snatter.example:4000".to_owned();
    config.max_status_chars = 300;
    config.max_media_attachments = 6;
    config.max_poll_options = 12;
    config.upload_limits.max_file_bytes = 1_048_576;
    config.registrations_open = Toggle::new(false);
    let document = serde_json::to_value(InstanceV2::build(
        &config,
        &[],
        None,
        TEST_VAPID_KEY,
        pleroma_block(&config),
    ))
    .unwrap();
    assert_eq!(
        document["domain"], "snatter.example:4000",
        "the domain with port"
    );
    assert_eq!(document["title"], "Snatter");
    assert_eq!(document["description"], "Where we all chatter");
    assert_eq!(document["languages"], json!(["en", "de"]));
    assert_eq!(
        document["thumbnail"]["url"],
        "http://snatter.example:4000/images/thumbnail.jpg"
    );
    assert_eq!(
        document["configuration"]["urls"]["streaming"],
        "ws://snatter.example:4000"
    );
    assert_eq!(
        document["configuration"]["statuses"]["max_characters"], 300,
        "the shared status group mirrors the config"
    );
    assert_eq!(
        document["configuration"]["media_attachments"]["image_size_limit"], 1_048_576,
        "the shared media group mirrors the upload limit"
    );
    assert_eq!(
        document["configuration"]["polls"]["max_options"], 12,
        "the shared poll group mirrors the option count"
    );
    assert_eq!(
        document["registrations"]["enabled"], false,
        "REGISTRATIONS_OPEN=false is advertised"
    );
    assert_eq!(document["contact"]["email"], "admin@example.org");
}
