//! The unit-test fixtures the account children share: the parsed
//! profile-write bodies, the stored row the merge helpers measure
//! against, the image bytes and file parts, and the two accounts the
//! profile-gate tests read.

use bytes::Bytes;
use chrono::{NaiveDate, Utc};
use serde_json::json;
use uuid::Uuid;

use crate::{
    config::Config,
    extract::multipart::UploadedFile,
    models::user::{self, DEFAULT_ACTOR_TYPE, Flag, User},
};

use super::credentials::ProfileParams;

/// The parameters of a body, parsed the way the extractor hands them
/// over: `json!` text for the JSON spellings, a map of strings for the
/// form and multipart ones.
pub(super) fn params(text: &str) -> ProfileParams {
    serde_json::from_str(text).expect("the fixture body parses")
}

/// The empty body, for the tests that only pass a file part.
pub(super) fn no_params() -> ProfileParams {
    params("{}")
}

/// The stored row the merge helpers are tested against: the column
/// defaults, with a few values set apart — `private`, a tag, a
/// two-key settings blob, one date, and two on-by-default flags — so a
/// merge that ignored the stored state cannot pass by accident.
pub(super) fn stored_user() -> User {
    User {
        id: Uuid::now_v7(),
        nickname: "alice".to_owned(),
        ap_id: "https://test.example/users/alice".to_owned(),
        local: Flag::new(true),
        domain: None,
        actor_type: DEFAULT_ACTOR_TYPE.to_owned(),
        email: None,
        password_hash: None,
        name: String::new(),
        bio: String::new(),
        raw_bio: String::new(),
        avatar_url: None,
        banner_url: None,
        profile_url: None,
        locked: Flag::new(false),
        bot: Flag::new(false),
        discoverable: Flag::new(false),
        fields: json!([]),
        raw_fields: json!([]),
        is_admin: Flag::new(false),
        is_moderator: Flag::new(false),
        is_active: Flag::new(true),
        hide_followers: Flag::new(true),
        hide_follows: Flag::new(false),
        hide_followers_count: Flag::new(false),
        hide_follows_count: Flag::new(false),
        hide_favorites: Flag::new(true),
        allow_following_move: Flag::new(true),
        skip_thread_containment: Flag::new(false),
        accepts_chat_messages: Flag::new(true),
        show_role: Flag::new(true),
        no_rich_text: Flag::new(false),
        default_scope: "private".to_owned(),
        sensitive_default: Flag::new(false),
        language: Some("pt-BR".to_owned()),
        tags: Vec::new(),
        settings_store: json!({"theme": "dark", "locale": "pt"}),
        notification_settings: json!({}),
        background_image_url: None,
        birthday: Some(NaiveDate::from_ymd_opt(1990, 1, 31).expect("a real day")),
        show_birthday: Flag::new(false),
        also_known_as: Vec::new(),
        moved_to_ap_id: None,
        last_move_at: None,
        email_confirmed: Flag::new(true),
        approval_pending: Flag::new(false),
        registration_reason: None,
        confirmation_token: None,
        confirmation_sent_at: None,
        last_active_at: None,
        followers_count: 0,
        following_count: 0,
        statuses_count: 0,
        last_status_at: None,
        inserted_at: Utc::now(),
        updated_at: Utc::now(),
        mfa_settings: json!({}),
        is_suggested: Flag::new(false),
        password_reset_pending: Flag::new(false),
        email_notifications: Flag::new(true),
        deleted_at: None,
        suspended_at: None,
        disabled_at: None,
        remote_suspended: Flag::new(false),
    }
}

/// Magic-byte fixtures for the image slots: a header is all [`infer`]
/// reads, and the endpoint never decodes a profile image.
pub(super) const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
/// A `JPEG`: the `SOI` marker `infer` matches on.
pub(super) const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01\x01\x00\x00\x01";
/// A `GIF`: the versioned header, `GIF89a`.
pub(super) const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00";
/// A `WebP`: the `RIFF` container with the `WEBP` and `VP8` tags.
pub(super) const WEBP: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";
/// A `BMP`: an image, just not one of the four the endpoint accepts.
pub(super) const BMP: &[u8] = b"BM\x36\x00\x00\x00\x00\x00\x36\x00\x00\x00";
/// A `PDF`: bytes with a type that is not an image at all.
pub(super) const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";
/// Text: bytes no matcher can name.
pub(super) const TEXT: &[u8] = b"a plain text file, definitely not an image\n";

/// A file part named `field` holding `bytes`, as the multipart extractor
/// hands one to the endpoint. The client's claimed `content_type` is
/// `None` where the test does not care: what is stored never comes from
/// it.
pub(super) fn part(field: &str, bytes: &[u8]) -> UploadedFile {
    UploadedFile {
        field: field.to_owned(),
        filename: Some("picture.bmp".to_owned()),
        content_type: Some("image/png".to_owned()),
        bytes: Bytes::copy_from_slice(bytes),
    }
}

/// The local and the remote account the area and gate tests read.
pub(super) async fn local_and_remote() -> (crate::test_db::TestDb, User, User) {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let local = user::insert(
        db.pool(),
        &user::NewUser::local(
            "alice",
            &user::local_ap_id(crate::config::TEST_BASE_URL, "alice"),
        ),
    )
    .await
    .expect("the local fixture inserts");
    let remote = user::insert(
        db.pool(),
        &user::NewUser {
            nickname: "bob@remote.example".to_owned(),
            ap_id: "https://remote.example/users/bob".to_owned(),
            domain: Some("remote.example".to_owned()),
            ..user::NewUser::default()
        },
    )
    .await
    .expect("the remote fixture inserts");
    (db, local, remote)
}

/// The config restricting exactly the areas in `areas`.
pub(super) fn config_restricting(areas: &[&str]) -> Config {
    let mut config = Config::for_tests();
    config.restrict_unauthenticated = areas.iter().map(|area| (*area).to_owned()).collect();
    config
}
