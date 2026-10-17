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
//! row, from the instance [`Config`], or from a documented constant, so a
//! test needs no database. It renders the document of a *local* user: the
//! collections are derived from `ap_id` the way Pleroma derives them, not
//! read back from the endpoint columns, and the endpoints themselves are
//! this instance's, which only makes sense for an actor this instance owns.
//! The route that serves it ([`crate::api::activitypub`]) answers 404 for a
//! remote mirror.
//!
//! Deliberately absent, unlike Pleroma: the `uploadMedia` endpoint (no
//! upload-over-AP endpoint yet), `webfinger`, `vcard:bday` and the emoji
//! `tag` list, which arrive with their own tickets.

use serde_json::{Value, json};

use crate::{
    config::Config,
    models::user::{User, followers, following, inbox, outbox},
    util::time::mastodon_time,
};

/// Path appended to the actor id for the featured (pinned) collection, as
/// Pleroma's `UserView` names it. Note this differs from
/// [`crate::models::user::featured`], the column default's path.
const FEATURED_PATH: &str = "/collections/featured";
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
/// `featured`, `endpoints`, `publicKey`, `manuallyApprovesFollowers`,
/// `discoverable`, `attachment`, `published`, `tag`, `alsoKnownAs` and
/// `capabilities`, plus `icon` and `image` only when the row carries an
/// avatar or a banner.
#[must_use]
pub fn actor_json(user: &User, config: &Config) -> Value {
    Value::Object(
        actor_fields(user, config)
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

/// Every field of [`actor_json`]'s object as a `(name, value)` pair, in the
/// order Pleroma's renderer assembles them; the media fields come in only
/// when the row has the media.
fn actor_fields(user: &User, config: &Config) -> Vec<(&'static str, Value)> {
    let ap_id = user.ap_id.as_str();
    let mut fields = vec![
        ("@context", json_ld_context()),
        ("id", json!(ap_id)),
        ("type", json!(user.actor_type)),
        ("preferredUsername", json!(user.nickname)),
        ("name", json!(user.name)),
        ("summary", json!(user.bio)),
        ("url", json!(ap_id)),
        ("inbox", json!(inbox(ap_id))),
        ("outbox", json!(outbox(ap_id))),
        ("followers", json!(followers(ap_id))),
        ("following", json!(following(ap_id))),
        ("featured", json!(format!("{ap_id}{FEATURED_PATH}"))),
        ("endpoints", endpoints(config)),
        ("publicKey", public_key(user)),
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
        ("tag", json!([])),
        ("alsoKnownAs", json!([])),
        ("capabilities", capabilities()),
    ]);
    fields
}

/// The `@context` of an actor document: the `ActivityStreams` and
/// `security` vocabularies plus the inline term definitions Pleroma ships
/// in `priv/static/schemas/litepub-0.1.jsonld` (the schema its 2.10
/// documents reference), extended with the `movedTo` alias of a moved
/// actor.
fn json_ld_context() -> Value {
    json!([
        "https://www.w3.org/ns/activitystreams",
        "https://w3id.org/security/v1",
        {
            "as": "https://www.w3.org/ns/activitystreams#",
            "fedibird": "http://fedibird.com/ns#",
            "litepub": "http://litepub.social/ns#",
            "ostatus": "http://ostatus.org#",
            "schema": "http://schema.org#",
            "sm": "http://smithereen.software/ns#",
            "toot": "http://joinmastodon.org/ns#",
            "vcard": "http://www.w3.org/2006/vcard/ns#",
            "alsoKnownAs": {"@id": "as:alsoKnownAs", "@type": "@id"},
            "capabilities": "litepub:capabilities",
            "ChatMessage": "litepub:ChatMessage",
            "conversation": {"@id": "ostatus:conversation", "@type": "@id"},
            "directMessage": "litepub:directMessage",
            "discoverable": "toot:discoverable",
            "Emoji": "toot:Emoji",
            "EmojiReact": "litepub:EmojiReact",
            "formerRepresentations": "litepub:formerRepresentations",
            "Hashtag": "as:Hashtag",
            "invisible": "litepub:invisible",
            "listMessage": {"@id": "litepub:listMessage", "@type": "@id"},
            "manuallyApprovesFollowers": "as:manuallyApprovesFollowers",
            "movedTo": {"@id": "as:movedTo", "@type": "@id"},
            "nonAnonymous": "sm:nonAnonymous",
            "oauthRegistrationEndpoint": {
                "@id": "litepub:oauthRegistrationEndpoint",
                "@type": "@id"
            },
            "PropertyValue": "schema:PropertyValue",
            "quoteUri": "fedibird:quoteUri",
            "quoteUrl": "as:quoteUrl",
            "sensitive": "as:sensitive",
            "value": "schema:value",
        }
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
/// stored PEM. A row without key material renders `publicKeyPem` as JSON
/// `null`; registration always generates a pair, so only a half-provisioned
/// row looks like that.
fn public_key(user: &User) -> Value {
    json!({
        "id": format!("{}{MAIN_KEY_FRAGMENT}", user.ap_id),
        "owner": user.ap_id,
        "publicKeyPem": user.public_key,
    })
}

/// The `attachment` array: the stored profile fields as `PropertyValue`
/// objects. Entries that are not `{name, value}` objects are skipped, so
/// malformed storage never fails a response.
fn attachments(user: &User) -> Value {
    let properties = user
        .fields
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(property_value)
                .collect::<Vec<Value>>()
        })
        .unwrap_or_default();
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
        models::user::{DEFAULT_SCOPE, Flag},
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
    const BASE_KEYS: [&str; 21] = [
        "@context",
        "alsoKnownAs",
        "attachment",
        "capabilities",
        "discoverable",
        "endpoints",
        "featured",
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
            preferred_frontend: None,
            is_suggested: Flag::new(false),
        }
    }

    /// The fixture user: a local `alice` with this instance's actor id.
    fn local_alice() -> User {
        base_user("alice", AP_ID)
    }

    /// The document `user` renders on the test instance.
    fn render(user: &User) -> Value {
        actor_json(user, &Config::for_tests())
    }

    /// The rendered object's keys, sorted, for exact key-set assertions.
    fn json_keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
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
    fn the_context_is_the_activitypub_one_pleroma_emits() {
        let document = render(&local_alice());
        let terms = document["@context"].as_array().unwrap();
        assert_eq!(
            terms[0],
            json!("https://www.w3.org/ns/activitystreams"),
            "the ActivityStreams vocabulary comes first"
        );
        assert_eq!(
            terms[1],
            json!("https://w3id.org/security/v1"),
            "then the security vocabulary that defines publicKey"
        );
        let aliases = terms[2].as_object().unwrap();
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
        alice.public_key = Some(PUBLIC_PEM.to_owned());
        assert_eq!(
            render(&alice)["publicKey"],
            json!({
                "id": format!("{AP_ID}#main-key"),
                "owner": AP_ID,
                "publicKeyPem": PUBLIC_PEM,
            }),
            "key id, owner and stored PEM"
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
}
