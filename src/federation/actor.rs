//! The `ActivityPub` actor document of a local user.
//!
//! The reference shape is `render("user.json", %{user: user})` with the
//! `common_actor_fields/1` and `render("endpoints.json", …)` clauses beside
//! it in Pleroma's `Pleroma.Web.ActivityPub.UserView`
//! (`lib/pleroma/web/activity_pub/views/user_view.ex`): the actor id and
//! type, the collections derived from the id, the profile text, the
//! `endpoints` and `publicKey` objects, and the JSON-LD context.
//!
//! [`actor_json`] is pure (decision 3): every value comes from the [`User`]
//! row, from the instance [`Config`], from the public key its caller read
//! from `activitypub_actors` ([`stored_public_key`]), or from a documented
//! constant, so a test needs no database. It renders the document of a *local* user: the
//! collections are derived from `ap_id` the way Pleroma derives them, not
//! read back from the endpoint columns, and the endpoints themselves are
//! this instance's, which only makes sense for an actor this instance owns.
//! The route that serves it ([`crate::api::activitypub`]) answers 404 for a
//! remote mirror.
//!
//! Deliberately absent, unlike Pleroma: the `uploadMedia` endpoint (no
//! upload-over-AP endpoint yet) and `webfinger`, which arrive with their
//! own tickets. `vcard:bday` is emitted, as Pleroma does, only while the
//! user shows a birthday; otherwise the key is left out.
//!
//! The `tag` list carries one `Emoji` tag per local custom emoji the profile
//! text uses. The document stays pure by taking those rows as an argument;
//! [`profile_emojis`] is the async lookup its callers run first.

use std::collections::{BTreeSet, HashSet};

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{
    config::Config,
    error::AppResult,
    federation::{
        note::{LANGUAGE_UNDETERMINED, LITEPUB_SCHEMA_PATH, SECURITY_VOCABULARY, tags::emoji_tag},
        vocab::AS_VOCABULARY,
    },
    models::{
        activitypub_actor,
        custom_emoji::{self, CustomEmoji},
        user::{User, featured, featured_tags, followers, following, inbox, outbox},
    },
    util::{emoji::shortcodes_in, time::mastodon_time},
};

/// Fragment appended to the actor id for the key's own id (Pleroma's
/// `common_actor_fields`, `"#{user.ap_id}#main-key"`).
pub const MAIN_KEY_FRAGMENT: &str = "#main-key";
/// The `type` of the `attachment` entries (Mastodon's profile metadata).
const PROPERTY_VALUE_TYPE: &str = "PropertyValue";
/// The `type` of an `icon` or `image` value.
const IMAGE_TYPE: &str = "Image";

/// The `ActivityPub` actor document for the local user `user`, as served by
/// the instance described by `config`.
///
/// The returned value is a JSON object whose keys are exactly the fediverse
/// actor fields this instance federates: `@context`, `id`, `type`,
/// `preferredUsername`, `name`, `summary`, `url`, the four collections,
/// `featured`, `featuredTags`, `endpoints`, `publicKey`, `manuallyApprovesFollowers`,
/// `discoverable`, `attachment`, `published`, `tag`, `alsoKnownAs` (the
/// row's stored aliases) and `capabilities`, plus `icon` and `image` only
/// when the row carries an avatar or a banner and `movedTo` only when the
/// row records a move.
///
/// `inbox` renders the `user`'s stored `inbox` (the built inbox address
/// behind a missing one). A caller serving a stored account passes it
/// through [`with_plumbing`](crate::models::activitypub_actor::with_plumbing),
/// so the inbox, like the public key, follows the `activitypub_actors` row.
///
/// `emojis` is the pool of custom emojis the caller knows; the `tag` array
/// holds an `Emoji` tag for each local, enabled one whose `:shortcode:` the
/// profile text uses (see [`profile_emojis`]), once each, in `emojis` order.
///
/// `public_key` is the PEM the account's `activitypub_actors` row stores
/// (see [`stored_public_key`]); `None` renders `publicKeyPem` as `null`.
#[must_use]
pub fn actor_json(
    user: &User,
    public_key: Option<&str>,
    config: &Config,
    emojis: &[CustomEmoji],
) -> Value {
    Value::Object(
        actor_fields(user, public_key, config, emojis)
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

/// The `ActivityPub` document of the instance actor.
///
/// It is [`actor_json`] without `featured` and `featuredTags`: the instance
/// actor pins nothing, features no tags and serves neither collection, so
/// advertising them only makes peers fetch a 404. Its `outbox`, `followers`
/// and `following` stay, as the empty collections `/internal/fetch` serves.
#[must_use]
pub fn instance_actor_json(
    user: &User,
    public_key: Option<&str>,
    config: &Config,
    emojis: &[CustomEmoji],
) -> Value {
    let mut document = actor_json(user, public_key, config, emojis);
    if let Some(fields) = document.as_object_mut() {
        drop(fields.remove("featured"));
        drop(fields.remove("featuredTags"));
    }
    document
}

/// Every field of [`actor_json`]'s object as a `(name, value)` pair, in the
/// order Pleroma's renderer assembles them; the media fields come in only
/// when the row has the media, and `movedTo` only when the row records a
/// move.
fn actor_fields(
    user: &User,
    stored_key: Option<&str>,
    config: &Config,
    emojis: &[CustomEmoji],
) -> Vec<(&'static str, Value)> {
    let ap_id = user.ap_id.as_str();
    let mut fields = vec![
        ("@context", json_ld_context(config)),
        ("id", json!(ap_id)),
        ("type", json!(user.actor_type)),
        ("preferredUsername", json!(user.nickname)),
        ("name", json!(user.name)),
        ("summary", json!(user.bio)),
        ("url", json!(ap_id)),
        (
            "inbox",
            json!(user.inbox.clone().unwrap_or_else(|| inbox(ap_id))),
        ),
        ("outbox", json!(outbox(ap_id))),
        ("followers", json!(followers(ap_id))),
        ("following", json!(following(ap_id))),
        ("featured", json!(featured(ap_id))),
        ("featuredTags", json!(featured_tags(ap_id))),
        ("endpoints", endpoints(config)),
        ("publicKey", public_key(user, stored_key)),
        ("manuallyApprovesFollowers", json!(user.locked.get())),
        ("discoverable", json!(user.discoverable.get())),
    ];
    if let Some(avatar) = user.avatar_url.as_deref() {
        fields.push(("icon", media_object(avatar)));
    }
    if let Some(banner) = user.banner_url.as_deref() {
        fields.push(("image", media_object(banner)));
    }
    fields.extend([
        ("attachment", attachments(user)),
        ("published", json!(mastodon_time(&user.inserted_at))),
        ("tag", Value::Array(emoji_tags(user, emojis))),
        ("alsoKnownAs", json!(user.also_known_as)),
    ]);
    if let Some(moved_to) = user.moved_to_ap_id.as_deref() {
        fields.push(("movedTo", json!(moved_to)));
    }
    if let Some(birthday) = user.birthday.filter(|_| user.show_birthday.get()) {
        fields.push(("vcard:bday", json!(birthday.to_string())));
    }
    fields.push(("capabilities", capabilities()));
    fields
}

/// Every text of the profile that renders custom emoji: display name, bio
/// (source and rendered), and each stored profile field's name and value.
fn profile_texts(user: &User) -> Vec<&str> {
    let mut texts = vec![user.name.as_str(), user.raw_bio.as_str(), user.bio.as_str()];
    for stored in [&user.fields, &user.raw_fields] {
        for entry in stored.as_array().into_iter().flatten() {
            texts.extend(
                ["name", "value"]
                    .iter()
                    .filter_map(|key| entry.get(*key).and_then(Value::as_str)),
            );
        }
    }
    texts
}

/// The distinct shortcodes the profile text uses, sorted.
fn used_shortcodes(user: &User) -> BTreeSet<&str> {
    profile_texts(user)
        .into_iter()
        .flat_map(shortcodes_in)
        .collect()
}

/// The local, enabled emojis among `user`'s profile shortcodes.
///
/// Shortcodes with no local row are simply absent from the result; with no
/// shortcode in the profile no query runs.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn profile_emojis(pool: &PgPool, user: &User) -> AppResult<Vec<CustomEmoji>> {
    let codes: Vec<String> = used_shortcodes(user)
        .into_iter()
        .map(str::to_owned)
        .collect();
    if codes.is_empty() {
        return Ok(Vec::new());
    }
    let rows = custom_emoji::get_by_shortcodes(pool, &codes, None).await?;
    Ok(rows.into_iter().filter(|emoji| !emoji.disabled).collect())
}

/// The `tag` array: an `Emoji` tag for each distinct local emoji of
/// `emojis` that the profile text of `user` uses.
fn emoji_tags(user: &User, emojis: &[CustomEmoji]) -> Vec<Value> {
    let wanted = used_shortcodes(user);
    let mut seen: HashSet<&str> = HashSet::new();
    emojis
        .iter()
        .filter(|emoji| emoji.domain.is_none() && wanted.contains(emoji.shortcode.as_str()))
        .filter(|emoji| seen.insert(emoji.shortcode.as_str()))
        .map(emoji_tag)
        .collect()
}

/// The `@context` of an actor document: the `ActivityStreams` and
/// `security` vocabularies, this instance's single served term map
/// ([`litepub_context_json`](crate::federation::note::litepub_context_json)
/// at [`LITEPUB_SCHEMA_PATH`]) and the `und` language map — the schema
/// Pleroma's 2.10 actor documents reference too.
fn json_ld_context(config: &Config) -> Value {
    json!([
        AS_VOCABULARY,
        SECURITY_VOCABULARY,
        format!("{}{LITEPUB_SCHEMA_PATH}", config.base_url),
        { "@language": LANGUAGE_UNDETERMINED },
    ])
}

/// The `endpoints` object of a local actor: the instance-wide shared inbox
/// and the OAuth endpoints clients discover here (Pleroma's
/// `render("endpoints.json", %{user: %User{local: true}})`).
fn endpoints(config: &Config) -> Value {
    json!({
        "sharedInbox": format!("{}/inbox", config.base_url),
        "oauthAuthorizationEndpoint": format!("{}/oauth/authorize", config.base_url),
        "oauthTokenEndpoint": format!("{}/oauth/token", config.base_url),
        "oauthRegistrationEndpoint": format!("{}/api/v1/apps", config.base_url),
    })
}

/// The `publicKey` object (Pleroma's `common_actor_fields`): the actor id
/// with [`MAIN_KEY_FRAGMENT`] appended, the actor id as owner and the
/// stored PEM (`pem`). An account without key material renders
/// `publicKeyPem` as JSON `null`; registration always generates a pair, so
/// only a half-provisioned account looks like that.
fn public_key(user: &User, pem: Option<&str>) -> Value {
    json!({
        "id": format!("{}{MAIN_KEY_FRAGMENT}", user.ap_id),
        "owner": user.ap_id,
        "publicKeyPem": pem,
    })
}

/// The public key PEM stored for `user`, from its `activitypub_actors` row.
///
/// An account with no row, like one with no key, has none (`None`).
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
pub async fn stored_public_key(pool: &PgPool, user: &User) -> AppResult<Option<String>> {
    let actor = activitypub_actor::get(pool, user.id).await?;
    Ok(actor.and_then(|row| row.public_key))
}

/// The `attachment` array: the stored profile fields as `PropertyValue`
/// objects. Entries that are not `{name, value}` objects are skipped, so
/// malformed storage never fails a response.
fn attachments(user: &User) -> Value {
    let properties = user.fields.as_array().map_or_default(|entries| {
        entries
            .iter()
            .filter_map(property_value)
            .collect::<Vec<Value>>()
    });
    Value::Array(properties)
}

/// One stored profile field as a `PropertyValue`, `None` when the entry
/// carries no string `name` and `value`.
fn property_value(entry: &Value) -> Option<Value> {
    let name = entry.get("name").and_then(Value::as_str)?;
    let value = entry.get("value").and_then(Value::as_str)?;
    Some(json!({
        "type": PROPERTY_VALUE_TYPE,
        "name": name,
        "value": value,
    }))
}

/// An `icon` or `image` value: the minimal `Image` object Pleroma's
/// `maybe_make_image/4` emits for a stored URL.
fn media_object(url: &str) -> Value {
    json!({ "type": IMAGE_TYPE, "url": url })
}

/// The `litepub:capabilities` object. Chat is not implemented here, and the
/// ticket fixes the flag to `true`; Pleroma sends `acceptsChatMessages`
/// when `Pleroma.Chat.enabled?` and the user opted in.
fn capabilities() -> Value {
    json!({ "acceptsChatMessages": true })
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::{
        federation::note::{litepub_context_json, test_support::undefined_terms},
        models::{
            custom_emoji::NewCustomEmoji,
            user::{DEFAULT_SCOPE, Flag},
        },
        test_fixtures::json_keys,
        util::time::parse_time,
    };

    /// The instance origin of [`Config::for_tests`].
    const BASE: &str = "https://test.example";
    /// The hyphenated lowercase id every fixture row carries.
    const ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
    /// The actor id of the fixture user.
    const AP_ID: &str = "https://test.example/users/alice";
    /// A public key PEM that stands in for registered key material.
    const PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\nRSA-DATA\n-----END PUBLIC KEY-----\n";
    /// Every key of an actor document without avatar or banner, sorted.
    const BASE_KEYS: [&str; 22] = [
        "@context",
        "alsoKnownAs",
        "attachment",
        "capabilities",
        "discoverable",
        "endpoints",
        "featured",
        "featuredTags",
        "followers",
        "following",
        "id",
        "inbox",
        "manuallyApprovesFollowers",
        "name",
        "outbox",
        "preferredUsername",
        "publicKey",
        "published",
        "summary",
        "tag",
        "type",
        "url",
    ];

    /// A [`User`] row named `nickname` at `ap_id`: a local, active `Person`
    /// with no text, no media, no fields and no keys — the row a fresh
    /// registration starts from.
    fn base_user(nickname: &str, ap_id: &str) -> User {
        User {
            id: Uuid::parse_str(ACCOUNT_ID).unwrap(),
            nickname: nickname.to_owned(),
            ap_id: ap_id.to_owned(),
            local: Flag::new(true),
            domain: None,
            actor_type: "Person".to_owned(),
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
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            is_admin: Flag::new(false),
            is_moderator: Flag::new(false),
            is_active: Flag::new(true),
            hide_followers: Flag::new(false),
            hide_follows: Flag::new(false),
            hide_followers_count: Flag::new(false),
            hide_follows_count: Flag::new(false),
            hide_favorites: Flag::new(true),
            allow_following_move: Flag::new(true),
            skip_thread_containment: Flag::new(false),
            accepts_chat_messages: Flag::new(true),
            show_role: Flag::new(true),
            no_rich_text: Flag::new(false),
            default_scope: DEFAULT_SCOPE.to_owned(),
            sensitive_default: Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            notification_settings: json!({}),
            mfa_settings: crate::models::user::mfa::MfaSettings::default_value(),
            background_image_url: None,
            birthday: None,
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
            last_fetched_at: None,
            inserted_at: parse_time("2024-01-31T12:34:56.789Z").unwrap(),
            updated_at: parse_time("2024-01-31T12:34:56.789Z").unwrap(),
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            email_notifications: Flag::new(true),
            deleted_at: None,
        }
    }

    /// The fixture user: a local `alice` with this instance's actor id.
    fn local_alice() -> User {
        base_user("alice", AP_ID)
    }

    /// The document `user` renders on the test instance.
    fn render(user: &User) -> Value {
        actor_json(user, None, &Config::for_tests(), &[])
    }

    /// A local, enabled emoji row named `shortcode`, its image under `BASE`.
    fn fixture_emoji(shortcode: &str) -> CustomEmoji {
        CustomEmoji {
            id: Uuid::parse_str(ACCOUNT_ID).unwrap(),
            shortcode: shortcode.to_owned(),
            domain: None,
            image_url: format!("{BASE}/emoji/{shortcode}.png"),
            static_url: None,
            category: None,
            tags: Vec::new(),
            visible_in_picker: true,
            disabled: false,
            inserted_at: parse_time("2024-01-31T12:34:56.789Z").unwrap(),
            updated_at: parse_time("2024-01-31T12:34:56.789Z").unwrap(),
        }
    }

    /// The `Emoji` tag `shortcode`'s fixture row renders as.
    fn expected_tag(shortcode: &str) -> Value {
        let url = format!("{BASE}/emoji/{shortcode}.png");
        json!({
            "type": "Emoji",
            "id": url,
            "name": format!(":{shortcode}:"),
            "icon": {"type": "Image", "url": url},
        })
    }

    /// A user whose name, bio and one field use `:wiredcat:` and `:other:`.
    fn emoji_alice() -> User {
        let mut alice = local_alice();
        alice.name = "Alice :wiredcat: A".to_owned();
        alice.raw_bio = "bio :wiredcat: and :other:".to_owned();
        alice.fields = json!([{"name": "emoji :wiredcat:", "value": ":other:"}]);
        alice
    }

    #[test]
    fn the_instance_actor_document_omits_only_the_featured_collections() {
        let mut user = base_user("internal.fetch", "https://test.example/internal/fetch");
        "Application".clone_into(&mut user.actor_type);
        let config = Config::for_tests();
        let document = instance_actor_json(&user, None, &config, &[]);
        let expected: Vec<&str> = BASE_KEYS
            .iter()
            .copied()
            .filter(|key| !matches!(*key, "featured" | "featuredTags"))
            .collect();
        assert_eq!(json_keys(&document), expected, "every key but the two");
        let mut with_featured = actor_json(&user, None, &config, &[]);
        for key in ["featured", "featuredTags"] {
            assert!(
                with_featured
                    .as_object_mut()
                    .and_then(|fields| fields.remove(key))
                    .is_some(),
                "the shared renderer still emits {key}"
            );
        }
        assert_eq!(document, with_featured, "nothing else differs");
    }

    #[test]
    fn a_user_document_advertises_its_featured_tags_collection() {
        let document = render(&local_alice());
        assert_eq!(
            document["featuredTags"],
            format!("{AP_ID}/collections/tags"),
            "Mastodon's featuredTags property"
        );
    }

    #[test]
    fn a_user_document_keeps_featured() {
        let document = render(&local_alice());
        assert_eq!(
            document["featured"],
            format!("{AP_ID}/collections/featured"),
            "a user actor still advertises featured"
        );
    }

    #[test]
    fn a_bare_local_user_renders_the_documented_key_set() {
        let document = render(&local_alice());
        assert_eq!(json_keys(&document), BASE_KEYS, "exact key set");
        assert_eq!(document["id"], AP_ID, "the actor id");
        assert_eq!(document["type"], "Person", "the stored actor type");
        assert_eq!(document["preferredUsername"], "alice", "the nickname");
        assert_eq!(document["name"], "", "an empty display name");
        assert_eq!(document["summary"], "", "an empty note");
        assert_eq!(document["url"], AP_ID, "the url is the actor id");
        assert_eq!(document["inbox"], format!("{AP_ID}/inbox"), "inbox");
        assert_eq!(document["outbox"], format!("{AP_ID}/outbox"), "outbox");
        assert_eq!(
            document["followers"],
            format!("{AP_ID}/followers"),
            "followers"
        );
        assert_eq!(
            document["following"],
            format!("{AP_ID}/following"),
            "following"
        );
        assert_eq!(
            document["featured"],
            format!("{AP_ID}/collections/featured"),
            "featured keeps Pleroma's path"
        );
        assert_eq!(
            document["manuallyApprovesFollowers"], false,
            "an unlocked user"
        );
        assert_eq!(document["discoverable"], false, "an undiscoverable user");
        assert_eq!(document["attachment"], json!([]), "no profile fields");
        assert_eq!(document["tag"], json!([]), "no emojis yet");
        assert_eq!(document["alsoKnownAs"], json!([]), "no aliases yet");
        assert!(
            document.get("movedTo").is_none(),
            "an account that never moved emits no movedTo"
        );
        assert_eq!(
            document["capabilities"],
            json!({"acceptsChatMessages": true}),
            "the litepub capabilities object"
        );
        assert_eq!(
            document["published"], "2024-01-31T12:34:56.789Z",
            "published is the insert time in the wire format"
        );
        assert!(
            document.get("icon").is_none() && document.get("image").is_none(),
            "a row without media emits neither icon nor image"
        );
    }

    #[test]
    fn a_shown_birthday_renders_vcard_bday() {
        let mut alice = local_alice();
        alice.birthday = chrono::NaiveDate::from_ymd_opt(1991, 6, 16);
        alice.show_birthday = Flag::new(true);
        assert_eq!(
            render(&alice)["vcard:bday"],
            "1991-06-16",
            "an ISO date, Pleroma's Date.to_iso8601"
        );
        let schema = litepub_context_json();
        assert_eq!(
            schema["@context"]["vcard"], "http://www.w3.org/2006/vcard/ns#",
            "the served schema defines the vcard prefix"
        );
    }

    #[test]
    fn a_hidden_or_missing_birthday_renders_no_vcard_bday() {
        let mut alice = local_alice();
        alice.birthday = chrono::NaiveDate::from_ymd_opt(1991, 6, 16);
        assert!(
            render(&alice).get("vcard:bday").is_none(),
            "a birthday that is not shown stays private"
        );
        alice.birthday = None;
        alice.show_birthday = Flag::new(true);
        assert!(
            render(&alice).get("vcard:bday").is_none(),
            "shown but unset has nothing to emit"
        );
    }

    #[test]
    fn the_context_references_the_one_served_litepub_schema() {
        let document = render(&local_alice());
        assert_eq!(
            document["@context"],
            json!([
                "https://www.w3.org/ns/activitystreams",
                "https://w3id.org/security/v1",
                "https://test.example/schemas/litepub-0.1.jsonld",
                {"@language": "und"},
            ]),
            "vocabulary, security, the served schema, the language map"
        );
    }

    #[test]
    fn the_served_schema_defines_the_actor_terms() {
        let schema = litepub_context_json();
        let aliases = schema["@context"].as_object().unwrap();
        assert_eq!(
            aliases["manuallyApprovesFollowers"], "as:manuallyApprovesFollowers",
            "manuallyApprovesFollowers is an ActivityStreams alias"
        );
        assert_eq!(aliases["discoverable"], "toot:discoverable", "toot term");
        assert_eq!(aliases["toot"], "http://joinmastodon.org/ns#", "toot iri");
        assert_eq!(
            aliases["alsoKnownAs"],
            json!({"@id": "as:alsoKnownAs", "@type": "@id"}),
            "alsoKnownAs is an id reference"
        );
        assert_eq!(
            aliases["movedTo"],
            json!({"@id": "as:movedTo", "@type": "@id"}),
            "movedTo is an id reference"
        );
        assert_eq!(
            aliases["capabilities"], "litepub:capabilities",
            "capabilities is a litepub term"
        );
        assert_eq!(
            aliases["litepub"], "http://litepub.social/ns#",
            "litepub iri"
        );
        assert_eq!(
            aliases["PropertyValue"], "schema:PropertyValue",
            "attachments need the schema term"
        );
    }

    #[test]
    fn every_term_the_actor_document_emits_is_defined_in_the_served_schema() {
        let mut alice = local_alice();
        alice.avatar_url = Some(format!("{BASE}/avatar.png"));
        alice.banner_url = Some(format!("{BASE}/banner.png"));
        alice.moved_to_ap_id = Some("https://other.example/users/alice".to_owned());
        alice.fields = json!([{"name": "site", "value": "https://alice.example"}]);
        assert_eq!(
            undefined_terms(&render(&alice)),
            Vec::<String>::new(),
            "no actor key is left undefined"
        );
    }

    #[test]
    fn the_endpoints_are_the_instances_own() {
        assert_eq!(
            render(&local_alice())["endpoints"],
            json!({
                "sharedInbox": "https://test.example/inbox",
                "oauthAuthorizationEndpoint": "https://test.example/oauth/authorize",
                "oauthTokenEndpoint": "https://test.example/oauth/token",
                "oauthRegistrationEndpoint": "https://test.example/api/v1/apps",
            }),
            "the endpoints object Pleroma renders for a local user"
        );
    }

    #[test]
    fn the_public_key_is_the_actors_main_key() {
        let mut alice = local_alice();
        alice.public_key = Some("the old users column".to_owned());
        assert_eq!(
            actor_json(&alice, Some(PUBLIC_PEM), &Config::for_tests(), &[])["publicKey"],
            json!({
                "id": format!("{AP_ID}#main-key"),
                "owner": AP_ID,
                "publicKeyPem": PUBLIC_PEM,
            }),
            "key id, owner and the PEM handed in, not the users column"
        );

        let document = render(&local_alice());
        assert_eq!(
            document["publicKey"]["publicKeyPem"],
            Value::Null,
            "a row without key material renders a null PEM"
        );
        assert_eq!(
            document["publicKey"]["id"],
            format!("{AP_ID}#main-key"),
            "the key id is there regardless"
        );
    }

    #[test]
    fn a_locked_user_approves_followers_manually() {
        let mut alice = local_alice();
        alice.locked = Flag::new(true);
        let document = render(&alice);
        assert_eq!(
            document["manuallyApprovesFollowers"], true,
            "the locked flag is what the flag means"
        );
        assert_eq!(json_keys(&document), BASE_KEYS, "the key set is stable");
    }

    #[test]
    fn a_bot_keeps_its_actor_type() {
        let mut alice = local_alice();
        alice.actor_type = "Service".to_owned();
        alice.bot = Flag::new(true);
        let document = render(&alice);
        assert_eq!(document["type"], "Service", "the stored actor type");
        assert!(
            document.get("bot").is_none(),
            "Pleroma does not federate the bot flag: the type says it"
        );
        assert_eq!(json_keys(&document), BASE_KEYS, "the key set is stable");
    }

    #[test]
    fn a_discoverable_user_says_so() {
        let mut alice = local_alice();
        alice.discoverable = Flag::new(true);
        assert_eq!(render(&alice)["discoverable"], true, "the flag");
    }

    #[test]
    fn avatar_and_banner_become_icon_and_image() {
        let mut alice = local_alice();
        alice.avatar_url = Some(format!("{BASE}/avatar.png"));
        alice.banner_url = Some(format!("{BASE}/banner.png"));
        let document = render(&alice);
        assert_eq!(
            document["icon"],
            json!({"type": "Image", "url": "https://test.example/avatar.png"}),
            "the avatar is an Image object"
        );
        assert_eq!(
            document["image"],
            json!({"type": "Image", "url": "https://test.example/banner.png"}),
            "the banner is an Image object"
        );
        let mut expected: Vec<&str> = BASE_KEYS.to_vec();
        expected.extend(["icon", "image"]);
        expected.sort_unstable();
        assert_eq!(json_keys(&document), expected, "two more keys");
    }

    #[test]
    fn an_avatar_alone_adds_only_the_icon() {
        let mut alice = local_alice();
        alice.avatar_url = Some(format!("{BASE}/avatar.png"));
        let document = render(&alice);
        assert!(
            document.get("icon").is_some() && document.get("image").is_none(),
            "the avatar alone gives an icon and no image"
        );
    }

    #[test]
    fn profile_fields_become_property_value_attachments() {
        let mut alice = local_alice();
        alice.bio = "<p>hello <em>there</em></p>".to_owned();
        alice.fields = json!([
            {"name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>"},
            {"name": "pronouns", "value": "she/her"},
            {"name": "broken", "value": 7},
            "not an object",
        ]);
        let document = render(&alice);
        assert_eq!(
            document["summary"], "<p>hello <em>there</em></p>",
            "the note is the stored HTML"
        );
        assert_eq!(
            document["attachment"],
            json!([
                {"type": "PropertyValue", "name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>"},
                {"type": "PropertyValue", "name": "pronouns", "value": "she/her"},
            ]),
            "the well-formed fields, in order; the rest are skipped"
        );
    }

    #[test]
    fn a_non_array_fields_column_renders_no_attachments() {
        let mut alice = local_alice();
        alice.fields = json!({"name": "site", "value": "https://alice.example"});
        assert_eq!(
            render(&alice)["attachment"],
            json!([]),
            "malformed storage renders as the empty list"
        );
    }

    #[test]
    fn stored_aliases_render_under_also_known_as_in_order() {
        let mut alice = local_alice();
        alice.also_known_as = vec![
            "https://test.example/users/alicia".to_owned(),
            "https://other.example/users/alice".to_owned(),
        ];
        let document = render(&alice);
        assert_eq!(
            document["alsoKnownAs"],
            json!([
                "https://test.example/users/alicia",
                "https://other.example/users/alice",
            ]),
            "the stored actor ids, verbatim and in order"
        );
        assert_eq!(
            json_keys(&document),
            BASE_KEYS,
            "aliases are a value, not a new key"
        );
    }

    #[test]
    fn a_moved_account_emits_moved_to() {
        let mut alice = local_alice();
        alice.moved_to_ap_id = Some("https://other.example/users/alice".to_owned());
        let document = render(&alice);
        assert_eq!(
            document["movedTo"], "https://other.example/users/alice",
            "the move target rides the actor document"
        );
        let mut expected: Vec<&str> = BASE_KEYS.to_vec();
        expected.push("movedTo");
        expected.sort_unstable();
        assert_eq!(json_keys(&document), expected, "one more key");
    }

    #[test]
    fn used_emojis_render_once_each_in_the_given_order() {
        let emojis = [fixture_emoji("other"), fixture_emoji("wiredcat")];
        let document = actor_json(&emoji_alice(), None, &Config::for_tests(), &emojis);
        assert_eq!(
            document["tag"],
            json!([expected_tag("other"), expected_tag("wiredcat")]),
            "one tag per emoji, though each shortcode is used twice"
        );
    }

    #[test]
    fn a_bio_and_a_field_alone_are_enough_to_use_an_emoji() {
        let mut alice = local_alice();
        alice.bio = "<p>:wiredcat:</p>".to_owned();
        let emojis = [fixture_emoji("wiredcat")];
        assert_eq!(
            actor_json(&alice, None, &Config::for_tests(), &emojis)["tag"],
            json!([expected_tag("wiredcat")]),
            "the rendered bio counts"
        );
        let mut bob = local_alice();
        bob.raw_fields = json!([{"name": ":wiredcat:", "value": "x"}]);
        assert_eq!(
            actor_json(&bob, None, &Config::for_tests(), &emojis)["tag"],
            json!([expected_tag("wiredcat")]),
            "a raw field name counts"
        );
    }

    #[test]
    fn a_profile_without_emoji_renders_an_empty_tag_list() {
        let emojis = [fixture_emoji("wiredcat")];
        let document = actor_json(&local_alice(), None, &Config::for_tests(), &emojis);
        assert_eq!(document["tag"], json!([]), "known emojis the text skips");
    }

    #[test]
    fn a_shortcode_without_an_emoji_row_makes_no_tag() {
        let emojis = [fixture_emoji("wiredcat")];
        let document = actor_json(&emoji_alice(), None, &Config::for_tests(), &emojis);
        assert_eq!(
            document["tag"],
            json!([expected_tag("wiredcat")]),
            "`other` has no row, so only `wiredcat` is tagged"
        );
    }

    #[test]
    fn a_remote_emoji_makes_no_tag() {
        let mut remote = fixture_emoji("wiredcat");
        remote.domain = Some("remote.example".to_owned());
        let document = actor_json(&emoji_alice(), None, &Config::for_tests(), &[remote]);
        assert_eq!(document["tag"], json!([]), "only local emojis federate");
    }

    /// The key of the document is the one in `activitypub_actors`: after only
    /// that row changes, [`stored_public_key`] hands the new PEM to the
    /// renderer, and an account with no row renders a null PEM.
    #[tokio::test]
    async fn the_document_key_is_read_from_the_actor_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = crate::models::user::insert(
            &pool,
            &crate::models::user::NewUser::local("alice", AP_ID),
        )
        .await
        .unwrap();
        let changed =
            sqlx::query("UPDATE activitypub_actors SET public_key = $2 WHERE user_id = $1")
                .bind(alice.id)
                .bind(PUBLIC_PEM)
                .execute(&pool)
                .await
                .unwrap()
                .rows_affected();
        assert_eq!(changed, 1, "registration wrote one actor row");

        let key = stored_public_key(&pool, &alice).await.unwrap();
        let document = actor_json(&alice, key.as_deref(), &Config::for_tests(), &[]);
        assert_eq!(
            document["publicKey"]["publicKeyPem"], PUBLIC_PEM,
            "the actor row's key, though users holds another"
        );

        let removed = sqlx::query("DELETE FROM activitypub_actors WHERE user_id = $1")
            .bind(alice.id)
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(removed, 1, "the actor row goes");
        assert_eq!(
            stored_public_key(&pool, &alice).await.unwrap(),
            None,
            "no row, no key"
        );
    }

    #[tokio::test]
    async fn profile_emojis_loads_the_local_enabled_rows_the_profile_uses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        for code in ["wiredcat", "unused"] {
            let row = NewCustomEmoji::local(code, &format!("{BASE}/emoji/{code}.png"));
            let _stored = custom_emoji::upsert(&pool, &row).await.unwrap();
        }
        let mut off = NewCustomEmoji::local("other", &format!("{BASE}/emoji/other.png"));
        off.disabled = true;
        let _stored = custom_emoji::upsert(&pool, &off).await.unwrap();

        let rows = profile_emojis(&pool, &emoji_alice()).await.unwrap();
        let codes: Vec<&str> = rows.iter().map(|row| row.shortcode.as_str()).collect();
        assert_eq!(codes, ["wiredcat"], "used and enabled; `other` is disabled");

        let none = profile_emojis(&pool, &local_alice()).await.unwrap();
        assert!(none.is_empty(), "no shortcode, no rows");
    }
}
