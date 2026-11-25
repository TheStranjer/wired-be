//! The `GET /api/v1/instance` document: Mastodon's instance shape in the key
//! order Pleroma renders it, with the limit groups the v2 document reuses.

use serde::Serialize;

use crate::{
    config::{Config, Toggle},
    default_assets,
    entities::account::Account,
    models::{instance_rule::InstanceRule, user::User},
    services::instance_meta::{
        AVATAR_UPLOAD_LIMIT, BACKGROUND_UPLOAD_LIMIT, BANNER_UPLOAD_LIMIT,
        MAX_POLL_EXPIRATION_SECS, MAX_POLL_OPTION_CHARS, MIN_POLL_EXPIRATION_SECS, PollLimits,
    },
};

use super::{
    CHARACTERS_RESERVED_PER_URL, DESCRIPTION_LIMIT, IMAGE_MATRIX_LIMIT, InstanceStats,
    MAX_FEATURED_TAGS, SUPPORTED_MIME_TYPES, THUMBNAIL_PATH, VIDEO_FRAME_RATE_LIMIT,
    VIDEO_MATRIX_LIMIT,
    handlers::{streaming_api_url, version_string},
    pleroma::PleromaBlock,
    wired::WiredBlock,
};

/// The whole instance document, in the key order Pleroma renders it.
#[derive(Debug, Serialize)]
pub(super) struct Instance {
    /// The domain of [`Config::base_url`], without scheme or port default.
    uri: String,
    /// Instance display name (`INSTANCE_NAME`).
    title: String,
    /// Instance description (`INSTANCE_DESCRIPTION`); wired-be has no
    /// separate short one, so both keys carry it (Pleroma's two config
    /// keys collapse into one setting).
    short_description: String,
    /// The same description again, where Mastodon puts the long form.
    description: String,
    /// Admin contact email (`CONTACT_EMAIL`), empty when unconfigured
    /// (Pleroma answers `null`; the ticket spells the empty string).
    email: String,
    /// The compatibility version line ([`version_string`]).
    version: String,
    /// The streaming endpoint group.
    urls: Urls,
    /// The live counters ([`InstanceStats`]).
    stats: InstanceStats,
    /// Instance thumbnail URL.
    thumbnail: String,
    /// Content language codes (`INSTANCE_LANGUAGES`).
    languages: Vec<String>,
    /// Whether anyone may register (`REGISTRATIONS_OPEN`).
    registrations: Toggle,
    /// Whether registration needs staff approval
    /// (`REGISTRATIONS_APPROVAL_REQUIRED`).
    approval_required: bool,
    /// Whether registration invites exist (`INVITES_ENABLED`).
    invites_enabled: bool,
    /// The account clients should contact the staff through: the first
    /// local admin, or JSON `null` when there is none (Pleroma resolves
    /// its `contact_username` config key the same way, missing user
    /// included).
    contact_account: Option<Account>,
    /// The instance rules, lowest priority first ([`Instance::build`]).
    rules: Vec<Rule>,
    /// Status length (`MAX_STATUS_CHARS`), Mastodon-era spelling.
    max_toot_chars: usize,
    /// Attachment limit per status (`MAX_MEDIA_ATTACHMENTS`).
    max_media_attachments: usize,
    /// The poll limits this instance enforces (Pleroma's extra block,
    /// beside [`Configuration`]'s Mastodon-shaped one), shared with `NodeInfo`.
    poll_limits: PollLimits,
    /// Bytes one uploaded file may hold (`UPLOAD_LIMIT_BYTES`).
    upload_limit: usize,
    /// Bytes an uploaded avatar may hold.
    avatar_upload_limit: usize,
    /// Bytes a profile background may hold.
    background_upload_limit: usize,
    /// Bytes an uploaded banner may hold.
    banner_upload_limit: usize,
    /// Characters the instance description may hold.
    description_limit: usize,
    /// The Mastodon client-configuration block.
    configuration: Configuration,
    /// Pleroma's extension block.
    pleroma: PleromaBlock,
    /// Wired's extension block ([`WiredBlock`]).
    wired: WiredBlock,
}

impl Instance {
    /// The document for the instance `config` serves, with the cached
    /// `stats` counters, the stored `rules`, `contact` as the account to
    /// advertise, and the built `pleroma` extension block (the endpoint
    /// assembles it because its `federation` metadata reads live state).
    ///
    /// Pure (decision 3): every value comes from one of the five inputs
    /// or a documented constant.
    pub(super) fn build(
        config: &Config,
        stats: &InstanceStats,
        rules: &[InstanceRule],
        contact: Option<&User>,
        pleroma: PleromaBlock,
    ) -> Self {
        Self {
            uri: config.domain().to_owned(),
            title: config.instance_name.clone(),
            short_description: config.instance_description.clone(),
            description: config.instance_description.clone(),
            email: config.contact_email.clone().unwrap_or_default(),
            version: version_string(),
            urls: Urls {
                streaming_api: streaming_api_url(config),
            },
            stats: *stats,
            thumbnail: default_assets::url(config, THUMBNAIL_PATH),
            languages: config.instance_languages.clone(),
            registrations: config.registrations_open,
            approval_required: config.registrations_approval_required.get(),
            invites_enabled: config.invites_enabled.get(),
            contact_account: contact.map(|user| Account::from_user(user, config)),
            rules: rules.iter().map(Rule::from_model).collect(),
            max_toot_chars: config.max_status_chars,
            max_media_attachments: config.max_media_attachments,
            poll_limits: PollLimits::for_config(config),
            upload_limit: config.upload_limits.max_file_bytes,
            avatar_upload_limit: AVATAR_UPLOAD_LIMIT,
            background_upload_limit: BACKGROUND_UPLOAD_LIMIT,
            banner_upload_limit: BANNER_UPLOAD_LIMIT,
            description_limit: DESCRIPTION_LIMIT,
            configuration: Configuration::for_config(config),
            pleroma,
            wired: WiredBlock::for_config(config),
        }
    }
}

/// The `urls` block.
#[derive(Debug, Serialize)]
struct Urls {
    /// The websocket address of the streaming API ([`streaming_api_url`]).
    streaming_api: String,
}

/// One entry of the [`rules`](Instance::rules) list, as Pleroma's
/// `rule.json` renderer writes it.
///
/// The id is the stored rule's key as the hyphenated string clients know
/// (decision 4); the text and hint come from the row unchanged.
#[derive(Debug, Serialize)]
pub(super) struct Rule {
    /// The rule's primary key.
    id: String,
    /// The rule text.
    text: String,
    /// The explanatory hint, empty when the instance gave none.
    hint: String,
}

impl Rule {
    /// Renders the stored `rule` for this document.
    pub(super) fn from_model(rule: &InstanceRule) -> Self {
        Self {
            id: rule.id.to_string(),
            text: rule.text.clone(),
            hint: rule.hint.clone(),
        }
    }
}

/// The `configuration` block: Mastodon's client limits, in Mastodon's
/// group and key order.
#[derive(Debug, Serialize)]
struct Configuration {
    /// The status limits group.
    statuses: StatusesConfiguration,
    /// The upload limits group.
    media_attachments: MediaAttachmentsConfiguration,
    /// The poll limits group.
    polls: PollsConfiguration,
    /// The account limits group.
    accounts: AccountsConfiguration,
}

impl Configuration {
    /// The block for the limits `config` configures, with the shipped
    /// constants for what wired-be does not make configurable.
    const fn for_config(config: &Config) -> Self {
        Self {
            statuses: StatusesConfiguration::for_config(config),
            media_attachments: MediaAttachmentsConfiguration::for_config(config),
            polls: PollsConfiguration::for_config(config),
            accounts: AccountsConfiguration {
                max_featured_tags: MAX_FEATURED_TAGS,
            },
        }
    }
}

/// The `configuration.statuses` group.
#[derive(Debug, Serialize)]
pub(super) struct StatusesConfiguration {
    /// Characters per status (`MAX_STATUS_CHARS`).
    max_characters: usize,
    /// Attachments per status (`MAX_MEDIA_ATTACHMENTS`).
    max_media_attachments: usize,
    /// Characters Mastodon subtracts per link before counting.
    characters_reserved_per_url: usize,
}

impl StatusesConfiguration {
    /// The status limits `config` configures, beside the fixed
    /// link reserve.
    pub(super) const fn for_config(config: &Config) -> Self {
        Self {
            max_characters: config.max_status_chars,
            max_media_attachments: config.max_media_attachments,
            characters_reserved_per_url: CHARACTERS_RESERVED_PER_URL,
        }
    }
}

/// The `configuration.media_attachments` group.
#[derive(Debug, Serialize)]
pub(super) struct MediaAttachmentsConfiguration {
    /// The attachment types the upload endpoint documents.
    supported_mime_types: &'static [&'static str],
    /// Bytes one image may hold (`UPLOAD_LIMIT_BYTES`).
    image_size_limit: usize,
    /// Bytes one video may hold (`UPLOAD_LIMIT_BYTES`).
    video_size_limit: usize,
    /// Total pixels one image may hold.
    image_matrix_limit: usize,
    /// Total pixels per frame one video may hold.
    video_matrix_limit: usize,
    /// Frames per second one video may run at.
    video_frame_rate_limit: usize,
}

impl MediaAttachmentsConfiguration {
    /// The upload limits `config` configures, beside the fixed
    /// matrix/frame-rate caps.
    pub(super) const fn for_config(config: &Config) -> Self {
        let upload_limit = config.upload_limits.max_file_bytes;
        Self {
            supported_mime_types: &SUPPORTED_MIME_TYPES,
            image_size_limit: upload_limit,
            video_size_limit: upload_limit,
            image_matrix_limit: IMAGE_MATRIX_LIMIT,
            video_matrix_limit: VIDEO_MATRIX_LIMIT,
            video_frame_rate_limit: VIDEO_FRAME_RATE_LIMIT,
        }
    }
}

/// The `configuration.polls` group — the same four limits as
/// [`PollLimits`](crate::services::instance_meta::PollLimits), under Mastodon's own key names.
#[derive(Debug, Serialize)]
pub(super) struct PollsConfiguration {
    /// Options one poll may offer (`MAX_POLL_OPTIONS`).
    max_options: usize,
    /// Characters one option may hold.
    max_characters_per_option: usize,
    /// Shortest poll duration, in seconds.
    min_expiration: usize,
    /// Longest poll duration, in seconds.
    max_expiration: usize,
}

impl PollsConfiguration {
    /// The poll limits `config` configures, beside Pleroma's shipped
    /// option length and duration window.
    pub(super) const fn for_config(config: &Config) -> Self {
        Self {
            max_options: config.max_poll_options,
            max_characters_per_option: MAX_POLL_OPTION_CHARS,
            min_expiration: MIN_POLL_EXPIRATION_SECS,
            max_expiration: MAX_POLL_EXPIRATION_SECS,
        }
    }
}

/// The `configuration.accounts` group.
#[derive(Debug, Serialize)]
struct AccountsConfiguration {
    /// Tags one account may feature.
    max_featured_tags: usize,
}

#[cfg(test)]
mod tests;
