//! The Mastodon `Application` entity: a registered client application.
//!
//! One shape serves both app endpoints (`src/api/apps.rs` owns the
//! routes): `POST /api/v1/apps` answers with the entity *with* its OAuth
//! credentials — Pleroma's `show.json`
//! (`lib/pleroma/web/mastodon_api/views/app_view.ex`) extended with the
//! Mastodon fields — and `GET /api/v1/apps/verify_credentials` answers
//! with the same entity minus the secret fields
//! (`client_id`, `client_secret`, `client_secret_expires_at`), which is
//! what [`Application::from_app`] with `with_secret = false` leaves out
//! of the JSON entirely. `vapid_key` is emitted as `null` both ways;
//! the web-push ticket fills it in.

use serde::Serialize;

use crate::models::oauth_app::OauthApp;

/// `client_secret_expires_at` when the secret is shown: these secrets
/// never expire, which Mastodon spells `0`.
const NEVER_EXPIRES: i64 = 0;

/// The `Application` entity, with or without the OAuth credentials.
#[derive(Debug, Serialize)]
pub struct Application {
    /// The app's `UUIDv7` id as lowercase hyphenated text (decision 4).
    pub id: String,
    /// The registered application name (`client_name`).
    pub name: String,
    /// The application homepage, `null` when unset.
    pub website: Option<String>,
    /// The redirect URIs, exactly as stored.
    pub redirect_uri: String,
    /// The public OAuth client id; only present when `with_secret`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// The OAuth client secret; only present when `with_secret`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// `0`: client secrets do not expire; only present when
    /// `with_secret`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<i64>,
    /// Web-push public key; `null` until the push ticket fills it in.
    pub vapid_key: Option<String>,
    /// The app's registered scopes as a list.
    pub scopes: Vec<String>,
}

impl Application {
    /// Renders the stored row as the entity.
    ///
    /// `with_secret` selects the registration shape: the credential
    /// fields are serialized; when it is `false` they are left out of
    /// the JSON entirely, which is the `verify_credentials` shape.
    #[must_use]
    pub fn from_app(app: OauthApp, with_secret: bool) -> Self {
        Self {
            id: app.id.to_string(),
            name: app.client_name,
            website: app.website,
            redirect_uri: app.redirect_uris,
            client_id: with_secret.then(|| app.client_id.clone()),
            client_secret: with_secret.then_some(app.client_secret),
            client_secret_expires_at: with_secret.then_some(NEVER_EXPIRES),
            vapid_key: None,
            scopes: scope_list(&app.scopes),
        }
    }
}

/// The stored scope string as the JSON list the API returns.
fn scope_list(stored: &str) -> Vec<String> {
    stored
        .split(' ')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::{json, to_value};
    use uuid::Uuid;

    use super::*;

    /// A stored app with recognizable values in every column the
    /// entity reads.
    fn stored_app() -> OauthApp {
        OauthApp {
            id: Uuid::now_v7(),
            client_name: "Entity".to_owned(),
            redirect_uris: "https://a.example/cb\nhttps://b.example/cb".to_owned(),
            scopes: "read write".to_owned(),
            website: None,
            client_id: "cid".to_owned(),
            client_secret: "csec".to_owned(),
            trusted: false,
            user_id: None,
            inserted_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn with_secret_renders_the_registration_entity() {
        let app = stored_app();
        let id = app.id.to_string();
        let json = to_value(Application::from_app(app, true)).unwrap();
        assert_eq!(json["id"], json!(id), "the uuid as hyphenated text");
        assert_eq!(json["name"], "Entity", "client_name becomes name");
        assert!(json["website"].is_null(), "an unset website is null");
        assert_eq!(
            json["redirect_uri"], "https://a.example/cb\nhttps://b.example/cb",
            "redirect_uri is the stored string"
        );
        assert_eq!(json["client_id"], "cid", "the client id is handed back");
        assert_eq!(
            json["client_secret"], "csec",
            "the client secret is handed back"
        );
        assert_eq!(json["client_secret_expires_at"], 0, "secrets do not expire");
        assert!(json["vapid_key"].is_null(), "vapid_key is null for now");
        assert_eq!(json["scopes"], json!(["read", "write"]), "the scope list");
    }

    #[test]
    fn without_secret_the_credential_keys_are_absent() {
        let json = to_value(Application::from_app(stored_app(), false)).unwrap();
        let fields = json.as_object().expect("the entity is an object");
        for secret in ["client_id", "client_secret", "client_secret_expires_at"] {
            assert!(
                !fields.contains_key(secret),
                "`{secret}` must not appear at all"
            );
        }
        assert_eq!(
            fields.len(),
            6,
            "id, name, website, redirect_uri, vapid_key, scopes"
        );
    }

    #[test]
    fn the_open_shape_equals_the_secret_shape_minus_the_secrets() {
        let app = stored_app();
        let mut registration = to_value(Application::from_app(app.clone(), true))
            .unwrap()
            .as_object()
            .expect("the entity is an object")
            .clone();
        for secret in ["client_id", "client_secret", "client_secret_expires_at"] {
            let shown = registration.remove(secret);
            assert!(shown.is_some(), "the registration carries `{secret}`");
        }
        let verified = to_value(Application::from_app(app, false)).unwrap();
        assert_eq!(
            verified.as_object().expect("an object"),
            &registration,
            "verify_credentials is the registration response minus the secrets"
        );
    }

    #[test]
    fn the_default_scope_string_is_a_one_item_list() {
        let app = OauthApp {
            scopes: "read".to_owned(),
            ..stored_app()
        };
        let json = to_value(Application::from_app(app, false)).unwrap();
        assert_eq!(json["scopes"], json!(["read"]), "a lone scope");
    }

    #[test]
    fn an_empty_scope_string_is_an_empty_list() {
        assert!(scope_list("").is_empty(), "nothing between the blanks");
    }

    #[test]
    fn a_set_website_round_trips() {
        let app = OauthApp {
            website: Some("https://entity.example".to_owned()),
            ..stored_app()
        };
        let json = to_value(Application::from_app(app, true)).unwrap();
        assert_eq!(json["website"], "https://entity.example", "the homepage");
    }
}
