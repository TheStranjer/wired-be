//! The state side of an external login: the query bodies the routes
//! deserialize, the reply `prepare_request` answers with, the
//! `/oauth/authorize` parameters an `oauth_login_states` row carries across
//! the provider round trip ([`StoredParams`]), the state check a redirect or
//! a callback runs on ([`live_state`]), and the four flow halves —
//! [`prepared`], [`redirected`], [`completed`] and [`details`] — the route
//! handlers of the parent run. The provider leg they call into is the
//! sibling `provider` child's; the registration form's own data is `form`'s.

use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;
use url::form_urlencoded;

use crate::{
    api::oauth::authorize::{self, AuthorizeReply, Delivery},
    config::OauthProvider,
    error::{AppError, AppResult},
    models::oauth_login_state::{self, OauthLoginState},
    oauth::scopes::Scopes,
    state::AppState,
};

use super::{
    GRANT_TYPE_CODE, INVALID_STATE, REGISTRATION_DETAILS,
    provider::{
        Landing, configured_provider, granted, identified, pending_of, provider_callback,
        provider_entry, provider_params,
    },
    registration::{pending_form, registrable_state},
};

/// The `GET /oauth/prepare_request` parameters: the provider name beside
/// the `/oauth/authorize` fields whose validation it shares. Every field is
/// optional at the shape level — the endpoint answers the missing ones with
/// its own bodies, exactly as the sibling endpoint does.
#[derive(Debug, Deserialize)]
pub(super) struct PrepareQuery {
    /// Which configured provider to prepare the login for.
    #[serde(default)]
    provider: Option<String>,
    /// The flow; only `code` is spoken here.
    #[serde(default)]
    response_type: Option<String>,
    /// The registered application's public id.
    #[serde(default)]
    client_id: Option<String>,
    /// Where the finished login is to be delivered; must be registered.
    #[serde(default)]
    redirect_uri: Option<String>,
    /// Requested Mastodon scopes, space- or `+`-separated.
    #[serde(default)]
    scope: Option<String>,
    /// Opaque client value, round-tripped to `redirect_uri`.
    #[serde(default)]
    state: Option<String>,
}

/// The `GET /oauth/{provider}` parameters.
#[derive(Debug, Deserialize)]
pub(super) struct StateQuery {
    /// The state [`prepare_request`] issued.
    ///
    /// [`prepare_request`]: super::prepare_request
    #[serde(default)]
    pub(super) state: Option<String>,
}

/// The `GET /oauth/{provider}/callback` parameters. `code` and `state` are
/// what this server asks the provider to send; `error` is what a provider
/// that refused the login sends instead.
#[derive(Debug, Deserialize)]
pub(super) struct CallbackQuery {
    /// The provider's one-time code for the token exchange.
    #[serde(default)]
    pub(super) code: Option<String>,
    /// The state [`prepare_request`] issued.
    ///
    /// [`prepare_request`]: super::prepare_request
    #[serde(default)]
    pub(super) state: Option<String>,
    /// The provider's own refusal, whatever its text.
    #[serde(default)]
    pub(super) error: Option<String>,
}

/// The 200 body of `GET /oauth/prepare_request`: the one URL that starts
/// the round trip.
#[derive(Debug, Serialize)]
pub(super) struct PrepareReply {
    /// `{BASE_URL}/oauth/{provider}?state={state}`.
    url: String,
}

/// The `/oauth/authorize` parameters an `oauth_login_states` row carries
/// across the provider round trip: what to mint the code for and where to
/// deliver it. The `pending` key an unlinked callback stores beside these
/// is the registration flow's document ([`PendingRegistration`]), so it is
/// not a field here.
///
/// [`PendingRegistration`]: super::form::PendingRegistration
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredParams {
    /// The registered application the finished login is for.
    #[serde(default)]
    pub(super) client_id: String,
    /// Where the code is delivered; validated when it was stored.
    #[serde(default)]
    pub(super) redirect_uri: String,
    /// The scopes validated at prepare time, granted verbatim.
    #[serde(default)]
    pub(super) scope: String,
    /// The client's own `state`, echoed back with the code. Absent from the
    /// stored document when the client sent none, rather than a null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) state: Option<String>,
}

impl StoredParams {
    /// The parameters of a validated prepare request: the application
    /// fields as checked, the scopes in their granted spelling.
    fn of_query(query: &PrepareQuery, scopes: &Scopes) -> Self {
        Self {
            client_id: query.client_id.clone().unwrap_or_default(),
            redirect_uri: query.redirect_uri.clone().unwrap_or_default(),
            scope: scopes.to_string(),
            state: query.state.clone(),
        }
    }

    /// The stored document of a live state, or the 400 `Invalid state` a
    /// row whose parameters cannot be read amounts to: nothing can be
    /// delivered on it.
    pub(super) fn of_state(stored: &OauthLoginState) -> AppResult<Self> {
        serde_json::from_value(stored.params.clone())
            .map_err(|_unreadable| AppError::BadRequest(INVALID_STATE.to_owned()))
    }

    /// Where the finished (or refused) login goes: the stored redirect URI
    /// and the client's stored state, in the shape [`authorize::grant_code`]
    /// and [`authorize::denied`] answer from.
    pub(super) fn delivery(&self) -> Delivery<'_> {
        Delivery {
            redirect_uri: self.redirect_uri.as_str(),
            state: self.state.as_deref(),
        }
    }

    /// The `grant_type=authorization_code` body for the provider's token
    /// endpoint: its credentials, the code it just sent, and the callback
    /// URL it saw at authorize time.
    pub(super) fn token_form(provider: &OauthProvider, callback_uri: &str, code: &str) -> String {
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("grant_type", GRANT_TYPE_CODE),
                ("code", code),
                ("redirect_uri", callback_uri),
                ("client_id", provider.client_id.as_str()),
                ("client_secret", provider.client_secret.as_str()),
            ])
            .finish()
    }
}

/// The prepare half in order: provider, flow, application, redirect URI,
/// scopes — then the stored state and the URL that names it.
pub(super) async fn prepared(state: &AppState, query: PrepareQuery) -> AppResult<PrepareReply> {
    let provider = configured_provider(state, query.provider.as_deref())?;
    authorize::require_code_response(query.response_type.as_deref())?;
    let app = authorize::load_app(&state.db, query.client_id.as_deref()).await?;
    let redirect_uri = query.redirect_uri.as_deref().unwrap_or_default();
    authorize::require_registered_redirect(&app, redirect_uri)?;
    let scopes = authorize::requested_scopes(query.scope.as_deref());
    authorize::check_scopes_subset(&scopes, &app)?;
    let params = StoredParams::of_query(&query, &scopes);
    let issued = oauth_login_state::create(&state.db, &provider.name, &json!(params)).await?;
    Ok(PrepareReply {
        url: authorize::with_query_params(
            &provider_entry(&state.config, &provider.name),
            &[("state", issued.state.as_str())],
        ),
    })
}

/// The redirect half: the state must be live and issued to this provider,
/// and then it is only a matter of spelling out the provider's own
/// authorization URL.
pub(super) async fn redirected(
    state: &AppState,
    name: &str,
    query: StateQuery,
) -> AppResult<AuthorizeReply> {
    let provider = configured_provider(state, Some(name))?;
    let stored = live_state(state, name, query.state.as_deref()).await?;
    let callback_uri = provider_callback(&state.config, &provider.name);
    authorize::redirect_to(
        &provider.authorize_url,
        &provider_params(provider, &callback_uri, &stored.state),
    )
}

/// The callback half in order: state, stored parameters, the provider's
/// answer.
pub(super) async fn completed(
    state: &AppState,
    name: &str,
    query: CallbackQuery,
) -> AppResult<AuthorizeReply> {
    let provider = configured_provider(state, Some(name))?;
    let stored = live_state(state, name, query.state.as_deref()).await?;
    let params = StoredParams::of_state(&stored)?;
    match identified(state, provider, query).await? {
        Landing::Denied => authorize::denied(&params.delivery()),
        Landing::Linked(user) => granted(state, &user, &params).await,
        Landing::Unlinked(account) => {
            let pending = pending_of(&provider.name, &account);
            if !oauth_login_state::set_pending(&state.db, &stored.state, &pending).await? {
                warn!(
                    provider = provider.name,
                    "oauth login state went away mid-callback"
                );
            }
            authorize::redirect_to(
                &format!("{}{REGISTRATION_DETAILS}", state.config.base_url),
                &[("state", stored.state.as_str())],
            )
        }
    }
}

/// The `GET /oauth/registration_details` half: the state's `pending`
/// account, prefilled into the form.
///
/// # Errors
///
/// 400 `Invalid state` from [`registrable_state`]; any database failure
/// arrives as [`AppError::Database`].
///
/// [`registrable_state`]: super::registration::registrable_state
pub(super) async fn details(
    state: &AppState,
    presented: Option<&str>,
) -> AppResult<AuthorizeReply> {
    let (stored, pending, _) = registrable_state(state, presented).await?;
    Ok(pending_form(&stored, &pending))
}

/// The state a redirect or a callback may run on: live, and issued to this
/// very provider.
///
/// # Errors
///
/// 400 `Invalid state` for a missing, unknown, expired or foreign state;
/// any database failure arrives as [`AppError::Database`].
async fn live_state(
    state: &AppState,
    name: &str,
    presented: Option<&str>,
) -> AppResult<OauthLoginState> {
    let invalid = || AppError::BadRequest(INVALID_STATE.to_owned());
    let Some(presented) = presented.filter(|text| !text.is_empty()) else {
        return Err(invalid());
    };
    oauth_login_state::get_valid(&state.db, presented, name)
        .await?
        .ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::api::oauth_external::test_support::{provider, query_pairs};

    /// A stored-parameters document with the fields `keep` overrides.
    fn stored_params() -> StoredParams {
        StoredParams {
            client_id: "client-id".to_owned(),
            redirect_uri: "https://client.example/callback".to_owned(),
            scope: "read".to_owned(),
            state: Some("client-state".to_owned()),
        }
    }

    #[test]
    fn a_validated_query_stores_the_app_parameters_verbatim() {
        let query: PrepareQuery = crate::extract::into_params(json!({
            "provider": "github",
            "response_type": "code",
            "client_id": "client-id",
            "redirect_uri": "https://client.example/callback",
            "scope": "read write",
            "state": "client-state",
        }))
        .expect("every field is optional at the shape level");
        let params = StoredParams::of_query(&query, &Scopes::parse("read write"));
        assert_eq!(params.client_id, "client-id", "the application");
        assert_eq!(
            params.redirect_uri, "https://client.example/callback",
            "the delivery target"
        );
        assert_eq!(params.scope, "read write", "the granted scopes");
        assert_eq!(params.state.as_deref(), Some("client-state"), "the echo");

        let round_trip = serde_json::from_value::<StoredParams>(json!(params))
            .expect("the stored document reads back");
        assert_eq!(round_trip, params, "serialize and deserialize round-trip");

        let empty: PrepareQuery = crate::extract::into_params(json!({})).unwrap();
        let defaults = StoredParams::of_query(&empty, &Scopes::default());
        assert_eq!(defaults.client_id, "", "an absent app id stores empty");
        assert_eq!(defaults.scope, "read", "no scope asked is the default");
        assert_eq!(defaults.state, None, "and no client state echoes");
    }

    #[test]
    fn the_delivery_carries_the_stored_target_and_state() {
        let params = stored_params();
        let delivery = params.delivery();
        assert_eq!(
            delivery.redirect_uri, "https://client.example/callback",
            "the stored redirect URI"
        );
        assert_eq!(delivery.state, Some("client-state"), "the stored echo");
        let silent = StoredParams {
            state: None,
            ..params
        };
        assert_eq!(silent.delivery().state, None, "no echo when none was sent");
    }

    #[test]
    fn stored_parameters_read_back_out_of_a_state_row() {
        let row = OauthLoginState {
            state: "a-state".to_owned(),
            provider: "github".to_owned(),
            params: json!({"client_id": "c", "redirect_uri": "r", "scope": "read"}),
            valid_until: Utc::now(),
        };
        let params = StoredParams::of_state(&row).expect("the document is readable");
        assert_eq!(params.client_id, "c", "the stored application");
        assert_eq!(params.redirect_uri, "r", "the stored target");
        assert_eq!(params.scope, "read", "the stored scopes");
        assert_eq!(params.state, None, "an unstored echo is absent");

        // A `pending` key from an unlinked callback rides along unread: it
        // is [`PendingRegistration`]'s document, not this one.
        let pending = OauthLoginState {
            params: json!({"client_id": "c", "redirect_uri": "r", "pending": {"uid": "1"}}),
            ..row
        };
        assert_eq!(
            StoredParams::of_state(&pending)
                .expect("an extra key does not break the read")
                .redirect_uri,
            "r",
            "the authorize parameters still come through"
        );

        for broken in [
            json!(null),
            json!("nope"),
            json!([1, 2]),
            json!({"scope": 7}),
        ] {
            let bad = OauthLoginState {
                params: broken,
                ..pending.clone()
            };
            let err = StoredParams::of_state(&bad)
                .expect_err("a document of that shape is no parameters");
            assert_eq!(err.to_string(), INVALID_STATE, "refused as a bad state");
        }
    }

    #[test]
    fn the_token_form_carries_the_provider_credentials_and_the_code() {
        let form = StoredParams::token_form(
            &provider("github"),
            "https://test.example/oauth/github/callback",
            "the code",
        );
        assert_eq!(
            form,
            "grant_type=authorization_code&code=the+code&redirect_uri=https%3A%2F%2Ftest.example%2Foauth%2Fgithub%2Fcallback&client_id=github-client-id&client_secret=github-client-secret",
            "five urlencoded fields, in order"
        );
        assert_eq!(
            query_pairs(&format!("x?{form}")).len(),
            5,
            "and it parses back to exactly those five fields"
        );
    }

    #[test]
    fn the_prepare_reply_is_the_one_url_key() {
        let json = serde_json::to_value(PrepareReply {
            url: "https://test.example/oauth/github?state=s".to_owned(),
        })
        .expect("a one-field struct serializes");
        assert_eq!(
            json,
            json!({"url": "https://test.example/oauth/github?state=s"}),
            "exactly the `url` key the ticket pins"
        );
    }
}
