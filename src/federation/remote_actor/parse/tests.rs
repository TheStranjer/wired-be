//! Tests for [`parse_actor`]: the fixtures, the defaults and every rejection.

use serde_json::{Value, json};

use super::*;
use crate::{
    federation::{
        note_parser::RemoteEmoji,
        remote_actor::{
            MAX_ALIASES, MAX_FIELD_NAME_CHARS, MAX_FIELD_VALUE_CHARS, MAX_REMOTE_EMOJIS,
            MAX_REMOTE_FIELDS,
            test_support::{MASTODON, PLEROMA, fixture, remove},
        },
    },
    util::time::parse_time,
};

#[test]
fn parses_a_mastodon_style_actor() {
    let document = fixture(MASTODON);
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(
        parsed.nickname, "alice@mastodon.example",
        "preferredUsername@host of id"
    );
    assert_eq!(
        parsed.ap_id, "https://mastodon.example/users/alice",
        "ap_id"
    );
    assert!(!parsed.local.get(), "a remote mirror is never local");
    assert_eq!(parsed.domain, "mastodon.example", "the id's host");
    assert_eq!(parsed.actor_type, "Person", "the actor type");
    assert_eq!(parsed.name, "Alice \u{1f98a}", "the display name");
    assert!(
        parsed
            .bio
            .contains(r#"<a href="https://alice.example" rel="ugc nofollow noopener noreferrer">"#),
        "the link survives with the forced rel, got {}",
        parsed.bio
    );
    assert!(!parsed.bio.contains("target="), "target is stripped");
    assert!(
        !parsed.bio.contains("steal"),
        "script contents are stripped"
    );
    assert_eq!(
        parsed.raw_bio, "A curator of https://alice.example/feed\nBio & notes \n",
        "the source note is the stripped text with p-ends as newlines"
    );
    assert!(!parsed.locked.get(), "manuallyApprovesFollowers: false");
    assert!(parsed.discoverable.get(), "discoverable: true");
    assert_eq!(
        parsed.public_key,
        document["publicKey"]["publicKeyPem"].as_str().unwrap(),
        "the PEM as sent"
    );
    assert_eq!(
        parsed.also_known_as,
        ["https://old.example/users/alice"],
        "alsoKnownAs"
    );
    assert_eq!(parsed.moved_to, None, "no movedTo in the document");
    assert!(
        !parsed.accepts_chat_messages.get(),
        "no capabilities object"
    );
    assert_eq!(
        parsed.published,
        Some(parse_time("2022-01-31T12:34:56Z").unwrap()),
        "published"
    );
}

#[test]
fn an_actor_id_on_an_explicit_port_keeps_it_in_the_handle() {
    // The handle the mirror stores is the one a `WebFinger` lookup was
    // asked for: `preferredUsername@host:port`, port included (and the
    // scheme's default port folded away).
    let mut document = fixture(MASTODON);
    document["id"] = json!("https://mastodon.example:8443/users/alice");
    document["publicKey"]["owner"] = json!("https://mastodon.example:8443/users/alice");
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(
        parsed.nickname, "alice@mastodon.example:8443",
        "the non-default port belongs to the host"
    );
    assert_eq!(parsed.domain, "mastodon.example:8443", "and the domain");

    document["id"] = json!("https://mastodon.example:443/users/alice");
    document["publicKey"]["owner"] = json!("https://mastodon.example:443/users/alice");
    let default = parse_actor(&document).unwrap();
    assert_eq!(
        default.nickname, "alice@mastodon.example",
        "a spelled-out default port carries nothing"
    );
}

#[test]
fn a_mastodon_actor_fills_the_endpoint_and_media_columns() {
    let parsed = parse_actor(&fixture(MASTODON)).unwrap();
    assert_eq!(
        parsed.avatar_url.as_deref(),
        Some("https://mastodon.example/avatar/alice.png"),
        "icon object url"
    );
    assert_eq!(
        parsed.banner_url.as_deref(),
        Some("https://mastodon.example/banner/alice.png"),
        "image object url"
    );
    assert_eq!(
        parsed.profile_url.as_deref(),
        Some("https://mastodon.example/@alice"),
        "url"
    );
    assert_eq!(
        parsed.inbox, "https://mastodon.example/users/alice/inbox",
        "inbox"
    );
    assert_eq!(
        parsed.shared_inbox.as_deref(),
        Some("https://mastodon.example/inbox"),
        "endpoints.sharedInbox"
    );
    assert_eq!(
        parsed.outbox.as_deref(),
        Some("https://mastodon.example/users/alice/outbox"),
        "outbox"
    );
    assert_eq!(
        parsed.followers_url.as_deref(),
        Some("https://mastodon.example/users/alice/followers"),
        "followers"
    );
    assert_eq!(
        parsed.following_url.as_deref(),
        Some("https://mastodon.example/users/alice/following"),
        "following"
    );
    assert_eq!(
        parsed.featured_url.as_deref(),
        Some("https://mastodon.example/users/alice/collections/featured"),
        "featured"
    );
    assert_eq!(
        parsed.fields,
        json!([
            {"name": "Site", "value": "<a href=\"https://alice.example\" rel=\"nofollow noopener noreferrer\" target=\"_blank\">alice.example</a>"},
            {"name": "Pronouns", "value": "she/her"},
        ]),
        "the two PropertyValues with name and value only; the typeless entry skipped"
    );
}

#[test]
fn parses_a_pleroma_style_actor() {
    let document = fixture(PLEROMA);
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(
        parsed.nickname, "bob.the-bot@pleroma.example",
        "dots are legal in the handle alphabet"
    );
    assert_eq!(
        parsed.ap_id, "https://pleroma.example/users/bob.the-bot",
        "the id is kept verbatim"
    );
    assert_eq!(parsed.domain, "pleroma.example", "the id's host");
    assert_eq!(parsed.actor_type, "Service", "the bot actor type");
    assert!(parsed.locked.get(), "manuallyApprovesFollowers: true");
    assert!(
        !parsed.discoverable.get(),
        "an absent discoverable defaults to false"
    );
    assert_eq!(
        parsed.profile_url.as_deref(),
        Some("https://pleroma.example/users/bob.the-bot"),
        "url given as an array of Link objects yields the first href"
    );
    assert_eq!(
        parsed.outbox.as_deref(),
        Some("https://pleroma.example/users/bob.the-bot/outbox"),
        "outbox given as the collection object yields its id"
    );
    assert_eq!(
        parsed.avatar_url.as_deref(),
        Some("https://pleroma.example/images/bob/avatar.png"),
        "icon: array -> Image object -> url array -> Link href"
    );
    assert_eq!(
        parsed.banner_url.as_deref(),
        Some("https://pleroma.example/images/bob/banner.png"),
        "image given as a bare string"
    );
    assert_eq!(
        parsed.public_key,
        document["publicKey"][0]["publicKeyPem"].as_str().unwrap(),
        "publicKey given as an array"
    );
    assert!(
        parsed.bio.contains(r#"<span class="h-card">"#),
        "h-card is an allowed span class, got {}",
        parsed.bio
    );
    assert!(!parsed.bio.contains("alert"), "the script body is gone");
    assert!(!parsed.bio.contains("p{}"), "the style body is gone");
    assert_eq!(
        parsed.raw_bio, "Hi @alice\nbot here\nbye\n",
        "the note as text"
    );
    assert_eq!(
        parsed.fields,
        json!([{"name": "web", "value": "https://bob.example"}]),
        "only the PropertyValue entry is kept"
    );
    assert_eq!(
        parsed.also_known_as,
        ["https://old.example/users/bob"],
        "alsoKnownAs given as a single string"
    );
    assert_eq!(
        parsed.moved_to.as_deref(),
        Some("https://new.example/users/bob.the-bot"),
        "movedTo"
    );
    assert!(parsed.accepts_chat_messages.get(), "capabilities flag");
    assert_eq!(
        parsed.published,
        Some(parse_time("2019-06-01T00:00:00.000Z").unwrap()),
        "the published timestamp parses"
    );
}

#[test]
fn parse_actor_records_the_public_key_id() {
    let document = fixture(MASTODON);
    assert_eq!(
        parse_actor(&document).unwrap().public_key_id.as_deref(),
        Some("https://mastodon.example/users/alice#main-key"),
        "a fragment key id is kept whole"
    );

    let array = fixture(PLEROMA);
    assert_eq!(
        parse_actor(&array).unwrap().public_key_id.as_deref(),
        Some("https://pleroma.example/users/bob.the-bot#main-key"),
        "an array publicKey is read from its first element"
    );

    let mut path_style = fixture(MASTODON);
    path_style["publicKey"]["id"] = json!("https://mastodon.example/users/alice/main-key");
    assert_eq!(
        parse_actor(&path_style).unwrap().public_key_id.as_deref(),
        Some("https://mastodon.example/users/alice/main-key"),
        "a GoToSocial path-style key id is kept"
    );

    let mut reference = fixture(MASTODON);
    reference["publicKey"]["id"] = json!({"id": "https://mastodon.example/keys/1"});
    assert_eq!(
        parse_actor(&reference).unwrap().public_key_id.as_deref(),
        Some("https://mastodon.example/keys/1"),
        "a reference object spelling resolves through value_or_id"
    );
}

#[test]
fn parse_actor_drops_a_public_key_id_it_cannot_trust() {
    let mut missing = fixture(MASTODON);
    remove(&mut missing["publicKey"], "id");
    assert_eq!(
        parse_actor(&missing).unwrap().public_key_id,
        None,
        "no key id is no key id, and the parse still succeeds"
    );

    for (case, key_id) in [
        ("another host", "https://evil.example/users/alice#main-key"),
        ("not a URL", "main-key"),
        ("not http(s)", "mailto:alice@mastodon.example"),
    ] {
        let mut document = fixture(MASTODON);
        document["publicKey"]["id"] = json!(key_id);
        assert_eq!(
            parse_actor(&document).unwrap().public_key_id,
            None,
            "{case} is dropped"
        );
    }
}

#[test]
fn a_pem_with_trailing_blank_lines_is_stored_with_one_newline() {
    let body = "-----BEGIN PUBLIC KEY-----\nQUJD\n-----END PUBLIC KEY-----";
    for (label, sent) in [
        ("Pleroma's double newline", format!("{body}\n\n")),
        (
            "CRLF endings",
            format!("{}\r\n", body.replace('\n', "\r\n")),
        ),
        ("surrounding spaces", format!("  {body} \n\n")),
        ("no final newline", body.to_owned()),
    ] {
        let mut document = fixture(MASTODON);
        document["publicKey"]["publicKeyPem"] = json!(sent);
        let parsed = parse_actor(&document).unwrap();
        assert_eq!(
            parsed.public_key,
            format!("{body}\n"),
            "{label}: stored with exactly one trailing newline"
        );
    }
}

#[test]
fn a_blank_pem_is_rejected() {
    let mut document = fixture(MASTODON);
    document["publicKey"]["publicKeyPem"] = json!(" \n\n");
    let outcome = parse_actor(&document);
    assert!(
        matches!(&outcome, Err(ActorError::Invalid(field)) if field.contains("publicKeyPem")),
        "whitespace alone is no key"
    );
}

#[test]
fn a_minimal_actor_gets_the_documented_defaults() {
    let document = json!({
        "id": "https://remote.example/users/carol",
        "preferredUsername": "carol",
        "inbox": "https://remote.example/users/carol/inbox",
        "publicKey": {
            "owner": "https://remote.example/users/carol",
            "publicKeyPem": "PEM-without-an-owner",
        },
    });
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(
        parsed.nickname, "carol@remote.example",
        "the handle is username@host"
    );
    assert_eq!(
        parsed.actor_type, DEFAULT_ACTOR_TYPE,
        "a missing type is Person"
    );
    assert_eq!(parsed.name, "", "no display name");
    assert_eq!(parsed.bio, "", "no note");
    assert_eq!(parsed.raw_bio, "", "no source note");
    assert_eq!(
        parsed.public_key, "PEM-without-an-owner\n",
        "owner may be absent; the PEM is normalized to one trailing newline"
    );
    assert_eq!(parsed.fields, json!([]), "no attachments");
    assert!(parsed.also_known_as.is_empty(), "no aliases");
    assert_eq!(parsed.published, None, "no creation time");
    for optional in [
        parsed.avatar_url,
        parsed.banner_url,
        parsed.profile_url,
        parsed.shared_inbox,
        parsed.outbox,
        parsed.followers_url,
        parsed.following_url,
        parsed.featured_url,
        parsed.moved_to,
    ] {
        assert_eq!(optional, None, "absent optional fields are None");
    }
    assert!(
        !parsed.locked.get() && !parsed.discoverable.get(),
        "flags default to false"
    );
    assert!(
        !parsed.accepts_chat_messages.get(),
        "no capabilities object"
    );
    assert!(!parsed.local.get(), "local is always false");
}

/// One rejection case: what to call it, how to break a copy of the
/// Mastodon fixture, and a fragment of the field name the error must
/// carry.
type Rejection = (&'static str, fn(&mut Value), &'static str);

/// Documents built from the Mastodon fixture by breaking exactly one
/// required rule each; none of them may parse.
fn rejections() -> Vec<Rejection> {
    vec![
        ("no id at all", |doc| remove(doc, "id"), "`id`"),
        (
            "relative id",
            |doc| doc["id"] = json!("/users/alice"),
            "`id`",
        ),
        (
            "non-http id",
            |doc| doc["id"] = json!("ftp://mastodon.example/users/alice"),
            "`id`",
        ),
        (
            "array id",
            |doc| doc["id"] = json!(["https://mastodon.example/users/alice"]),
            "`id`",
        ),
        ("no inbox", |doc| remove(doc, "inbox"), "`inbox`"),
        (
            "mailto inbox",
            |doc| doc["inbox"] = json!("mailto:alice@mastodon.example"),
            "`inbox`",
        ),
        (
            "no username",
            |doc| remove(doc, "preferredUsername"),
            "preferredUsername",
        ),
        (
            "empty username",
            |doc| doc["preferredUsername"] = json!(""),
            "preferredUsername",
        ),
        (
            "username with a space",
            |doc| doc["preferredUsername"] = json!("alice smith"),
            "preferredUsername",
        ),
        (
            "username with an at-sign",
            |doc| doc["preferredUsername"] = json!("alice@mastodon.example"),
            "preferredUsername",
        ),
        (
            "non-ascii username",
            |doc| doc["preferredUsername"] = json!("müller"),
            "preferredUsername",
        ),
        (
            "unknown actor type",
            |doc| doc["type"] = json!("Spaceship"),
            "`type`",
        ),
        ("non-string type", |doc| doc["type"] = json!(7), "`type`"),
        (
            "no publicKey",
            |doc| remove(doc, "publicKey"),
            "`publicKey`",
        ),
        (
            "empty publicKey array",
            |doc| doc["publicKey"] = json!([]),
            "`publicKey`",
        ),
        (
            "no PEM",
            |doc| remove(&mut doc["publicKey"], "publicKeyPem"),
            "publicKeyPem",
        ),
        (
            "empty PEM",
            |doc| doc["publicKey"]["publicKeyPem"] = json!(""),
            "publicKeyPem",
        ),
        (
            "owner names another actor",
            |doc| doc["publicKey"]["owner"] = json!("https://evil.example/users/alice"),
            "owner",
        ),
    ]
}

#[test]
fn documents_that_are_not_actors_are_rejected() {
    for (name, break_it, fragment) in rejections() {
        let mut document = fixture(MASTODON);
        break_it(&mut document);
        let error = parse_actor(&document).unwrap_err();
        let ActorError::Invalid(detail) = error else {
            panic!("{name}: must fail as Invalid, not {error:?}");
        };
        assert!(
            detail.contains(fragment),
            "{name}: the message `{detail}` must name {fragment}"
        );
    }
}

#[test]
fn every_supported_actor_type_round_trips() {
    for kind in ACTOR_TYPES {
        let mut document = fixture(MASTODON);
        document["type"] = json!(kind);
        let parsed = parse_actor(&document).unwrap();
        assert_eq!(parsed.actor_type, kind, "{kind} is accepted verbatim");
    }
}

/// Mastodon's `Account#bot?`: `Service` and `Application` are bots, every
/// other kind (`Group` included) is not.
#[test]
fn only_service_and_application_actors_are_bots() {
    for (kind, bot) in [
        ("Person", false),
        ("Service", true),
        ("Application", true),
        ("Group", false),
        ("Organization", false),
    ] {
        let mut document = fixture(MASTODON);
        document["type"] = json!(kind);
        let parsed = parse_actor(&document).unwrap();
        assert_eq!(parsed.bot.get(), bot, "{kind} bot flag");
    }
}

#[test]
fn an_actor_without_a_type_is_not_a_bot() {
    let mut document = fixture(MASTODON);
    let _removed = document.as_object_mut().unwrap().remove("type");
    let parsed = parse_actor(&document).unwrap();
    assert!(!parsed.bot.get(), "the Person default is no bot");
}

#[test]
fn the_summary_is_sanitized_and_the_source_kept() {
    let mut document = fixture(MASTODON);
    document["summary"] = json!(
        r#"<p onclick="x()">Hello <script>steal()</script>there <a href="javascript:alert(1)">x</a>!</p>"#
    );
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(
        parsed.bio, r#"<p>Hello there <a rel="ugc nofollow noopener noreferrer">x</a>!</p>"#,
        "onclick, the script and the javascript: href are gone"
    );
    assert_eq!(parsed.raw_bio, "Hello there x!\n", "the note as plain text");
}

#[test]
fn endpoints_on_other_hosts_are_accepted_without_containment() {
    let mut document = fixture(MASTODON);
    document["inbox"] = json!("https://relay.example/inbox/alice");
    document["outbox"] = json!("https://other.example/outbox/alice");
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(
        parsed.inbox, "https://relay.example/inbox/alice",
        "inbox host may differ"
    );
    assert_eq!(
        parsed.outbox.as_deref(),
        Some("https://other.example/outbox/alice"),
        "outbox host may differ"
    );
    assert_eq!(
        parsed.domain, "mastodon.example",
        "the domain is the id's host"
    );
    assert_eq!(
        parsed.nickname, "alice@mastodon.example",
        "and so the nickname's"
    );
}

#[test]
fn ids_are_stored_verbatim_while_hosts_are_lowercased() {
    let mut document = fixture(MASTODON);
    let mixed = "https://Mastodon.Example/users/alice";
    document["id"] = json!(mixed);
    document["publicKey"]["owner"] = json!(mixed);
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(parsed.ap_id, mixed, "the id bytes the remote sent");
    assert_eq!(
        parsed.domain, "mastodon.example",
        "the host comes back lowercased"
    );
    assert_eq!(parsed.nickname, "alice@mastodon.example");
}

#[test]
fn non_object_documents_are_rejected() {
    for document in [
        json!("https://x.example/users/a"),
        json!([]),
        json!(null),
        json!(1),
    ] {
        let error = parse_actor(&document).unwrap_err();
        assert!(
            matches!(error, ActorError::Invalid(_)),
            "a document that is not an object fails Invalid, got {error:?}"
        );
    }
}

#[test]
fn a_long_name_is_truncated_to_the_cap() {
    let mut document = fixture(MASTODON);
    document["name"] = json!("n".repeat(3000));
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(parsed.name.chars().count(), MAX_REMOTE_NAME_CHARS);
}

#[test]
fn a_long_summary_is_truncated_on_a_char_boundary() {
    let mut document = fixture(MASTODON);
    document["summary"] = json!("é".repeat(30_000));
    let parsed = parse_actor(&document).unwrap();
    assert!(parsed.raw_bio.chars().count() <= MAX_REMOTE_BIO_CHARS);
    assert!(
        parsed.raw_bio.chars().all(|c| c == 'é'),
        "multibyte input is cut between characters"
    );
    assert!(parsed.bio.chars().count() <= MAX_REMOTE_BIO_CHARS * 6);
    assert!(!parsed.bio.is_empty(), "the kept prefix is still sanitized");
}

#[test]
fn truncate_chars_keeps_short_text_and_cuts_on_boundaries() {
    assert_eq!(truncate_chars("abc", 3), "abc");
    assert_eq!(truncate_chars("abc", 10), "abc");
    assert_eq!(truncate_chars("ééé", 2), "éé");
    assert_eq!(truncate_chars("abc", 0), "");
}

#[test]
fn only_the_first_fifty_fields_are_kept() {
    let mut document = fixture(MASTODON);
    let attachment: Vec<Value> = (0..60)
        .map(|i| json!({"type": "PropertyValue", "name": format!("k{i}"), "value": "v"}))
        .collect();
    document["attachment"] = json!(attachment);
    let parsed = parse_actor(&document).unwrap();
    let fields = parsed.fields.as_array().unwrap();
    assert_eq!(fields.len(), MAX_REMOTE_FIELDS);
    assert_eq!(fields[0]["name"], "k0", "the first entries win");
    assert_eq!(fields[49]["name"], "k49");
}

#[test]
fn over_long_field_names_and_values_are_truncated() {
    let mut document = fixture(MASTODON);
    document["attachment"] = json!([{
        "type": "PropertyValue",
        "name": "n".repeat(600),
        "value": "v".repeat(3000),
    }]);
    let parsed = parse_actor(&document).unwrap();
    let field = &parsed.fields[0];
    assert_eq!(
        field["name"].as_str().unwrap().chars().count(),
        MAX_FIELD_NAME_CHARS
    );
    assert_eq!(
        field["value"].as_str().unwrap().chars().count(),
        MAX_FIELD_VALUE_CHARS
    );
}

#[test]
fn only_the_first_256_aliases_are_kept() {
    let mut document = fixture(MASTODON);
    let aliases: Vec<String> = (0..300)
        .map(|i| format!("https://old.example/users/{i}"))
        .collect();
    document["alsoKnownAs"] = json!(aliases);
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(parsed.also_known_as.len(), MAX_ALIASES);
    assert_eq!(parsed.also_known_as[0], "https://old.example/users/0");
}

#[test]
fn an_over_long_username_is_rejected_and_the_cap_itself_accepted() {
    let mut document = fixture(MASTODON);
    document["preferredUsername"] = json!("a".repeat(MAX_USERNAME_CHARS + 1));
    let error = parse_actor(&document).unwrap_err();
    assert!(
        matches!(error, ActorError::Invalid(_)),
        "2049 characters fail Invalid, got {error:?}"
    );
    document["preferredUsername"] = json!("a".repeat(MAX_USERNAME_CHARS));
    assert!(parse_actor(&document).is_ok(), "2048 characters pass");
}

#[test]
fn emoji_tags_of_both_server_shapes_are_parsed_and_malformed_ones_skipped() {
    let mut document = fixture(MASTODON);
    document["tag"] = json!([
        {
            "id": "https://mastodon.example/emojis/1",
            "type": "Emoji",
            "name": ":mastofox:",
            "updated": "2024-01-01T00:00:00Z",
            "icon": {
                "type": "Image",
                "mediaType": "image/png",
                "url": "https://mastodon.example/system/emoji/mastofox.png"
            }
        },
        {
            "id": "https://mastodon.example/emojis/misskeycat",
            "type": "Emoji",
            "name": "misskeycat",
            "icon": {"type": "Image", "url": "https://mastodon.example/files/misskeycat.webp"}
        },
        {"type": "Emoji", "name": ":no icon:", "icon": {"url": "https://mastodon.example/x.png"}},
        {"type": "Emoji", "name": ":noicon:"},
        {"type": "Emoji", "icon": {"url": "https://mastodon.example/y.png"}},
        {"type": "Hashtag", "name": "#rust"},
        "stray"
    ]);
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(
        parsed.emojis,
        [
            RemoteEmoji {
                shortcode: "mastofox".to_owned(),
                url: "https://mastodon.example/system/emoji/mastofox.png".to_owned(),
                domain: Some("mastodon.example".to_owned()),
            },
            RemoteEmoji {
                shortcode: "misskeycat".to_owned(),
                url: "https://mastodon.example/files/misskeycat.webp".to_owned(),
                domain: Some("mastodon.example".to_owned()),
            },
        ],
        "both shapes parse, colons stripped, filed under the actor's host; \
         the malformed and non-emoji entries are skipped"
    );
}

#[test]
fn an_actor_without_emoji_tags_has_no_emojis() {
    let mut document = fixture(MASTODON);
    assert!(
        parse_actor(&document).unwrap().emojis.is_empty(),
        "an empty tag array"
    );
    remove(&mut document, "tag");
    assert!(
        parse_actor(&document).unwrap().emojis.is_empty(),
        "no tag key"
    );
}

#[test]
fn a_single_object_tag_is_read_as_a_one_entry_list() {
    let mut document = fixture(MASTODON);
    document["tag"] = json!({
        "type": "Emoji",
        "name": ":solo:",
        "icon": {"url": "https://mastodon.example/solo.png"}
    });
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(parsed.emojis.len(), 1, "one object is a one-entry list");
}

#[test]
fn only_the_first_hundred_tags_are_examined_for_emojis() {
    let mut document = fixture(MASTODON);
    let tags: Vec<Value> = (0..120)
        .map(|n| {
            json!({
                "type": "Emoji",
                "name": format!(":e{n}:"),
                "icon": {"url": format!("https://mastodon.example/e{n}.png")}
            })
        })
        .collect();
    document["tag"] = json!(tags);
    let parsed = parse_actor(&document).unwrap();
    assert_eq!(parsed.emojis.len(), MAX_REMOTE_EMOJIS, "the cap");
}

#[test]
fn vcard_bday_is_read_as_a_date() {
    let mut document = fixture(PLEROMA);
    assert_eq!(
        parse_actor(&document).unwrap().birthday,
        None,
        "the fixture carries no birthday"
    );
    document["vcard:bday"] = json!("1990-05-15");
    assert_eq!(
        parse_actor(&document).unwrap().birthday,
        chrono::NaiveDate::from_ymd_opt(1990, 5, 15),
        "a plain date"
    );
    document["vcard:bday"] = json!("1990-05-15T23:30:00-05:00");
    assert_eq!(
        parse_actor(&document).unwrap().birthday,
        chrono::NaiveDate::from_ymd_opt(1990, 5, 15),
        "a datetime keeps the date it is written with"
    );
}

#[test]
fn an_invalid_or_absent_vcard_bday_is_ignored() {
    let mut document = fixture(PLEROMA);
    for bad in [
        json!("yesterday"),
        json!("1990-13-45"),
        json!(7),
        Value::Null,
    ] {
        document["vcard:bday"] = bad.clone();
        assert_eq!(
            parse_actor(&document).unwrap().birthday,
            None,
            "{bad} is dropped, not rejected"
        );
    }
    remove(&mut document, "vcard:bday");
    assert_eq!(parse_actor(&document).unwrap().birthday, None, "absent");
}
