//! The `Listen` activity of a scrobble and the `Audio` object it wraps.
//!
//! The reference is Pleroma's `Listen` (`ActivityDraft.listen/2`): the
//! activity `{BASE_URL}/activities/{id}` wraps an `Audio` object at
//! `{BASE_URL}/objects/{id}` that carries the media metadata
//! (`title`, `artist`, `album`, `length`, `externalLink`). Both are
//! addressed by the scrobble's own visibility, by the rule statuses use
//! ([`audience_lines`], over the listener's followers collection), and share
//! one `context`.

use serde_json::{Value, json};

use super::{TYPE_AUDIO, TYPE_LISTEN};
use crate::{
    config::Config,
    federation::note::{audience_lines, json_ld_context},
    models::{scrobble::Scrobble, user::User},
    util::{
        id::{activity_ap_id, object_ap_id},
        time::mastodon_time,
    },
};

/// The conversation id both documents of `scrobble` share:
/// `{BASE_URL}/contexts/{id}`, the shape a status' context takes.
fn context_id(scrobble: &Scrobble, config: &Config) -> String {
    format!("{}/contexts/{}", config.base_url, scrobble.id)
}

/// The `Audio` object behind `scrobble`, listened to by `author`.
///
/// Its `id` is `{BASE_URL}/objects/{id}`, its `to` and `cc` what the
/// scrobble's visibility addresses (`followers` is the address of the
/// author's followers collection, the one on their `activitypub_actors`
/// row), and the optional metadata keys (`artist`, `album`, `length`,
/// `externalLink`) are left out when the listener gave none, as Pleroma's
/// object has no key then.
#[must_use]
pub fn audio_json(scrobble: &Scrobble, author: &User, followers: &str, config: &Config) -> Value {
    let (to, cc) = audience_lines(scrobble.visibility, followers, &[]);
    let fields: [(&str, Option<Value>); 13] = [
        (
            "id",
            Some(json!(object_ap_id(&config.base_url, scrobble.id))),
        ),
        ("type", Some(json!(TYPE_AUDIO))),
        ("actor", Some(json!(&author.ap_id))),
        ("attributedTo", Some(json!(&author.ap_id))),
        ("to", Some(Value::Array(to))),
        ("cc", Some(Value::Array(cc))),
        ("title", Some(json!(&scrobble.title))),
        ("artist", scrobble.artist.as_ref().map(|text| json!(text))),
        ("album", scrobble.album.as_ref().map(|text| json!(text))),
        ("length", scrobble.length.map(|millis| json!(millis))),
        (
            "externalLink",
            scrobble.external_link.as_ref().map(|link| json!(link)),
        ),
        (
            "published",
            Some(json!(mastodon_time(&scrobble.inserted_at))),
        ),
        ("context", Some(json!(context_id(scrobble, config)))),
    ];
    Value::Object(
        fields
            .into_iter()
            .filter_map(|(key, value)| value.map(|present| (key.to_owned(), present)))
            .collect(),
    )
}

/// The `Listen` activity of `scrobble`, listened to by `author`, with its
/// [`audio_json`] object embedded.
///
/// The `id` is the row's stored `ap_id` (decision 6), falling back to the
/// id [`activity_ap_id`] mints for a row that has none, so the document a
/// peer fetches is the one delivered. `followers` is as for [`audio_json`].
#[must_use]
pub fn listen_json(scrobble: &Scrobble, author: &User, followers: &str, config: &Config) -> Value {
    let (to, cc) = audience_lines(scrobble.visibility, followers, &[]);
    let id = scrobble
        .ap_id
        .clone()
        .unwrap_or_else(|| activity_ap_id(&config.base_url, scrobble.id));
    json!({
        "@context": json_ld_context(config),
        "id": id,
        "type": TYPE_LISTEN,
        "actor": &author.ap_id,
        "to": to,
        "cc": cc,
        "context": context_id(scrobble, config),
        "published": mastodon_time(&scrobble.inserted_at),
        "object": audio_json(scrobble, author, followers, config),
    })
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::super::test_support::{
        ALICE_AP, BASE, FOLLOWERS, PUBLISHED, config, json_keys, published,
    };
    use super::*;
    use crate::{
        federation::{note::test_support::author, vocab::AS_PUBLIC},
        models::status::Visibility,
    };

    /// The scrobble fixture's id.
    const SCROBBLE_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

    /// The scrobble row of `visibility`, every optional column set.
    fn scrobble(visibility: Visibility) -> Scrobble {
        let id = Uuid::parse_str(SCROBBLE_ID).expect("the fixture id is an id");
        Scrobble {
            id,
            ap_id: Some(format!("{BASE}/activities/{SCROBBLE_ID}")),
            user_id: Uuid::now_v7(),
            title: "Wired Song".to_owned(),
            artist: Some("The Lain".to_owned()),
            album: Some("Serial Experiments".to_owned()),
            length: Some(183_000),
            external_link: Some("https://example.com/track/1".to_owned()),
            visibility,
            inserted_at: published(),
        }
    }

    #[test]
    fn a_public_listen_carries_the_exact_keys_and_addressing() {
        let row = scrobble(Visibility::Public);
        let listen = listen_json(&row, &author(), FOLLOWERS, &config());

        assert_eq!(
            json_keys(&listen),
            [
                "@context",
                "actor",
                "cc",
                "context",
                "id",
                "object",
                "published",
                "to",
                "type"
            ],
            "exactly the Listen keys"
        );
        assert_eq!(listen["type"], "Listen");
        assert_eq!(listen["id"], format!("{BASE}/activities/{SCROBBLE_ID}"));
        assert_eq!(listen["actor"], ALICE_AP);
        assert_eq!(listen["to"], json!([AS_PUBLIC]), "the world in to");
        assert_eq!(listen["cc"], json!([FOLLOWERS]), "the followers in cc");
        assert_eq!(listen["context"], format!("{BASE}/contexts/{SCROBBLE_ID}"));
        assert_eq!(listen["published"], PUBLISHED);
    }

    #[test]
    fn the_embedded_audio_carries_the_media_metadata() {
        let row = scrobble(Visibility::Public);
        let listen = listen_json(&row, &author(), FOLLOWERS, &config());
        let audio = &listen["object"];

        assert_eq!(
            json_keys(audio),
            [
                "actor",
                "album",
                "artist",
                "attributedTo",
                "cc",
                "context",
                "externalLink",
                "id",
                "length",
                "published",
                "title",
                "to",
                "type"
            ],
            "exactly the Audio keys"
        );
        assert_eq!(audio["type"], "Audio");
        assert_eq!(audio["id"], format!("{BASE}/objects/{SCROBBLE_ID}"));
        assert_eq!(audio["actor"], ALICE_AP);
        assert_eq!(audio["attributedTo"], ALICE_AP);
        assert_eq!(audio["title"], "Wired Song");
        assert_eq!(audio["artist"], "The Lain");
        assert_eq!(audio["album"], "Serial Experiments");
        assert_eq!(audio["length"], 183_000);
        assert_eq!(audio["externalLink"], "https://example.com/track/1");
        assert_eq!(audio["published"], PUBLISHED);
        assert_eq!(audio["context"], listen["context"], "one conversation");
        assert_eq!(audio["to"], listen["to"], "the object is addressed alike");
        assert_eq!(audio["cc"], listen["cc"]);
        assert_eq!(
            audio,
            &audio_json(&row, &author(), FOLLOWERS, &config()),
            "and is the document the object route serves"
        );
    }

    #[test]
    fn every_visibility_addresses_as_a_status_does() {
        for (visibility, to, cc) in [
            (Visibility::Public, json!([AS_PUBLIC]), json!([FOLLOWERS])),
            (Visibility::Unlisted, json!([FOLLOWERS]), json!([AS_PUBLIC])),
            (Visibility::Private, json!([FOLLOWERS]), json!([])),
            (Visibility::Direct, json!([]), json!([])),
        ] {
            let listen = listen_json(&scrobble(visibility), &author(), FOLLOWERS, &config());
            let name = visibility.as_str();
            assert_eq!(listen["to"], to, "{name} to");
            assert_eq!(listen["cc"], cc, "{name} cc");
            assert_eq!(listen["object"]["to"], to, "{name} object to");
            assert_eq!(listen["object"]["cc"], cc, "{name} object cc");
        }
    }

    #[test]
    fn absent_metadata_leaves_its_keys_out() {
        let row = Scrobble {
            artist: None,
            album: None,
            length: None,
            external_link: None,
            ..scrobble(Visibility::Public)
        };
        let audio = audio_json(&row, &author(), FOLLOWERS, &config());

        assert_eq!(
            json_keys(&audio),
            [
                "actor",
                "attributedTo",
                "cc",
                "context",
                "id",
                "published",
                "title",
                "to",
                "type"
            ],
            "only the keys with a value"
        );
    }

    #[test]
    fn a_row_without_an_ap_id_gets_the_minted_activity_id() {
        let row = Scrobble {
            ap_id: None,
            ..scrobble(Visibility::Public)
        };
        let listen = listen_json(&row, &author(), FOLLOWERS, &config());

        assert_eq!(
            listen["id"],
            format!("{BASE}/activities/{SCROBBLE_ID}"),
            "the shape decision 6 mints"
        );
    }
}
