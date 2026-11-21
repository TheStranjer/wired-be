//! Tests for [`parse_actor`]: the fixtures, the defaults and every rejection.

use serde_json::{Value, json};

use super::*;
use crate::{
    federation::remote_actor::test_support::{MASTODON, PLEROMA, fixture, remove},
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
        parsed.public_key, "PEM-without-an-owner",
        "owner may be absent"
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
