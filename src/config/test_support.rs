//! Shared fixtures for the unit tests of the `config` children: the lookup
//! builders every test reads a [`Config`] through.
//!
//! [`Config`]: crate::config::Config

use crate::config::{BASE_URL_VAR, Config, ConfigError, DATABASE_URL_VAR};

/// `DATABASE_URL` the fixture configs read.
pub(crate) const URL: &str = "postgres://user:secret@localhost/wired";
/// `BASE_URL` the fixture configs read.
pub(crate) const ORIGIN: &str = "https://social.example";

/// Builds a lookup over a fixed list of variables.
pub(crate) fn lookup_from(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let vars: Vec<(String, String)> = vars
        .iter()
        .map(|&(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

/// Reads a [`Config`] with the two required variables set, plus `extra`
/// (which may override them).
///
/// [`Config`]: crate::config::Config
pub(crate) fn try_config_with(extra: &[(&str, &str)]) -> Result<Config, ConfigError> {
    let mut vars: Vec<(String, String)> = vec![
        (DATABASE_URL_VAR.to_owned(), URL.to_owned()),
        (BASE_URL_VAR.to_owned(), ORIGIN.to_owned()),
    ];
    for &(key, value) in extra {
        match vars.iter_mut().find(|(k, _)| k == key) {
            Some(existing) => existing.1 = value.to_owned(),
            None => vars.push((key.to_owned(), value.to_owned())),
        }
    }
    Config::from_lookup(lookup_from(
        &vars
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect::<Vec<_>>(),
    ))
}

/// Like [`try_config_with`], panicking on error.
pub(crate) fn config_with(extra: &[(&str, &str)]) -> Config {
    try_config_with(extra).unwrap()
}

/// The result of reading a config whose `BASE_URL` is `value`.
pub(crate) fn base_url_result(value: &str) -> Result<Config, ConfigError> {
    Config::from_lookup(lookup_from(&[
        (DATABASE_URL_VAR, URL),
        (BASE_URL_VAR, value),
    ]))
}
