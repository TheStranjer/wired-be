//! Fixtures the admin ops children's unit tests share.

use super::{
    INSTALL_FAILURE,
    frontends::{Install, InstallParams, resolve_install},
};
use crate::{
    config::{Config, FrontendConfig},
    error::AppError,
};

/// Whether `err` is the install refusal naming `detail` — the ticket's
/// pinned prefix, with the tail left to the reason.
pub(super) fn is_install_failure(err: &AppError, detail_start: &str) -> bool {
    matches!(
        err,
        AppError::BadRequest(message)
            if message
                .strip_prefix(INSTALL_FAILURE)
                .is_some_and(|rest| rest.starts_with(detail_start))
    )
}

/// A [`Config`] over the test constants with the frontends settings
/// applied directly (the fields, not the environment).
pub(super) fn config_with_frontends(frontends: &[(&str, Option<&str>)]) -> Config {
    let mut config = Config::for_tests();
    config.statics.frontends.available = frontends
        .iter()
        .map(|(name, reference)| FrontendConfig {
            name: (*name).to_owned(),
            git: None,
            build_url: None,
            reference: reference.map(|r| (*r).to_owned()),
            build_dir: None,
        })
        .collect();
    config
}

/// The install the params resolve to, panicking on a refusal the
/// test did not expect.
pub(super) fn resolved(config: &Config, params: &InstallParams, has_file: bool) -> Install {
    resolve_install(config, params, has_file).expect("the fixture installs")
}
