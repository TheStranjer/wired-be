//! The v1 document's tests: its key sets, the identity and limit values it
//! reads from the config, the registration modes, and the rules it renders.

use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::api::instance::{
    test_support::{TEST_VAPID_KEY, empty_document, keys_of, pleroma_block, stamped},
    v2::InstanceV2,
};

/// A stored rule with `id` text `n` for rendering tests.
fn rule(id: &str, text: &str, hint: &str, priority: i32) -> InstanceRule {
    InstanceRule {
        id: Uuid::parse_str(id).unwrap(),
        text: text.to_owned(),
        hint: hint.to_owned(),
        priority,
        inserted_at: stamped(),
        updated_at: stamped(),
    }
}

#[test]
fn the_document_holds_the_ticket_top_level_key_set() {
    let document = empty_document();
    assert_eq!(
        keys_of(&document),
        vec![
            "approval_required",
            "avatar_upload_limit",
            "background_upload_limit",
            "banner_upload_limit",
            "configuration",
            "contact_account",
            "description",
            "description_limit",
            "email",
            "invites_enabled",
            "languages",
            "max_media_attachments",
            "max_toot_chars",
            "pleroma",
            "poll_limits",
            "registrations",
            "rules",
            "short_description",
            "stats",
            "thumbnail",
            "title",
            "upload_limit",
            "uri",
            "urls",
            "version",
        ],
        "the ticket's top-level key set"
    );
    assert_eq!(
        keys_of(&document["stats"]),
        vec!["domain_count", "status_count", "user_count"],
        "the stats block"
    );
    assert_eq!(
        keys_of(&document["poll_limits"]),
        vec![
            "max_expiration",
            "max_option_chars",
            "max_options",
            "min_expiration"
        ],
        "the poll limits block"
    );
}

#[test]
fn the_document_holds_the_ticket_nested_key_sets() {
    let document = empty_document();
    assert_eq!(
        keys_of(&document["configuration"]),
        vec!["accounts", "media_attachments", "polls", "statuses"],
        "the configuration groups"
    );
    assert_eq!(
        keys_of(&document["configuration"]["media_attachments"]),
        vec![
            "image_matrix_limit",
            "image_size_limit",
            "supported_mime_types",
            "video_frame_rate_limit",
            "video_matrix_limit",
            "video_size_limit",
        ],
        "the media limits"
    );
    assert_eq!(
        keys_of(&document["pleroma"]),
        vec!["metadata", "stats", "vapid_public_key"],
        "the pleroma block"
    );
    assert_eq!(
        keys_of(&document["pleroma"]["metadata"]),
        vec![
            "account_activation_required",
            "birthday_min_age",
            "birthday_required",
            "features",
            "federation",
            "fields_limits",
            "markup",
            "post_formats",
            "restrict_unauthenticated",
        ],
        "the pleroma metadata block"
    );
    assert_eq!(
        keys_of(&document["pleroma"]["metadata"]["markup"]),
        vec![
            "allow_fonts",
            "allow_headings",
            "allow_inline_images",
            "allow_tables",
        ],
        "the markup keys keep Pleroma's allow_ prefix on the wire"
    );
}

#[test]
fn the_empty_document_holds_the_instance_identity() {
    let document = empty_document();
    assert_eq!(document["uri"], "test.example", "the config domain");
    assert_eq!(document["title"], "Wired", "the config name");
    assert_eq!(
        document["short_description"], document["description"],
        "both descriptions are the one config setting"
    );
    assert_eq!(document["email"], "", "an unconfigured email is empty");
    assert_eq!(
        document["thumbnail"],
        "https://test.example/images/thumbnail.jpg"
    );
    assert_eq!(document["languages"], json!(["en"]), "the config languages");
    assert_eq!(document["registrations"], true, "open by default");
    assert_eq!(
        document["approval_required"], false,
        "REGISTRATIONS_APPROVAL_REQUIRED is off by default"
    );
    assert_eq!(
        document["invites_enabled"], false,
        "INVITES_ENABLED is off by default"
    );
    assert_eq!(document["contact_account"], Value::Null, "no admin fixture");
    assert_eq!(document["rules"], json!([]), "no rules stored");
    assert_eq!(
        document["stats"],
        json!({"user_count": 0, "status_count": 0, "domain_count": 0}),
        "the zeroed counters"
    );
}

/// Both documents' thumbnails hang under `FRONTEND_BASE_URL`, where the
/// frontend ships the file, not under `BASE_URL`; the identity tests
/// above pin the unchanged URL for the default config, where the
/// variable is unset and the two origins are equal.
#[test]
fn the_thumbnail_hangs_off_the_frontend_origin() {
    let mut config = Config::for_tests();
    config.frontend_base_url = "https://fe.example".to_owned();
    let stats = InstanceStats {
        users: 0,
        statuses: 0,
        domains: 0,
        active_month: 0,
        active_halfyear: 0,
    };
    let v1 = serde_json::to_value(Instance::build(
        &config,
        &stats,
        &[],
        None,
        pleroma_block(&config),
    ))
    .unwrap();
    let thumbnail = v1["thumbnail"].as_str().unwrap();
    assert!(
        thumbnail.starts_with("https://fe.example") && thumbnail.ends_with(THUMBNAIL_PATH),
        "the v1 thumbnail follows FRONTEND_BASE_URL: {thumbnail}"
    );
    let v2 = serde_json::to_value(InstanceV2::build(
        &config,
        &[],
        None,
        TEST_VAPID_KEY,
        pleroma_block(&config),
    ))
    .unwrap();
    let wrapped = v2["thumbnail"]["url"].as_str().unwrap();
    assert!(
        wrapped.starts_with("https://fe.example") && wrapped.ends_with(THUMBNAIL_PATH),
        "the v2 thumbnail block follows FRONTEND_BASE_URL: {wrapped}"
    );
}

/// `invites_enabled` is the `INVITES_ENABLED` setting, on both sides
/// of its default.
#[test]
fn invites_enabled_mirrors_the_config() {
    let stats = InstanceStats {
        users: 0,
        statuses: 0,
        domains: 0,
        active_month: 0,
        active_halfyear: 0,
    };
    let mut config = Config::for_tests();
    config.invites_enabled = Toggle::new(true);
    let on = serde_json::to_value(Instance::build(
        &config,
        &stats,
        &[],
        None,
        pleroma_block(&config),
    ))
    .unwrap();
    assert_eq!(
        on["invites_enabled"], true,
        "INVITES_ENABLED=true is advertised"
    );
    config.invites_enabled = Toggle::new(false);
    let off = serde_json::to_value(Instance::build(
        &config,
        &stats,
        &[],
        None,
        pleroma_block(&config),
    ))
    .unwrap();
    assert_eq!(off["invites_enabled"], false, "and the off state stays off");
}

/// The two registration modes are the two settings, in all three places
/// they are advertised: `approval_required` on `GET /api/v1/instance`,
/// `registrations.approval_required` on `/api/v2/instance` and
/// `pleroma.metadata.account_activation_required` on both documents.
#[test]
fn registration_modes_mirror_the_config() {
    let stats = InstanceStats {
        users: 0,
        statuses: 0,
        domains: 0,
        active_month: 0,
        active_halfyear: 0,
    };
    let mut config = Config::for_tests();
    for (approval, activation) in [(false, false), (true, false), (false, true), (true, true)] {
        config.registrations_approval_required = Toggle::new(approval);
        config.account_activation_required = Toggle::new(activation);
        let v1 = serde_json::to_value(Instance::build(
            &config,
            &stats,
            &[],
            None,
            pleroma_block(&config),
        ))
        .unwrap();
        let v2 = serde_json::to_value(InstanceV2::build(
            &config,
            &[],
            None,
            TEST_VAPID_KEY,
            pleroma_block(&config),
        ))
        .unwrap();
        assert_eq!(
            v1["approval_required"], approval,
            "v1 approval_required follows REGISTRATIONS_APPROVAL_REQUIRED={approval}"
        );
        assert_eq!(
            v1["pleroma"]["metadata"]["account_activation_required"], activation,
            "v1 metadata follows ACCOUNT_ACTIVATION_REQUIRED={activation}"
        );
        assert_eq!(
            v2["registrations"]["approval_required"], approval,
            "v2 follows the approval setting too"
        );
        assert_eq!(
            v2["pleroma"]["metadata"]["account_activation_required"], activation,
            "and the shared pleroma block reports activation on v2"
        );
    }
}

#[test]
fn the_limit_blocks_hold_their_defaults() {
    let document = empty_document();
    assert_eq!(document["max_toot_chars"], 5000, "the config default");
    assert_eq!(document["max_media_attachments"], 4, "the config default");
    assert_eq!(document["upload_limit"], 16_000_000, "the config default");
    assert_eq!(
        document["avatar_upload_limit"], 2_000_000,
        "Pleroma's default"
    );
    assert_eq!(
        document["background_upload_limit"], 4_000_000,
        "Pleroma's default"
    );
    assert_eq!(
        document["banner_upload_limit"], 4_000_000,
        "Pleroma's default"
    );
    assert_eq!(document["description_limit"], 5000, "Pleroma's default");
    assert_eq!(
        document["poll_limits"],
        json!({"max_options": 20, "max_option_chars": 200, "min_expiration": 0, "max_expiration": 31_536_000}),
        "the configured option count with Pleroma's window"
    );
    assert_eq!(
        document["configuration"]["statuses"],
        json!({"max_characters": 5000, "max_media_attachments": 4, "characters_reserved_per_url": 23}),
        "the status limits mirror the config"
    );
    assert_eq!(
        document["configuration"]["media_attachments"],
        json!({
            "supported_mime_types": ["application/octet-stream"],
            "image_size_limit": 16_000_000,
            "video_size_limit": 16_000_000,
            "image_matrix_limit": 16_777_216,
            "video_matrix_limit": 2_304_000,
            "video_frame_rate_limit": 60,
        }),
        "the upload limits mirror the config"
    );
    assert_eq!(
        document["configuration"]["polls"],
        json!({"max_options": 20, "max_characters_per_option": 200, "min_expiration": 0, "max_expiration": 31_536_000}),
        "the poll limits appear under Mastodon's names too"
    );
    assert_eq!(
        document["configuration"]["accounts"],
        json!({"max_featured_tags": 10}),
        "Mastodon's shipped featured-tag limit"
    );
}

#[test]
fn limits_and_identity_follow_the_config() {
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
    let stats = InstanceStats {
        users: 7,
        statuses: 11,
        domains: 3,
        active_month: 2,
        active_halfyear: 4,
    };
    let document = serde_json::to_value(Instance::build(
        &config,
        &stats,
        &[],
        None,
        pleroma_block(&config),
    ))
    .unwrap();
    assert_eq!(
        document["uri"], "snatter.example:4000",
        "the domain with port"
    );
    assert_eq!(document["title"], "Snatter");
    assert_eq!(document["description"], "Where we all chatter");
    assert_eq!(document["email"], "admin@example.org");
    assert_eq!(document["languages"], json!(["en", "de"]));
    assert_eq!(
        document["urls"]["streaming_api"],
        "ws://snatter.example:4000"
    );
    assert_eq!(
        document["thumbnail"],
        "http://snatter.example:4000/images/thumbnail.jpg"
    );
    assert_eq!(document["registrations"], false);
    assert_eq!(document["max_toot_chars"], 300);
    assert_eq!(document["max_media_attachments"], 6);
    assert_eq!(document["poll_limits"]["max_options"], 12);
    assert_eq!(document["upload_limit"], 1_048_576);
    assert_eq!(document["configuration"]["statuses"]["max_characters"], 300);
    assert_eq!(
        document["configuration"]["media_attachments"]["image_size_limit"],
        1_048_576
    );
    assert_eq!(
        document["stats"],
        json!({"user_count": 7, "status_count": 11, "domain_count": 3})
    );
}

#[test]
fn rules_render_id_text_and_hint() {
    let rules = vec![
        rule(
            "01934567-89ab-7cde-f012-3456789abcde",
            "Be nice",
            "say hi",
            0,
        ),
        rule("01934567-89ab-7cde-f012-3456789abcdf", "No spam", "", 1),
    ];
    let document = serde_json::to_value(empty_document_with_rules(&rules)).unwrap();
    assert_eq!(
        document["rules"],
        json!([
            {"id": "01934567-89ab-7cde-f012-3456789abcde", "text": "Be nice", "hint": "say hi"},
            {"id": "01934567-89ab-7cde-f012-3456789abcdf", "text": "No spam", "hint": ""},
        ]),
        "ids arrive as the hyphenated lowercase strings (decision 4)"
    );
}

/// The empty-instance document carrying `rules`.
fn empty_document_with_rules(rules: &[InstanceRule]) -> Instance {
    let config = Config::for_tests();
    let stats = InstanceStats {
        users: 0,
        statuses: 0,
        domains: 0,
        active_month: 0,
        active_halfyear: 0,
    };
    Instance::build(&config, &stats, rules, None, pleroma_block(&config))
}
