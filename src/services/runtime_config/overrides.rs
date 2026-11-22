//! Applying the stored `:instance` subkeys onto the environment config.

use serde_json::{Map, Value};
use tracing::warn;

use crate::config::{Config, Toggle};
#[cfg(doc)]
use crate::services::config_registry;

/// Overlays the stored `:instance` subkeys onto `config`, one whitelisted
/// field at a time (the set [`config_registry`] enumerates).
pub(super) fn apply_instance_overrides(config: &mut Config, overrides: &Map<String, Value>) {
    if let Some(name) = override_text(overrides, "name") {
        name.clone_into(&mut config.instance_name);
    }
    if let Some(description) = override_text(overrides, "description") {
        description.clone_into(&mut config.instance_description);
    }
    if let Some(email) = override_text(overrides, "email") {
        config.contact_email = Some(email.to_owned());
    }
    if let Some(open) = override_flag(overrides, "registrations_open") {
        config.registrations_open = Toggle::new(open);
    }
    if let Some(approval) = override_flag(overrides, "account_approval_required") {
        config.registrations_approval_required = Toggle::new(approval);
    }
    if let Some(activation) = override_flag(overrides, "account_activation_required") {
        config.account_activation_required = Toggle::new(activation);
    }
    if let Some(invites) = override_flag(overrides, "invites_enabled") {
        config.invites_enabled = Toggle::new(invites);
    }
    if let Some(limit) = override_count(overrides, "limit") {
        config.max_status_chars = limit;
    }
    if let Some(attachments) = override_count(overrides, "max_media_attachments") {
        config.max_media_attachments = attachments;
    }
}

/// One stored text subkey, warned away when the row carries another type.
fn override_text<'m>(overrides: &'m Map<String, Value>, key: &str) -> Option<&'m str> {
    let value = overrides.get(key)?;
    let text = value.as_str();
    if text.is_none() {
        warn!(key, value = %value, "a :instance override is not text; the env value stands");
    }
    text
}

/// One stored boolean subkey, warned away when the row carries another
/// type.
fn override_flag(overrides: &Map<String, Value>, key: &str) -> Option<bool> {
    let value = overrides.get(key)?;
    let flag = value.as_bool();
    if flag.is_none() {
        warn!(key, value = %value, "a :instance override is not a boolean; the env value stands");
    }
    flag
}

/// One stored count subkey, warned away when the row carries another
/// type or a number past `usize`'s range.
fn override_count(overrides: &Map<String, Value>, key: &str) -> Option<usize> {
    let value = overrides.get(key)?;
    let count = value
        .as_u64()
        .and_then(|number| usize::try_from(number).ok());
    if count.is_none() {
        warn!(key, value = %value, "a :instance override is not a count; the env value stands");
    }
    count
}
