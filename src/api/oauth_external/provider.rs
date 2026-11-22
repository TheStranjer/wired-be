//! The provider leg of an external login: where one callback's
//! identification landed ([`Landing`]) and the steps that decide it — the
//! code the provider offered, the token exchange, the userinfo read and the
//! identity lookup ([`identified`]) — beside the configured provider's own
//! URLs and parameters, the account its document describes
//! ([`ProviderAccount`]), and the finished login [`granted`] hands back.

use serde_json::{Value, json};
use tracing::warn;

use crate::{
    api::oauth::{
        authorize::{self, AuthorizeReply},
        check_account_ready, mfa,
    },
    config::{Config, OauthProvider},
    error::{AppError, AppResult},
    http_client::{HttpClient, HttpError, HttpResponse},
    models::{user::User, user_identity},
    oauth::scopes::Scopes,
    state::AppState,
};

use super::{
    ACCEPT_JSON, CALLBACK_SEGMENT, FORM_TYPE, OAUTH_PATH, UNKNOWN_PROVIDER, USERINFO_MAX_BYTES,
    state::{CallbackQuery, StoredParams},
};

/// Where one callback's identification landed.
#[derive(Debug)]
pub(super) enum Landing {
    /// The provider refused, or its answer could not be used: send the app
    /// `error=access_denied`.
    Denied,
    /// The provider account is linked to this local user, who is now
    /// logged in. The row is boxed: it is the widest thing in the flow's
    /// answer, and `Denied` and `Unlinked` are the ones that pay for it.
    Linked(Box<User>),
    /// Nobody here knows the provider account; its document is the
    /// registration form's to work with.
    Unlinked(ProviderAccount),
}

/// The provider's answer, in order: a code (its own `error` parameter
/// counts as the absence of one), the token exchange, the userinfo read,
/// then the identity lookup.
pub(super) async fn identified(
    state: &AppState,
    provider: &OauthProvider,
    query: CallbackQuery,
) -> AppResult<Landing> {
    let Some(code) = offered_code(query.error.as_deref(), query.code.as_deref()) else {
        return Ok(Landing::Denied);
    };
    let callback_uri = provider_callback(&state.config, &provider.name);
    let Some(access_token) = exchanged(state, provider, &callback_uri, code).await else {
        return Ok(Landing::Denied);
    };
    let Some(account) = read_account(&state.http_client, provider, &access_token).await else {
        return Ok(Landing::Denied);
    };
    let found = user_identity::find_user(&state.db, &provider.name, &account.uid).await?;
    Ok(found.map_or(Landing::Unlinked(account), |user| {
        Landing::Linked(Box::new(user))
    }))
}

/// The code the provider offered: `None`, with the reason logged, when it
/// sent an `error` parameter instead of a code, or an empty one.
fn offered_code<'a>(error: Option<&'a str>, code: Option<&'a str>) -> Option<&'a str> {
    if let Some(refusal) = error {
        warn!(provider_error = refusal, "external OAuth provider refused");
        return None;
    }
    code.filter(|text| !text.is_empty())
}

/// The login a finished flow earned, from the callback's linked user or a
/// completed registration: the stored application, redirect URI and
/// scopes re-checked the way the consent submission checks its round-tripped
/// form, then the readiness gate, then the very same code and delivery.
///
/// # Errors
///
/// 403 `account_is_disabled` from
/// [`crate::api::oauth`]'s account gate, a 400 refusal of the stored
/// application, redirect URI or scopes, or any database failure as
/// [`AppError::Database`].
pub(super) async fn granted(
    state: &AppState,
    user: &User,
    params: &StoredParams,
) -> AppResult<AuthorizeReply> {
    let app = authorize::load_app(&state.db, Some(params.client_id.as_str())).await?;
    authorize::require_registered_redirect(&app, &params.redirect_uri)?;
    let scopes = Scopes::parse(&params.scope);
    authorize::check_scopes_subset(&scopes, &app)?;
    check_account_ready(user)?;
    if mfa::required(user) {
        return authorize::mfa_detour(state, user, &app, &scopes, &params.delivery()).await;
    }
    authorize::grant_code(state, user, &app, &scopes, &params.delivery()).await
}

/// The configured provider a request names, or the 404 saying it does not
/// exist. The reference lives as long as the state's configuration.
pub(super) fn configured_provider<'a>(
    state: &'a AppState,
    name: Option<&str>,
) -> AppResult<&'a OauthProvider> {
    state
        .config
        .oauth_provider(name.unwrap_or_default())
        .ok_or_else(|| AppError::NotFoundMessage(UNKNOWN_PROVIDER.to_owned()))
}

/// `{BASE_URL}/oauth/{provider}`: the entry point `prepare_request` answers.
pub(super) fn provider_entry(config: &Config, name: &str) -> String {
    format!("{}{OAUTH_PATH}/{name}", config.base_url)
}

/// `{BASE_URL}/oauth/{provider}/callback`: the `redirect_uri` the provider
/// is told to answer to, and the URL its code is exchanged against.
pub(super) fn provider_callback(config: &Config, name: &str) -> String {
    format!("{}/{}", provider_entry(config, name), CALLBACK_SEGMENT)
}

/// The parameters of the provider's authorization URL, in Pleroma's
/// `Ueberauth` spelling: the configured application, this instance's
/// callback, the code flow, the provider's own scopes and the state that
/// ties the answer back to the prepared request.
pub(super) const fn provider_params<'a>(
    provider: &'a OauthProvider,
    callback_uri: &'a str,
    state: &'a str,
) -> [(&'a str, &'a str); 5] {
    [
        ("client_id", provider.client_id.as_str()),
        ("redirect_uri", callback_uri),
        ("response_type", authorize::RESPONSE_TYPE_CODE),
        ("scope", provider.scopes.as_str()),
        ("state", state),
    ]
}

/// `POST`s the code to the provider's token endpoint and reads its
/// `access_token`, `None` when the exchange failed in any way.
async fn exchanged(
    state: &AppState,
    provider: &OauthProvider,
    callback_uri: &str,
    code: &str,
) -> Option<String> {
    let body = StoredParams::token_form(provider, callback_uri, code);
    let outcome = state
        .http_client
        .post_bytes(
            &provider.token_url,
            &[("content-type", FORM_TYPE), ("accept", ACCEPT_JSON)],
            body.into_bytes(),
        )
        .await;
    let document = json_document("token", &provider.token_url, outcome)?;
    text_at(&document, "access_token").or_else(|| {
        warn!(
            provider = provider.name,
            "provider token answer carries no access_token"
        );
        None
    })
}

/// `GET`s the provider's userinfo document with the bearer token and reads
/// the account out of it, `None` when the read or the account failed.
async fn read_account(
    http: &HttpClient,
    provider: &OauthProvider,
    access_token: &str,
) -> Option<ProviderAccount> {
    let bearer = format!("Bearer {access_token}");
    let outcome = http
        .get_limited(
            &provider.userinfo_url,
            &[("authorization", bearer.as_str()), ("accept", ACCEPT_JSON)],
            USERINFO_MAX_BYTES,
        )
        .await;
    let document = json_document("userinfo", &provider.userinfo_url, outcome)?;
    ProviderAccount::of_document(provider, &document).or_else(|| {
        warn!(
            provider = provider.name,
            uid_field = provider.uid_field,
            "provider userinfo names no account id"
        );
        None
    })
}

/// Reads a provider answer as a JSON document, logging (never sending) the
/// reason it is not one.
fn json_document(role: &str, url: &str, outcome: Result<HttpResponse, HttpError>) -> Option<Value> {
    let response = match outcome.and_then(HttpResponse::error_for_status) {
        Err(err) => {
            warn!(call = role, %url, reason = %err, "external OAuth provider call failed");
            return None;
        }
        Ok(response) => response,
    };
    match response.json::<Value>() {
        Ok(document) => Some(document),
        Err(err) => {
            warn!(call = role, %url, reason = %err, "external OAuth provider answered no JSON");
            None
        }
    }
}

/// One provider account, as its userinfo document described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProviderAccount {
    /// The stable provider id `uid_field` names; the `user_identities` key.
    uid: String,
    /// `nickname_field`, when the provider says it.
    nickname: Option<String>,
    /// `email_field`, when the provider says it.
    email: Option<String>,
    /// `name_field`, when the provider says it.
    name: Option<String>,
}

impl ProviderAccount {
    /// The account `document` describes under `provider`'s configured field
    /// names. Without a `uid_field` value there is no account to speak of;
    /// the three optional fields are simply absent.
    fn of_document(provider: &OauthProvider, document: &Value) -> Option<Self> {
        Some(Self {
            uid: text_at(document, &provider.uid_field)?,
            nickname: text_at(document, &provider.nickname_field),
            email: text_at(document, &provider.email_field),
            name: text_at(document, &provider.name_field),
        })
    }
}

/// The `params.pending` document of an unlinked callback: everything the
/// registration form needs to prefill, under the state that earned it.
pub(super) fn pending_of(provider: &str, account: &ProviderAccount) -> Value {
    json!({
        "provider": provider,
        "uid": account.uid,
        "nickname": account.nickname,
        "email": account.email,
        "name": account.name,
    })
}

/// The text at the dotted path `path` of `document`: nested objects are dug
/// through key by key, a number reads as its own text (GitHub numbers its
/// account ids), and anything else — an absent key, an empty string, a list
/// or an object — is no value.
fn text_at(document: &Value, path: &str) -> Option<String> {
    let found = path
        .split('.')
        .filter(|key| !key.is_empty())
        .try_fold(document, |step, key| step.get(key))?;
    match found {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::String(_) | Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::oauth_external::test_support::{provider, query_pairs},
        config::BASE_URL_VAR,
    };

    #[test]
    fn the_provider_redirect_url_names_the_flow_and_the_callback() {
        let config = Config::for_tests();
        let github = provider("github");
        let callback_uri = provider_callback(&config, "github");
        let url = authorize::with_query_params(
            &github.authorize_url,
            &provider_params(&github, &callback_uri, "issued-state"),
        );
        let (base, _) = url.split_once('?').expect("a query string");
        assert_eq!(
            base, "https://github.example/authorize",
            "the configured endpoint"
        );
        assert_eq!(
            query_pairs(&url),
            vec![
                ("client_id".to_owned(), "github-client-id".to_owned()),
                ("redirect_uri".to_owned(), callback_uri),
                (
                    "response_type".to_owned(),
                    authorize::RESPONSE_TYPE_CODE.to_owned()
                ),
                ("scope".to_owned(), "read:user user:email".to_owned()),
                ("state".to_owned(), "issued-state".to_owned()),
            ],
            "the client id, our callback, the code flow, the provider's scopes, our state"
        );
    }

    #[test]
    fn a_provider_redirect_url_merges_into_an_existing_query() {
        let config = Config::for_tests();
        let mut github = provider("github");
        github.authorize_url = "https://github.example/authorize?foo=1".to_owned();
        let url = authorize::with_query_params(
            &github.authorize_url,
            &provider_params(&github, &provider_callback(&config, "github"), "s"),
        );
        assert_eq!(
            url,
            "https://github.example/authorize?foo=1&client_id=github-client-id&redirect_uri=https%3A%2F%2Ftest.example%2Foauth%2Fgithub%2Fcallback&response_type=code&scope=read%3Auser+user%3Aemail&state=s",
            "the operator's own parameters are kept and the values urlencoded"
        );
    }

    #[test]
    fn entry_and_callback_urls_hang_off_the_base_url() {
        let config = Config::from_lookup(|key| match key {
            crate::config::DATABASE_URL_VAR => Some("postgres://localhost/wired".to_owned()),
            BASE_URL_VAR => Some("https://social.example:4000/".to_owned()),
            _ => None,
        })
        .expect("a valid config");
        assert_eq!(
            provider_entry(&config, "github"),
            "https://social.example:4000/oauth/github",
            "the entry point follows BASE_URL, port included"
        );
        assert_eq!(
            provider_callback(&config, "github"),
            "https://social.example:4000/oauth/github/callback",
            "and so does the provider's answer target"
        );
    }

    #[test]
    fn userinfo_fields_are_dug_out_by_dotted_path() {
        let document = json!({
            "id": 4242,
            "login": "octocat",
            "name": "",
            "email": null,
            "profile": {"bio": "  hi  ", "tags": ["a"]},
            "flags": true,
        });
        assert_eq!(
            text_at(&document, "id").as_deref(),
            Some("4242"),
            "a number reads as its own text"
        );
        assert_eq!(
            text_at(&document, "login").as_deref(),
            Some("octocat"),
            "a string reads as itself"
        );
        assert_eq!(
            text_at(&document, "profile.bio").as_deref(),
            Some("  hi  "),
            "a nested string is dug out by its dotted path, verbatim"
        );
        for quiet in ["name", "email", "missing", "profile.tags", "flags", "", "."] {
            assert_eq!(
                text_at(&document, quiet),
                None,
                "`{quiet}` names no usable text"
            );
        }
    }

    #[test]
    fn the_account_is_the_configured_fields_of_the_document() {
        let mut keyed = provider("github");
        keyed.uid_field = "data.user.id".to_owned();
        keyed.nickname_field = "data.user.login".to_owned();
        keyed.email_field = String::new();
        keyed.name_field = "absent".to_owned();
        let document = json!({"data": {"user": {"id": 7, "login": "octo"}}});
        let account = ProviderAccount::of_document(&keyed, &document).expect("the nested id");
        assert_eq!(account.uid, "7", "the dotted uid_field");
        assert_eq!(account.nickname.as_deref(), Some("octo"), "the dotted name");
        assert_eq!(account.email, None, "an empty field name says nothing");
        assert_eq!(account.name, None, "an absent field says nothing");

        assert!(
            ProviderAccount::of_document(&keyed, &json!({"login": "octo"})).is_none(),
            "without a uid there is no account"
        );
    }

    #[test]
    fn the_pending_document_carries_every_field() {
        let account = ProviderAccount {
            uid: "4242".to_owned(),
            nickname: Some("octocat".to_owned()),
            email: None,
            name: Some("The Octocat".to_owned()),
        };
        assert_eq!(
            pending_of("github", &account),
            json!({
                "provider": "github",
                "uid": "4242",
                "nickname": "octocat",
                "email": null,
                "name": "The Octocat",
            }),
            "the registration form reads a null as 'the provider did not say'"
        );
    }

    #[test]
    fn an_error_parameter_is_a_code_the_provider_never_sent() {
        assert_eq!(
            offered_code(None, Some("the-code")),
            Some("the-code"),
            "a plain code is offered"
        );
        assert_eq!(offered_code(None, None), None, "and nothing otherwise");
        assert_eq!(
            offered_code(None, Some("")),
            None,
            "an empty code is nothing"
        );
        assert_eq!(
            offered_code(Some("access_denied"), Some("the-code")),
            None,
            "a refusal wins over a code sent beside it"
        );
    }
}
