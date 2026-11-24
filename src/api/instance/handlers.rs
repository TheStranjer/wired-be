//! The instance endpoints' handlers and the reads behind them: the v1 and v2
//! documents, the peer list, the rules, the domain blocks, the translation
//! languages, the activity listing and the extended description.

use axum::{Json, extract::State};
use chrono::Utc;
use serde_json::{Map, Value};
use sqlx::PgPool;

use crate::{
    config::Config,
    error::AppResult,
    federation::mrf::{self, Transparency},
    models::{instance_rule, user::User},
    services::{
        instance_meta::{self, Federation, PLEROMA_COMPATIBILITY_VERSION},
        runtime_config::RuntimeConfig,
        translate::{Translator, languages_matrix},
        vapid,
    },
    state::AppState,
};

use super::{
    FIRST_ADMIN_SQL, MASTODON_API_LEVEL, PEER_LIMIT, PEERS_SQL,
    domain_blocks::{DomainBlock, ExtendedDescription},
    pleroma::PleromaBlock,
    stats::stats,
    v1::{Instance, Rule},
    v2::InstanceV2,
};

/// `GET /api/v1/instance` — the public instance document. No
/// authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn show_instance(State(state): State<AppState>) -> AppResult<Json<Instance>> {
    let config = effective_instance_config(&state).await?;
    let counters = stats(&state).await?;
    let rules = instance_rule::list(&state.db).await?;
    let contact = read_first_admin(&state.db).await?;
    let vapid_key = vapid::vapid_public_key(&state.db).await?;
    let pleroma = PleromaBlock::build(&config, &vapid_key, federation_block(&state).await);
    Ok(Json(Instance::build(
        &config,
        &counters,
        &rules,
        contact.as_ref(),
        pleroma,
    )))
}

/// The `pleroma.metadata.federation` block: what the live [MRF
/// chain](crate::federation::mrf) publishes under the instance's
/// transparency settings — Pleroma-style policy names beside the
/// `mrf_simple`, `mrf_simple_info` and `mrf_<policy>` objects each policy
/// keys for itself — or, with `MRF_TRANSPARENCY` off, nothing but the
/// fixed keys. The configuration read is the pipeline's own cached one,
/// and [`crate::api::nodeinfo`] builds its block from the same answer.
async fn federation_block(state: &AppState) -> Federation {
    Federation::with_mrf(
        state.config.federation_enabled.get(),
        &mrf::describe(state).await,
    )
}

/// The `Instance::contact_account` user row: the first active local
/// admin, `None` when the instance has no admin to point at.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
async fn read_first_admin(pool: &PgPool) -> AppResult<Option<User>> {
    let admin = sqlx::query_as::<_, User>(FIRST_ADMIN_SQL)
        .fetch_optional(pool)
        .await?;
    Ok(admin)
}

/// `GET /api/v2/instance` — the public instance document in Mastodon 4's
/// reshaped form. No authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn show_instance_v2(State(state): State<AppState>) -> AppResult<Json<InstanceV2>> {
    let config = effective_instance_config(&state).await?;
    let rules = instance_rule::list(&state.db).await?;
    let contact = read_first_admin(&state.db).await?;
    let vapid_key = vapid::vapid_public_key(&state.db).await?;
    let pleroma = PleromaBlock::build(&config, &vapid_key, federation_block(&state).await);
    Ok(Json(InstanceV2::build(
        &config,
        &rules,
        contact.as_ref(),
        &vapid_key,
        pleroma,
    )))
}

/// The [`Config`] the instance documents answer with: the environment-
/// derived one with the admin config store's stored `:instance` overrides
/// applied (WBE-206) — the name, description, email, registration flags,
/// `limit` and `max_media_attachments` an operator set through
/// `POST /api/v1/pleroma/admin/config`, which [`RuntimeConfig::effective_config`]
/// resolves. Every other setting stays exactly as `Config` carries it.
///
/// # Errors
///
/// Any database failure arrives as [`AppError::Database`](crate::error::AppError).
///
/// [`RuntimeConfig::effective_config`]: crate::services::runtime_config::RuntimeConfig::effective_config
async fn effective_instance_config(state: &AppState) -> AppResult<Config> {
    RuntimeConfig::new(&state.db, &state.config)
        .effective_config()
        .await
}

/// [`show_peers`]: the peer list — distinct `users.domain`,
/// alphabetically, at most [`PEER_LIMIT`] hosts — with the hosts the
/// [`simple`](crate::federation::mrf::simple) policy's `reject` list names
/// taken out: an instance does not advertise a peer it refuses to talk to.
/// Unless the instance restricts unauthenticated reads, in which case
/// nothing is listed at all.
async fn list_peers(state: &AppState) -> AppResult<Vec<String>> {
    let peers = sqlx::query_scalar::<_, String>(PEERS_SQL)
        .bind(PEER_LIMIT)
        .fetch_all(&state.db)
        .await?;
    let config = mrf::load_config(state).await;
    let visible = peers
        .into_iter()
        .filter(|host| !config.on_reject_list(host))
        .collect();
    Ok(visible)
}

/// Whether the [`instance_meta`] access restrictions hide the federated
/// timeline from anonymous callers — the Akkoma-style gate Pleroma's
/// `peers` action grows behind it: an instance that will not serve remote
/// content to strangers publishes no peer list either. wired-be has no
/// private-instance mode to set, so this follows `config`'s
/// `timelines_federated` restriction (the advertised
/// `restrict_unauthenticated.timelines.federated`).
fn peers_restricted(config: &Config) -> bool {
    instance_meta::restrict_unauthenticated(config)
        .get("timelines")
        .and_then(|timelines| timelines.get("federated"))
        == Some(&Value::Bool(true))
}

/// `GET /api/v1/instance/peers` — the JSON array of known remote
/// domains. No authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn show_peers(State(state): State<AppState>) -> AppResult<Json<Vec<String>>> {
    let config = effective_instance_config(&state).await?;
    if peers_restricted(&config) {
        return Ok(Json(Vec::new()));
    }
    Ok(Json(list_peers(&state).await?))
}

/// `GET /api/v1/instance/rules` — the instance rules, the same entries
/// and order as the v1 document's `rules` block. No authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
pub(super) async fn show_rules(State(state): State<AppState>) -> AppResult<Json<Vec<Rule>>> {
    let rules = instance_rule::list(&state.db).await?;
    Ok(Json(rules.iter().map(Rule::from_model).collect()))
}

/// `GET /api/v1/instance/domain_blocks` — the domains this instance's own
/// [`simple`](crate::federation::mrf::simple) domain lists restrict, in
/// the shape Mastodon's domain-block listing documents: Pleroma's
/// `domain_blocks.json` renders those very lists, and the severity a host
/// takes is what its list does to it ([`DomainBlock::severity_of`]).
///
/// The answer is the transparency answer, because it is the transparency
/// lists: the empty array while `MRF_TRANSPARENCY` is off, no host
/// `MRF_TRANSPARENCY_EXCLUSIONS` keeps back, and every pattern verbatim —
/// a `*.example.com` entry covers that domain and everything below it, and
/// the listing says so in those words. No authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError) — the `mrf` instance
/// setting read is the pipeline's own cached one.
pub(super) async fn show_domain_blocks(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<DomainBlock>>> {
    let config = mrf::load_config(&state).await;
    Ok(Json(DomainBlock::published_all(
        config.simple(),
        &Transparency::from_config(&state.config),
    )))
}

/// `GET /api/v1/instance/translation_languages` — the language pairs the
/// configured provider offers, `{"<source>": ["<target>", …], …}`.
///
/// The list comes from [`Translator::languages`], fetched at most once per
/// [`LANGUAGES_TTL`] and kept in the [`TimedCache`] slot on
/// [`crate::state::AppState`] — a third-party round trip is no business of a
/// document clients read on every start-up. Only a list that actually arrived
/// is cached: an empty answer is either no provider or one that refused, and
/// neither should buy an hour of silence. With no provider configured (or one
/// that will not answer its list) the document is the empty object Pleroma's
/// `translation_languages.json` renders for an unconfigured instance. No
/// authentication.
///
/// [`TimedCache`]: crate::util::cache::TimedCache
/// [`LANGUAGES_TTL`]: crate::services::translate::LANGUAGES_TTL
pub(super) async fn show_translation_languages(
    State(state): State<AppState>,
) -> Json<Map<String, Value>> {
    let now = Utc::now();
    let pairs = if let Some(cached) = state.translation_languages_cache.get(now) {
        cached
    } else {
        let translator = Translator::from_config(&state.config, &state.http_client);
        let fresh = translator.languages().await;
        if !fresh.is_empty() {
            state.translation_languages_cache.insert(now, fresh.clone());
        }
        fresh
    };
    Json(languages_matrix(&pairs))
}

/// `GET /api/v1/instance/activity` — Mastodon's weekly activity stats.
/// wired-be keeps no activity counters (the same reason the `pleroma`
/// block's `stats.mau` is zero), so the listing is the empty array
/// Mastodon answers before its first counted week. No authentication.
pub(super) async fn show_activity() -> Json<Vec<Value>> {
    Json(Vec::new())
}

/// `GET /api/v1/instance/extended_description` — Mastodon's long-form
/// instance description. wired-be has one description setting, so the
/// document is that text wrapped in a paragraph, stamped with the
/// current time (there is no stored revision to date from). No
/// authentication.
pub(super) async fn show_extended_description(
    State(state): State<AppState>,
) -> Json<ExtendedDescription> {
    Json(ExtendedDescription::build(&state.config, Utc::now()))
}

/// The `version` string: Mastodon's API level, commented with the Pleroma
/// release this server is compatible with and Wired's own crate version —
/// the same comment style the [`NodeInfo`] software version uses.
///
/// [`NodeInfo`]: crate::api::nodeinfo
pub(super) fn version_string() -> String {
    format!(
        "{MASTODON_API_LEVEL} (compatible; Pleroma {PLEROMA_COMPATIBILITY_VERSION}; Wired {})",
        env!("CARGO_PKG_VERSION")
    )
}

/// The `urls.streaming_api` value: the websocket origin of this instance
/// — `wss` under `https`, plain `ws` otherwise (Pleroma's
/// `Endpoint.websocket_url()`).
pub(super) fn streaming_api_url(config: &Config) -> String {
    let scheme = if config.is_https() { "wss" } else { "ws" };
    format!("{scheme}://{}", config.domain())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_names_the_api_level_pleroma_and_wired() {
        assert_eq!(
            version_string(),
            format!(
                "2.7.2 (compatible; Pleroma 2.10.2; Wired {})",
                env!("CARGO_PKG_VERSION")
            ),
            "the API level, the compatibility target, and Wired's own version"
        );
    }

    #[test]
    fn the_streaming_url_follows_the_base_url_scheme() {
        let https = Config::for_tests();
        assert_eq!(
            streaming_api_url(&https),
            "wss://test.example",
            "https gains the secure websocket scheme"
        );
        let mut http = Config::for_tests();
        http.base_url = "http://localhost:4000".to_owned();
        assert_eq!(
            streaming_api_url(&http),
            "ws://localhost:4000",
            "http keeps plain ws, domain with port"
        );
    }
}
