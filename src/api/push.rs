//! Web Push subscription management: `/api/v1/push/subscription`
//! (WBE-144).
//!
//! The reference is Pleroma's `SubscriptionController`
//! (`lib/pleroma/web/mastodon_api/controllers/subscription_controller.ex`)
//! and `Pleroma.Web.Push.Subscription`
//! (`lib/pleroma/web/push/subscription.ex`). Every route authenticates
//! with [`AuthUser`] and demands the `push` scope (Pleroma's
//! `OAuthScopesPlug %{scopes: ["push"]}` on the controller), and every
//! read, update and delete is keyed by the request's **access token**, not
//! the account: one token has one subscription, two tokens of one account
//! have two. What lives where follows decision 3: the row is
//! [`crate::models::web_push_subscription`], the response is
//! [`crate::entities::push_subscription`], and this module is the four
//! routes, the [`ApiBody`] parameter shapes (decision 10) and the
//! validation.
//!
//! - `POST /api/v1/push/subscription` registers `subscription[endpoint]`
//!   (an `http`/`https` URL of at most 2048 characters whose host the
//!   outbound SSRF guard would let a delivery reach: a loopback, private
//!   or otherwise reserved address is refused, since every delivery to it
//!   would be blocked and a subscription is attacker-chosen input; only
//!   an instance that runs with `HTTP_ALLOW_PRIVATE_HOSTS` — the
//!   deliberate local-delivery mode — accepts such endpoints) with
//!   `subscription[keys][p256dh]` and `subscription[keys][auth]` (stored
//!   base64url-normalised, Pleroma's `ensure_base64_urlsafe`), plus the
//!   optional `data[alerts][…]` preferences and `data[policy]`. It
//!   replaces any subscription the token already had; a missing endpoint
//!   or key, an unusable, overlong or blocked endpoint and an unknown
//!   policy are 422 `{"error":"Validation failed: …"}`.
//! - `GET /api/v1/push/subscription` answers the token's subscription or
//!   404 `{"error":"Record not found"}`.
//! - `PUT /api/v1/push/subscription` updates `data[alerts][…]` (a named
//!   flag moves, an unnamed one stays) and `data[policy]`; 404 when the
//!   token has none.
//! - `DELETE /api/v1/push/subscription` removes it and answers `{}`;
//!   404 when there is nothing to remove.
//!
//! Every response carries `server_key`: the instance VAPID public key
//! (see [`crate::services::vapid`]).

use axum::{Json, Router, extract::State, routing::get};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use crate::{
    auth::AuthUser,
    entities::push_subscription::{PushSubscription, merge_alerts},
    error::{AppError, AppResult},
    extract::{ApiBody, flex},
    http_client::{HttpClient, HttpError},
    models::web_push_subscription::{self, DEFAULT_POLICY, NewSubscription},
    oauth::scopes::PUSH,
    services::vapid,
    state::AppState,
    util::url::is_http_url,
};

/// Longest accepted push endpoint URL, in characters (Mastodon caps
/// `PushSubscription#endpoint` at 2048).
const MAX_ENDPOINT_CHARS: usize = 2048;

/// The notification policies `data[policy]` may name (Mastodon's
/// `PushSubscription::ALLOWED_POLICIES`).
const POLICIES: [&str; 4] = ["all", "followed", "follower", "none"];

/// The `/api/v1/push` router, merged into [`crate::api::v1`].
///
/// Do not call [`Router::fallback`] here: unmatched paths must reach the
/// app-wide JSON fallback in [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new().route(
        "/push/subscription",
        get(show_subscription)
            .post(create_subscription)
            .put(update_subscription)
            .delete(delete_subscription),
    )
}

/// `POST /api/v1/push/subscription` — register the token's subscription,
/// replacing an earlier one, and answer the entity.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser`] and
/// the `push` check; [`Unprocessable`](crate::error::AppError::Unprocessable)
/// for the validation failures the [module docs](self) list (decision 8
/// carries it as `{"error":"Validation failed: …"}`). Any database failure
/// arrives through `?` as [`Database`](crate::error::AppError::Database).
async fn create_subscription(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<SubscriptionParams>,
) -> AppResult<Json<PushSubscription>> {
    auth.require(PUSH)?;
    let registered = NewSubscription {
        user_id: auth.0.id,
        token_id: auth.1.id,
        endpoint: valid_endpoint(&params)?,
        key_p256dh: valid_key(params.keys().p256dh.as_deref(), "P256dh key")?,
        key_auth: valid_key(params.keys().auth.as_deref(), "Auth key")?,
        alerts: merge_alerts(&Value::Null, &params.given_alerts()),
        policy: valid_policy(params.data_policy())?.unwrap_or_else(|| DEFAULT_POLICY.to_owned()),
    };
    refuse_blocked_endpoint(&state.http_client, &registered.endpoint).await?;
    let row = web_push_subscription::create(&state.db, &registered).await?;
    debug!(
        subscription = %row.id,
        token = %registered.token_id,
        "push subscription registered"
    );
    subscription_response(&state, &row).await
}

/// `GET /api/v1/push/subscription` — the token's subscription.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser`] and
/// the `push` check; [`NotFound`](crate::error::AppError::NotFound) when
/// the token never registered one. Any database failure arrives through
/// `?` as [`Database`](crate::error::AppError::Database).
async fn show_subscription(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<PushSubscription>> {
    auth.require(PUSH)?;
    let Some(row) = web_push_subscription::find(&state.db, auth.1.id).await? else {
        return Err(AppError::NotFound);
    };
    subscription_response(&state, &row).await
}

/// `PUT /api/v1/push/subscription` — move the preferences the body names
/// and answer the updated entity.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser`] and
/// the `push` check; [`NotFound`](crate::error::AppError::NotFound) when
/// the token has no subscription;
/// [`Unprocessable`](crate::error::AppError::Unprocessable) for a policy
/// outside [`POLICIES`]. Any database failure arrives through `?` as
/// [`Database`](crate::error::AppError::Database).
async fn update_subscription(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<SubscriptionParams>,
) -> AppResult<Json<PushSubscription>> {
    auth.require(PUSH)?;
    let Some(stored) = web_push_subscription::find(&state.db, auth.1.id).await? else {
        return Err(AppError::NotFound);
    };
    // Every submission merges over the stored row: a named flag moves,
    // an unnamed one keeps its value, and the stored object — always
    // complete in an API-written row — comes back unchanged in shape.
    let alerts = merge_alerts(&stored.alerts, &params.given_alerts());
    let policy = match valid_policy(params.data_policy())? {
        Some(policy) => policy,
        None => stored.policy.clone(),
    };
    let row = web_push_subscription::update_settings(&state.db, auth.1.id, &alerts, &policy)
        .await?
        .ok_or(AppError::NotFound)?;
    debug!(
        subscription = %row.id,
        token = %auth.1.id,
        "push subscription updated"
    );
    subscription_response(&state, &row).await
}

/// `DELETE /api/v1/push/subscription` — remove the token's subscription
/// and answer the empty object.
///
/// # Errors
///
/// [`Unauthorized`](crate::error::AppError::Unauthorized) /
/// [`Forbidden`](crate::error::AppError::Forbidden) from [`AuthUser`] and
/// the `push` check; [`NotFound`](crate::error::AppError::NotFound) when
/// the token has nothing to remove. Any database failure arrives through
/// `?` as [`Database`](crate::error::AppError::Database).
async fn delete_subscription(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Value>> {
    auth.require(PUSH)?;
    if !web_push_subscription::delete_for_token(&state.db, auth.1.id).await? {
        return Err(AppError::NotFound);
    }
    debug!(
        token = %auth.1.id,
        "push subscription deleted"
    );
    Ok(Json(Value::Object(serde_json::Map::new())))
}

/// Renders a written or read row as the response entity, with the
/// instance VAPID public key as `server_key`.
///
/// # Errors
///
/// Any database failure from seeding a first key pair arrives as
/// [`AppError::Database`](crate::error::AppError::Database).
async fn subscription_response(
    state: &AppState,
    row: &web_push_subscription::WebPushSubscription,
) -> AppResult<Json<PushSubscription>> {
    let server_key = vapid::vapid_public_key(&state.db).await?;
    Ok(Json(PushSubscription::from_row(row, &server_key)))
}

/// The request parameters of `POST`/`PUT /api/v1/push/subscription`, in
/// the `subscription[…]` / `data[…]` nesting both body styles produce
/// (decision 10). Every piece is optional at the type level so the
/// endpoint can answer a missing one with Pleroma's `Validation failed:`
/// wording rather than the extractor's generic parameter message.
#[derive(Debug, Default, Deserialize)]
struct SubscriptionParams {
    /// The `PushSubscription` itself: endpoint plus transport keys.
    #[serde(default)]
    subscription: Option<SubscriptionBody>,
    /// The delivery preferences: alert flags and policy.
    #[serde(default)]
    data: Option<SubscriptionData>,
}

/// The `subscription` object: the callback endpoint and the keys object.
#[derive(Debug, Deserialize)]
struct SubscriptionBody {
    /// The browser callback URL; required at create.
    #[serde(default)]
    endpoint: Option<String>,
    /// The client transport keys; required at create.
    #[serde(default)]
    keys: Option<SubscriptionKeys>,
}

impl SubscriptionBody {
    /// The stand-in the accessors hand out when the body named no
    /// `subscription` object.
    const EMPTY: Self = Self {
        endpoint: None,
        keys: None,
    };
}

/// The `subscription[keys]` object.
#[derive(Debug, Deserialize)]
struct SubscriptionKeys {
    /// The client ECDH public key, base64.
    #[serde(rename = "p256dh", default)]
    p256dh: Option<String>,
    /// The client auth secret, base64.
    #[serde(default)]
    auth: Option<String>,
}

impl SubscriptionKeys {
    /// The stand-in for a body that named no `keys` object.
    const EMPTY: Self = Self {
        p256dh: None,
        auth: None,
    };
}

/// The `data` object of both requests.
#[derive(Debug, Deserialize)]
struct SubscriptionData {
    /// The per-notification-type flags.
    #[serde(default)]
    alerts: Option<AlertsParams>,
    /// Who may wake the subscription.
    #[serde(default)]
    policy: Option<String>,
}

impl SubscriptionData {
    /// The stand-in for a body that named no `data` object.
    const EMPTY: Self = Self {
        alerts: None,
        policy: None,
    };
}

/// The `data[alerts]` object: every [`ALERT_TYPES`] name as an optional
/// flex boolean (JSON `true`/`false` or the form spellings; anything else
/// is the extractor's 422). An absent field keeps its stored value.
///
/// [`ALERT_TYPES`]: crate::entities::push_subscription::ALERT_TYPES
#[derive(Debug, Clone, Copy, Default, Deserialize, serde::Serialize)]
struct AlertsParams {
    /// Someone followed the account.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    follow: Option<bool>,
    /// Someone favourited a status.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    favourite: Option<bool>,
    /// Someone reblogged a status.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    reblog: Option<bool>,
    /// Someone mentioned the account.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    mention: Option<bool>,
    /// A poll the account voted in or posted ended.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    poll: Option<bool>,
    /// Someone posted a status the account follows.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    status: Option<bool>,
    /// A followed status was edited.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    update: Option<bool>,
    /// Someone quoted a status of the account.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    quote: Option<bool>,
    /// A status quoting one of the account's was edited.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    quoted_update: Option<bool>,
    /// Someone requested to follow a locked account (`follow` also wakes
    /// for it, Pleroma's `enabled?/2` alias).
    #[serde(
        rename = "follow_request",
        default,
        deserialize_with = "flex::opt_bool"
    )]
    follow_request: Option<bool>,
    /// Someone mentioned the account in a chat (Pleroma extension).
    #[serde(
        rename = "pleroma:chat_mention",
        default,
        deserialize_with = "flex::opt_bool"
    )]
    pleroma_chat_mention: Option<bool>,
    /// Someone reacted with an emoji to a status (Pleroma extension).
    #[serde(
        rename = "pleroma:emoji_reaction",
        default,
        deserialize_with = "flex::opt_bool"
    )]
    pleroma_emoji_reaction: Option<bool>,
    /// A new account signed up (admin alert).
    #[serde(rename = "admin.sign_up", default, deserialize_with = "flex::opt_bool")]
    admin_sign_up: Option<bool>,
    /// A report was filed (admin alert).
    #[serde(rename = "admin.report", default, deserialize_with = "flex::opt_bool")]
    admin_report: Option<bool>,
}

impl SubscriptionParams {
    /// The `subscription` object as a value, [`SubscriptionBody::EMPTY`]
    /// standing in for a missing one.
    fn body(&self) -> &SubscriptionBody {
        self.subscription
            .as_ref()
            .unwrap_or(&SubscriptionBody::EMPTY)
    }

    /// The `subscription[keys]` object, [`SubscriptionKeys::EMPTY`] when
    /// the body named none.
    fn keys(&self) -> &SubscriptionKeys {
        self.body()
            .keys
            .as_ref()
            .unwrap_or(&SubscriptionKeys::EMPTY)
    }

    /// The `data` object, [`SubscriptionData::EMPTY`] when the body named
    /// none.
    fn data(&self) -> &SubscriptionData {
        self.data.as_ref().unwrap_or(&SubscriptionData::EMPTY)
    }

    /// The requested `data[policy]`, if any.
    fn data_policy(&self) -> Option<&str> {
        self.data().policy.as_deref()
    }

    /// The `data[alerts]` object as a JSON value of the named flags: the
    /// input side of [`merge_alerts`], which reads booleans by alert name
    /// and ignores the absent ones (serialized as nulls). Serializing a
    /// struct of `Option<bool>` fields cannot fail, so a hypothetical
    /// error degrades to "nothing named" rather than a 500.
    fn given_alerts(&self) -> Value {
        serde_json::to_value(self.data().alerts.unwrap_or_default())
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
    }
}

/// The validated `subscription[endpoint]`: required, at most
/// [`MAX_ENDPOINT_CHARS`] characters and an `http` or `https` URL. The
/// scheme is what the outbound client can call; whether the host is one
/// the SSRF guard allows is [`refuse_blocked_endpoint`]'s question.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with the Mastodon changeset wording for
/// whichever rule the endpoint breaks.
fn valid_endpoint(params: &SubscriptionParams) -> AppResult<String> {
    let Some(endpoint) = params
        .body()
        .endpoint
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    else {
        return Err(validation_failed("Endpoint can't be blank"));
    };
    if endpoint.chars().count() > MAX_ENDPOINT_CHARS {
        return Err(validation_failed(format!(
            "Endpoint is too long (maximum is {MAX_ENDPOINT_CHARS} characters)"
        )));
    }
    if !is_http_url(endpoint) {
        return Err(validation_failed("Endpoint must be a valid URL"));
    }
    Ok(endpoint.to_owned())
}

/// Refuses an `endpoint` whose host the outbound client's SSRF guard
/// would block at delivery time.
///
/// Literal IPs are judged directly and hostnames by their resolved
/// addresses, exactly as [`HttpClient::check_target`] does; with the guard
/// off (`HTTP_ALLOW_PRIVATE_HOSTS`) every endpoint passes. Without this a
/// subscription to a private address would queue a delivery job per
/// notification that can only be blocked.
///
/// # Errors
///
/// [`AppError::Unprocessable`] naming the endpoint when the guard blocks
/// it, or when it is not an `http`/`https` URL.
async fn refuse_blocked_endpoint(http: &HttpClient, endpoint: &str) -> AppResult<()> {
    match http.check_target(endpoint).await {
        Ok(()) => Ok(()),
        Err(HttpError::Blocked(_)) => Err(validation_failed(
            "Endpoint must not be a loopback, private, or otherwise reserved address",
        )),
        Err(_) => Err(validation_failed("Endpoint must be a valid URL")),
    }
}

/// The validated `subscription[keys]` member `attribute` (`P256dh key` or
/// `Auth key`), stored base64url-normalised: Pleroma's
/// `ensure_base64_urlsafe` folds `+` to `-`, `/` to `_` and drops the
/// `=` padding, so clients that send standard base64 still deliver.
///
/// # Errors
///
/// [`AppError::Unprocessable`] with `"{attribute} can't be blank"` when
/// the client named no key there.
fn valid_key(value: Option<&str>, attribute: &str) -> AppResult<String> {
    let Some(key) = value.filter(|text| !text.trim().is_empty()) else {
        return Err(validation_failed(format!("{attribute} can't be blank")));
    };
    Ok(key
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_owned())
}

/// The validated `data[policy]`: one of [`POLICIES`] in its own spelling,
/// or `None` for "the client named no policy" (create then defaults to
/// `all`, update keeps the stored one).
///
/// # Errors
///
/// [`AppError::Unprocessable`] with Mastodon's inclusion wording for any
/// other text.
fn valid_policy(value: Option<&str>) -> AppResult<Option<String>> {
    let Some(text) = value else {
        return Ok(None);
    };
    POLICIES
        .iter()
        .find(|policy| **policy == text)
        .map(|policy| Some((*policy).to_owned()))
        .ok_or_else(|| validation_failed("Policy is not included in the list"))
}

/// A 422 in Mastodon's changeset shape: `{"error":"Validation failed:
/// <attribute> <reason>"}` (decision 8 renders the message as-is).
fn validation_failed(reason: impl std::fmt::Display) -> AppError {
    AppError::Unprocessable(format!("Validation failed: {reason}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The request parameters `body`, as the extractor would deserialize them.
    fn params(body: Value) -> SubscriptionParams {
        serde_json::from_value(body).expect("the test body has the endpoint's shape")
    }

    /// The complete, valid create body used across the tests.
    fn full_body() -> Value {
        json!({
            "subscription": {
                "endpoint": "https://push.example/sub/1",
                "keys": {"p256dh": "BCIXkey", "auth": "auth-sec="},
            },
            "data": {
                "alerts": {"follow": true, "admin.report": "on"},
                "policy": "follower",
            },
        })
    }

    #[test]
    fn a_json_body_deserializes_every_piece_of_the_shape() {
        let parsed = params(full_body());
        assert_eq!(
            parsed.body().endpoint.as_deref(),
            Some("https://push.example/sub/1")
        );
        assert_eq!(parsed.keys().p256dh.as_deref(), Some("BCIXkey"));
        assert_eq!(parsed.keys().auth.as_deref(), Some("auth-sec="));
        assert_eq!(parsed.data_policy(), Some("follower"));
        assert_eq!(
            parsed.given_alerts(),
            json!({
                "follow": true, "favourite": null, "reblog": null, "mention": null,
                "poll": null, "status": null, "update": null, "quote": null,
                "quoted_update": null, "follow_request": null,
                "pleroma:chat_mention": null, "pleroma:emoji_reaction": null,
                "admin.sign_up": null, "admin.report": true,
            }),
            "named flags as booleans, the rest as nulls merge_alerts ignores"
        );
    }

    #[test]
    fn an_empty_body_leaves_every_piece_absent() {
        let parsed = params(json!({}));
        assert!(parsed.body().endpoint.is_none(), "no endpoint");
        assert!(parsed.keys().p256dh.is_none(), "no p256dh");
        assert!(parsed.keys().auth.is_none(), "no auth");
        assert_eq!(
            parsed.given_alerts(),
            json!(AlertsParams::default()),
            "all-null alerts"
        );
        assert_eq!(parsed.data_policy(), None, "no policy");
    }

    #[test]
    fn alert_flags_read_the_form_spellings() {
        let parsed = params(json!({
            "data": {"alerts": {
                "pleroma:chat_mention": "true",
                "pleroma:emoji_reaction": "0",
                "follow_request": "yes",
                "admin.sign_up": "off",
            }},
        }));
        let given = parsed.given_alerts();
        assert_eq!(given["pleroma:chat_mention"], json!(true), "string true");
        assert_eq!(
            given["pleroma:emoji_reaction"],
            json!(false),
            "string 0 is false"
        );
        assert_eq!(given["follow_request"], json!(true), "yes");
        assert_eq!(given["admin.sign_up"], json!(false), "off");
    }

    #[test]
    fn unknown_alert_names_are_not_deserialized() {
        let parsed = params(json!({"data": {"alerts": {"follow": true, "hype": true}}}));
        assert!(
            !parsed
                .given_alerts()
                .get("hype")
                .is_some_and(Value::is_boolean),
            "a type outside ALERT_TYPES never reaches the stored object"
        );
    }

    #[test]
    fn a_non_boolean_alert_is_a_deserialization_failure() {
        let body = json!({"data": {"alerts": {"follow": "maybe"}}});
        assert!(
            serde_json::from_value::<SubscriptionParams>(body.clone()).is_err(),
            "`{body}` names no boolean",
        );
    }

    #[test]
    fn endpoint_validation_demands_a_usable_url() {
        assert_eq!(
            valid_endpoint(&params(full_body())).unwrap(),
            "https://push.example/sub/1",
            "the good case passes through verbatim"
        );
        assert_eq!(
            valid_endpoint(&params(
                json!({"subscription": {"endpoint": "http://127.0.0.1:9/push"}})
            ))
            .unwrap(),
            "http://127.0.0.1:9/push",
            "the shape check passes a plain-http loopback; the guard check refuses it"
        );
        for (body, message) in [
            (json!({}), "Validation failed: Endpoint can't be blank"),
            (
                json!({"subscription": {"endpoint": "   "}}),
                "Validation failed: Endpoint can't be blank",
            ),
            (
                json!({"subscription": {"endpoint": "push.example/1"}}),
                "Validation failed: Endpoint must be a valid URL",
            ),
            (
                json!({"subscription": {"endpoint": "ftp://push.example/1"}}),
                "Validation failed: Endpoint must be a valid URL",
            ),
            (
                json!({"subscription": {"endpoint": format!("https://p.example/{}", "x".repeat(MAX_ENDPOINT_CHARS))}}),
                "Validation failed: Endpoint is too long (maximum is 2048 characters)",
            ),
        ] {
            let err = valid_endpoint(&params(body)).expect_err("the endpoint is refused");
            assert_eq!(err.to_string(), message, "wording of the refusal");
            let AppError::Unprocessable(_) = err else {
                panic!("a validation refusal is a 422, got {err}");
            };
        }
    }

    #[tokio::test]
    async fn the_guard_refuses_private_endpoints_unless_it_is_off() {
        let guarded = HttpClient::new("Wired (https://wired.test)");
        for endpoint in [
            "http://127.0.0.1:8080/push",
            "http://10.1.2.3/push",
            "https://192.168.0.9/push",
            "http://[::1]/push",
            "http://169.254.169.254/latest/meta-data",
        ] {
            let err = refuse_blocked_endpoint(&guarded, endpoint)
                .await
                .expect_err("a reserved address is refused");
            assert_eq!(
                err.to_string(),
                "Validation failed: Endpoint must not be a loopback, private, or otherwise reserved address",
                "wording for {endpoint}"
            );
            assert!(
                matches!(err, AppError::Unprocessable(_)),
                "a refusal is a 422, got {err}"
            );
        }
        assert!(
            refuse_blocked_endpoint(&guarded, "https://8.8.8.8/push")
                .await
                .is_ok(),
            "a public address passes"
        );
        let open = HttpClient::new_for_tests("Wired (https://wired.test)");
        assert!(
            refuse_blocked_endpoint(&open, "http://127.0.0.1:8080/push")
                .await
                .is_ok(),
            "with the guard off a loopback endpoint is accepted"
        );
    }

    #[tokio::test]
    async fn a_non_http_endpoint_is_an_invalid_url_to_the_guard_check() {
        let guarded = HttpClient::new("Wired (https://wired.test)");
        let err = refuse_blocked_endpoint(&guarded, "ftp://push.example/1")
            .await
            .expect_err("a foreign scheme is refused");
        assert_eq!(
            err.to_string(),
            "Validation failed: Endpoint must be a valid URL",
            "the scheme wording"
        );
    }

    #[test]
    fn an_endpoint_at_exactly_the_limit_is_accepted() {
        let filler = "x".repeat(MAX_ENDPOINT_CHARS - "https://p.example/".len());
        let endpoint = format!("https://p.example/{filler}");
        assert_eq!(
            endpoint.chars().count(),
            MAX_ENDPOINT_CHARS,
            "the fixture is exactly at the cap"
        );
        let body = json!({"subscription": {"endpoint": endpoint}});
        assert_eq!(
            valid_endpoint(&params(body)).unwrap(),
            endpoint,
            "the cap is inclusive"
        );
    }

    #[test]
    fn keys_are_required_and_stored_urlsafe() {
        let missing_err = valid_key(None, "P256dh key").expect_err("a missing key is refused");
        assert_eq!(
            missing_err.to_string(),
            "Validation failed: P256dh key can't be blank",
            "the message names the attribute"
        );
        let blank_err = valid_key(Some(" "), "Auth key").expect_err("a blank key is refused");
        assert_eq!(
            blank_err.to_string(),
            "Validation failed: Auth key can't be blank"
        );
        assert_eq!(
            valid_key(Some("a+b/c=="), "Auth key").unwrap(),
            "a-b_c",
            "Pleroma's fold: `+`→`-`, `/`→`_`, padding dropped"
        );
    }

    #[test]
    fn policy_validation_walks_the_allowed_list() {
        assert_eq!(valid_policy(None).unwrap(), None, "absent is no statement");
        for policy in POLICIES {
            assert_eq!(
                valid_policy(Some(policy)).unwrap().as_deref(),
                Some(policy),
                "`{policy}` is allowed verbatim"
            );
        }
        for text in ["All", "", "everyone", "followee"] {
            let err = valid_policy(Some(text)).expect_err("a policy outside POLICIES is refused");
            assert_eq!(
                err.to_string(),
                "Validation failed: Policy is not included in the list",
                "Mastodon's inclusion wording for {text:?}"
            );
        }
    }

    #[test]
    fn create_merges_given_alerts_over_the_all_off_default() {
        let parsed = params(full_body());
        let stored = merge_alerts(&Value::Null, &parsed.given_alerts());
        assert_eq!(stored["follow"], json!(true), "named on");
        assert_eq!(stored["admin.report"], json!(true), "named on, form string");
        assert_eq!(stored["mention"], json!(false), "unnamed defaults to off");
    }

    #[test]
    fn an_all_null_alerts_object_merges_as_nothing_named() {
        let parsed = params(json!({"data": {"alerts": {}}}));
        let merged = merge_alerts(&json!({"follow": true}), &parsed.given_alerts());
        assert_eq!(
            merged["follow"],
            json!(true),
            "an empty alerts submission changes no flag"
        );
    }
}
