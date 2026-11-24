//! The `GET /api/v2/instance` document: the v1 facts regrouped the way
//! Mastodon 4 clients read them, with the v1 limit groups reused whole.

use serde::Serialize;
use serde_json::Value;

use crate::{
    config::{Config, Toggle},
    entities::account::Account,
    models::{instance_rule::InstanceRule, user::User},
    services::instance_meta::SOURCE_URL,
};

use super::{
    MAX_FEATURED_TAGS, MAX_PINNED_STATUSES, MONTHLY_ACTIVE_USERS, THUMBNAIL_PATH,
    handlers::{streaming_api_url, version_string},
    pleroma::PleromaBlock,
    v1::{MediaAttachmentsConfiguration, PollsConfiguration, Rule, StatusesConfiguration},
};

/// The `GET /api/v2/instance` document, in the shape Pleroma's
/// `InstanceView.render("show2.json", …)` writes: the same facts as
/// [`Instance`](super::v1::Instance), regrouped the way Mastodon 4 clients
/// read them — `domain`/`source_url`/`usage`/`thumbnail`, nested
/// `registrations` and `contact`, a widened `configuration` — plus
/// Pleroma's own `pleroma` block, unchanged from v1.
#[derive(Debug, Serialize)]
pub(super) struct InstanceV2 {
    /// The domain of [`Config::base_url`], without scheme or port default.
    domain: String,
    /// Instance display name (`INSTANCE_NAME`).
    title: String,
    /// The compatibility version line ([`version_string`]).
    version: String,
    /// This server's source repository ([`SOURCE_URL`]).
    source_url: &'static str,
    /// Instance description (`INSTANCE_DESCRIPTION`); like the v1
    /// document, the one setting fills both of Pleroma's description
    /// keys, and v2 keeps only the long one.
    description: String,
    /// The user counters block.
    usage: Usage,
    /// The instance thumbnail block.
    thumbnail: Thumbnail,
    /// Content language codes (`INSTANCE_LANGUAGES`).
    languages: Vec<String>,
    /// The widened client-configuration block.
    configuration: ConfigurationV2,
    /// The registration state, nested as Mastodon 4 spells it.
    registrations: RegistrationsV2,
    /// The admin contact pair: email and account.
    contact: Contact,
    /// The instance rules, lowest priority first ([`Rule::from_model`]).
    rules: Vec<Rule>,
    /// Pleroma's extension block, the same [`PleromaBlock`] v1 writes.
    pleroma: PleromaBlock,
}

impl InstanceV2 {
    /// The document for the instance `config` serves, with the stored
    /// `rules`, `contact` as the account to advertise, the instance
    /// `vapid_key` in the push places, and the built `pleroma` extension
    /// block (its `federation` metadata reads live state). The `usage`
    /// counter needs no database read while no activity counter exists.
    ///
    /// Pure (decision 3): every value comes from one of the five inputs
    /// or a documented constant.
    pub(super) fn build(
        config: &Config,
        rules: &[InstanceRule],
        contact: Option<&User>,
        vapid_key: &str,
        pleroma: PleromaBlock,
    ) -> Self {
        Self {
            domain: config.domain().to_owned(),
            title: config.instance_name.clone(),
            version: version_string(),
            source_url: SOURCE_URL,
            description: config.instance_description.clone(),
            usage: Usage::monthly(),
            thumbnail: Thumbnail::for_config(config),
            languages: config.instance_languages.clone(),
            configuration: ConfigurationV2::for_config(config, vapid_key),
            registrations: RegistrationsV2::for_config(config),
            contact: Contact::build(config, contact),
            rules: rules.iter().map(Rule::from_model).collect(),
            pleroma,
        }
    }
}

/// The `usage` block.
#[derive(Debug, Serialize)]
struct Usage {
    /// The user counters.
    users: UsageUsers,
}

impl Usage {
    /// The usage block of an instance that counts no activity: the same
    /// zero the `pleroma` block reports as `stats.mau`.
    const fn monthly() -> Self {
        Self {
            users: UsageUsers {
                active_month: MONTHLY_ACTIVE_USERS,
            },
        }
    }
}

/// The `usage.users` block.
#[derive(Debug, Serialize)]
struct UsageUsers {
    /// Users active in the last month; wired-be keeps no activity
    /// counter yet, so a constant zero (the ticket's answer while the
    /// counters ticket is open).
    active_month: usize,
}

/// The `thumbnail` block: Mastodon 4 wraps the URL v1 writes bare.
#[derive(Debug, Serialize)]
struct Thumbnail {
    /// The instance thumbnail URL ([`THUMBNAIL_PATH`] under
    /// [`Config::frontend_base_url`]).
    url: String,
}

impl Thumbnail {
    /// The thumbnail block for `config`'s frontend origin.
    fn for_config(config: &Config) -> Self {
        Self {
            url: format!("{}{THUMBNAIL_PATH}", config.frontend_base_url),
        }
    }
}

/// The v2 `configuration` block: the three limit groups v1 already
/// writes, beside Mastodon 4's own `urls`, `vapid`, `translation` and
/// the widened `accounts` group.
#[derive(Debug, Serialize)]
struct ConfigurationV2 {
    /// The endpoint group.
    urls: EndpointUrls,
    /// The Web Push keys group.
    vapid: Vapid,
    /// The account limits group, wider than v1's.
    accounts: AccountsConfigurationV2,
    /// The status limits group, as in v1.
    statuses: StatusesConfiguration,
    /// The upload limits group, as in v1.
    media_attachments: MediaAttachmentsConfiguration,
    /// The poll limits group, as in v1.
    polls: PollsConfiguration,
    /// The translation support group.
    translation: TranslationConfiguration,
}

impl ConfigurationV2 {
    /// The block for the limits `config` configures and the instance
    /// `vapid_key`; the shared v1 groups are reused whole, the Mastodon
    /// 4-only groups carry the shipped constants.
    fn for_config(config: &Config, vapid_key: &str) -> Self {
        Self {
            urls: EndpointUrls::for_config(config),
            vapid: Vapid::with_key(vapid_key),
            accounts: AccountsConfigurationV2::shipped(),
            statuses: StatusesConfiguration::for_config(config),
            media_attachments: MediaAttachmentsConfiguration::for_config(config),
            polls: PollsConfiguration::for_config(config),
            translation: TranslationConfiguration::for_config(config),
        }
    }
}

/// The v2 `configuration.urls` group.
#[derive(Debug, Serialize)]
struct EndpointUrls {
    /// The websocket address of the streaming API ([`streaming_api_url`],
    /// under Mastodon 4's own key name).
    streaming: String,
    /// The status page URL; wired-be configures none, so JSON `null`
    /// (Pleroma's `[:instance, :status_page]` default).
    status: Value,
}

impl EndpointUrls {
    /// The endpoint group for `config`'s origin, with no status page.
    fn for_config(config: &Config) -> Self {
        Self {
            streaming: streaming_api_url(config),
            status: Value::Null,
        }
    }
}

/// The v2 `configuration.vapid` group.
#[derive(Debug, Serialize)]
struct Vapid {
    /// The instance Web Push public key, the one the push endpoints
    /// hand out as `server_key` (see [`crate::services::vapid`]).
    public_key: String,
}

impl Vapid {
    /// The group advertising `public_key`.
    fn with_key(public_key: &str) -> Self {
        Self {
            public_key: public_key.to_owned(),
        }
    }
}

/// The v2 `configuration.accounts` group: the featured-tag limit v1
/// reports, plus the pinned-status one.
#[derive(Debug, Serialize)]
struct AccountsConfigurationV2 {
    /// Tags one account may feature.
    max_featured_tags: usize,
    /// Statuses one account may pin.
    max_pinned_statuses: usize,
}

impl AccountsConfigurationV2 {
    /// The shipped account limits.
    const fn shipped() -> Self {
        Self {
            max_featured_tags: MAX_FEATURED_TAGS,
            max_pinned_statuses: MAX_PINNED_STATUSES,
        }
    }
}

/// The v2 `configuration.translation` group.
#[derive(Debug, Serialize)]
struct TranslationConfiguration {
    /// Whether a translation provider is configured, from
    /// `TRANSLATION_PROVIDER` and its URL and key (Pleroma's
    /// `Pleroma.Language.Translation.configured?()`).
    enabled: bool,
}

impl TranslationConfiguration {
    /// The group of the provider `config` configures: enabled exactly when
    /// an instance built from those settings can talk to a provider (the same
    /// rule [`Translator::from_config`] applies to pick a translator).
    ///
    /// [`Translator::from_config`]: crate::services::translate::Translator::from_config
    const fn for_config(config: &Config) -> Self {
        Self {
            enabled: config.translation.is_configured(),
        }
    }
}

/// The v2 `registrations` block: the flat v1 pair, nested, plus the
/// invitation fields Mastodon 4 adds (this instance has neither).
#[derive(Debug, Serialize)]
struct RegistrationsV2 {
    /// Whether anyone may register (`REGISTRATIONS_OPEN`).
    enabled: Toggle,
    /// Whether registration needs staff approval
    /// (`REGISTRATIONS_APPROVAL_REQUIRED`).
    approval_required: bool,
    /// The text shown on the registration form; `None` renders JSON
    /// `null`, the answer when the instance writes none.
    message: Value,
    /// The external registration URL; wired-be registers in place, so
    /// JSON `null`.
    url: Value,
}

impl RegistrationsV2 {
    /// The block for `config`'s registration state, with the two
    /// never-set extras.
    const fn for_config(config: &Config) -> Self {
        Self {
            enabled: config.registrations_open,
            approval_required: config.registrations_approval_required.get(),
            message: Value::Null,
            url: Value::Null,
        }
    }
}

/// The v2 `contact` block: the two flat v1 keys (`email`,
/// `contact_account`), nested.
#[derive(Debug, Serialize)]
struct Contact {
    /// Admin contact email (`CONTACT_EMAIL`), empty when unconfigured
    /// (the same answer `Instance::email` gives).
    email: String,
    /// The account clients should contact the staff through: the first
    /// local admin, or JSON `null` when there is none.
    account: Option<Account>,
}

impl Contact {
    /// The block for `config`'s email and the `account` to advertise.
    fn build(config: &Config, account: Option<&User>) -> Self {
        Self {
            email: config.contact_email.clone().unwrap_or_default(),
            account: account.map(|user| Account::from_user(user, config)),
        }
    }
}

#[cfg(test)]
mod tests;
