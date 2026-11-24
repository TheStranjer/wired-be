//! The admin-managed static content: the installable frontends and the
//! instance documents, with the readers that fill one [`StaticSettings`]
//! from the four `FRONTENDS_*` and `INSTANCE_DOCUMENT*` variables.

use std::path::PathBuf;

#[cfg(doc)]
use super::Config;

use super::{
    defaults::{
        DEFAULT_FRONTENDS_AVAILABLE, DEFAULT_FRONTENDS_DIR, DEFAULT_INSTANCE_DOCS_DIR,
        DEFAULT_INSTANCE_DOCUMENT_NAMES,
    },
    error::ConfigError,
    parse::comma_list,
    urls::directory,
    vars::{
        FRONTENDS_AVAILABLE_VAR, FRONTENDS_DIR_VAR, INSTANCE_DOCS_DIR_VAR,
        INSTANCE_DOCUMENT_NAMES_VAR,
    },
};

/// One installable frontend, as one element of the
/// `FRONTENDS_AVAILABLE` JSON array.
///
/// Pleroma's `[:frontends, :available]` keyword list, expressed as data
/// (see `Pleroma.Frontend.install`, `lib/pleroma/frontend.ex`). `name` is
/// the directory segment the frontend installs under and the answer's
/// identity; the four optional fields carry Pleroma's own meaning: `git`
/// is the source repository for the admin UI, `build_url` the pre-built-
/// archive address (`${ref}` stands for the ref), `reference` the build
/// ref (Pleroma's `ref` key), and `build_dir` the archive subdirectory to
/// install (default `dist`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct FrontendConfig {
    /// Frontend name: the answer's identity and the install directory's
    /// first segment, e.g. `plume`.
    pub name: String,
    /// The source repository, answered verbatim; `None` when unset.
    #[serde(default)]
    pub git: Option<String>,
    /// The build-archive download URL, `None` when unset — then only an
    /// uploaded file can install this frontend.
    #[serde(default)]
    pub build_url: Option<String>,
    /// The build ref, from the JSON `ref` key; `None` falls back to
    /// the `master` default at install and to the name's top directory
    /// for the installed flag.
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    /// The archive subdirectory to install; `None` means `dist`.
    #[serde(default)]
    pub build_dir: Option<String>,
}

/// The two settings that locate and describe installed frontends, from
/// `FRONTENDS_DIR` and `FRONTENDS_AVAILABLE`
/// (see [`crate::api::admin::ops`]).
///
/// It rides inside [`StaticSettings`], the one [`Config`] field for the
/// admin-managed static trees, because the [`Config`] readers and the
/// [`Debug`] impl each have an 80-line budget (AGENTS.md).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendSettings {
    /// Directory installed frontends live in, from `FRONTENDS_DIR`; the
    /// installed flag reads `dir/<name>/<ref>`.
    pub dir: PathBuf,
    /// The installable frontends, from `FRONTENDS_AVAILABLE`; empty when
    /// none is configured.
    pub available: Vec<FrontendConfig>,
}

/// Reads the two `FRONTENDS_*` settings into a [`FrontendSettings`].
///
/// # Errors
///
/// [`ConfigError::InvalidFrontends`] when `FRONTENDS_AVAILABLE` is neither
/// unset, blank, nor a JSON array of frontend objects.
pub(crate) fn frontend_settings<F>(lookup: &F) -> Result<FrontendSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let configured = lookup(FRONTENDS_AVAILABLE_VAR).unwrap_or_default();
    let text = if configured.trim().is_empty() {
        DEFAULT_FRONTENDS_AVAILABLE
    } else {
        configured.as_str()
    };
    let available =
        serde_json::from_str(text).map_err(|err| ConfigError::InvalidFrontends(err.to_string()))?;
    Ok(FrontendSettings {
        dir: directory(lookup, FRONTENDS_DIR_VAR, DEFAULT_FRONTENDS_DIR),
        available,
    })
}

/// The two settings that locate the instance documents and say which names
/// exist, from `INSTANCE_DOCS_DIR` and `INSTANCE_DOCUMENT_NAMES`
/// (see [`crate::api::admin::apps_docs`]).
///
/// It rides inside [`StaticSettings`] for the same line-budget reason
/// [`FrontendSettings`] does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceDocumentSettings {
    /// Directory the `<name>.html` documents are stored in, from
    /// `INSTANCE_DOCS_DIR`.
    pub dir: PathBuf,
    /// The document names the admin document routes accept, from
    /// `INSTANCE_DOCUMENT_NAMES`; the names outside this list are the
    /// routes' 404.
    pub names: Vec<String>,
}

/// Reads the two `INSTANCE_DOCUMENT*` settings into an
/// [`InstanceDocumentSettings`]: the directory with the shared
/// [`directory`] reader, the names with the shared [`comma_list`] split
/// over their documented default. A set-but-blank
/// `INSTANCE_DOCUMENT_NAMES` names no documents, exactly as an operator
/// writing it means.
pub(crate) fn instance_document_settings<F>(lookup: &F) -> InstanceDocumentSettings
where
    F: Fn(&str) -> Option<String>,
{
    let names = lookup(INSTANCE_DOCUMENT_NAMES_VAR)
        .unwrap_or_else(|| DEFAULT_INSTANCE_DOCUMENT_NAMES.to_owned());
    InstanceDocumentSettings {
        dir: directory(lookup, INSTANCE_DOCS_DIR_VAR, DEFAULT_INSTANCE_DOCS_DIR),
        names: comma_list(&names),
    }
}

/// The two admin-managed static file trees: the frontends the installer
/// lands and the documents the instance-document routes serve.
///
/// Pleroma keeps both under one `[:instance, :static_dir]`; this server
/// gives each its own directory variable, and they share one [`Config`]
/// field because the [`Config`] readers and the [`Debug`] impl each have
/// an 80-line budget (AGENTS.md) and both feed admin-served static
/// content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticSettings {
    /// The installed and installable frontends
    /// (see [`crate::api::admin::ops`]).
    pub frontends: FrontendSettings,
    /// The instance documents
    /// (see [`crate::api::admin::apps_docs`]).
    pub documents: InstanceDocumentSettings,
}

/// Reads the four `FRONTENDS_*` and `INSTANCE_DOCUMENT*` settings into the
/// one [`StaticSettings`].
///
/// # Errors
///
/// Those of [`frontend_settings`].
pub(crate) fn statics_settings<F>(lookup: &F) -> Result<StaticSettings, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(StaticSettings {
        frontends: frontend_settings(lookup)?,
        documents: instance_document_settings(lookup),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::test_support::{config_with, try_config_with};

    #[test]
    fn frontends_default_to_the_stock_directory_and_no_catalogue() {
        let config = config_with(&[]);
        assert_eq!(
            config.statics.frontends.dir,
            PathBuf::from("./frontends"),
            "default FRONTENDS_DIR"
        );
        assert!(
            config.statics.frontends.available.is_empty(),
            "nothing is installable until the operator names it"
        );
        for blank in ["", "   "] {
            let blank_catalogue = config_with(&[(FRONTENDS_AVAILABLE_VAR, blank)]);
            assert!(
                blank_catalogue.statics.frontends.available.is_empty(),
                "FRONTENDS_AVAILABLE={blank:?} means no frontends"
            );
        }
    }

    #[test]
    fn reads_the_frontends_catalogue() {
        let config = config_with(&[
            (FRONTENDS_DIR_VAR, "/srv/frontends"),
            (
                FRONTENDS_AVAILABLE_VAR,
                r#"[{"name":"pleroma-fe","git":"https://example.com/fe","build_url":"https://build.example/${ref}.zip","ref":"stable","build_dir":"www"},
                    {"name":"volar"}]"#,
            ),
        ]);
        assert_eq!(
            config.statics.frontends.dir,
            PathBuf::from("/srv/frontends"),
            "custom FRONTENDS_DIR"
        );
        assert_eq!(config.statics.frontends.available.len(), 2, "two entries");
        assert_eq!(
            config.statics.frontends.available[0],
            FrontendConfig {
                name: "pleroma-fe".to_owned(),
                git: Some("https://example.com/fe".to_owned()),
                build_url: Some("https://build.example/${ref}.zip".to_owned()),
                reference: Some("stable".to_owned()),
                build_dir: Some("www".to_owned()),
            },
            "the full entry"
        );
        assert_eq!(
            config.statics.frontends.available[1],
            FrontendConfig {
                name: "volar".to_owned(),
                git: None,
                build_url: None,
                reference: None,
                build_dir: None,
            },
            "every field but the name is optional"
        );
    }

    #[test]
    fn invalid_frontends_available_is_an_error() {
        for broken in ["{not json}", r#"[{"git":"https://example.com"}]"#] {
            let err = try_config_with(&[(FRONTENDS_AVAILABLE_VAR, broken)]).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidFrontends(_)),
                "FRONTENDS_AVAILABLE={broken:?} is refused, got {err:?}"
            );
            assert!(
                err.to_string().contains(FRONTENDS_AVAILABLE_VAR),
                "the message names the variable: {err}"
            );
        }
    }

    #[test]
    fn instance_documents_default_to_the_terms_and_directory() {
        let config = config_with(&[]);
        assert_eq!(
            config.statics.documents.dir,
            PathBuf::from("./instance"),
            "default INSTANCE_DOCS_DIR"
        );
        assert_eq!(
            config.statics.documents.names,
            vec!["terms-of-service".to_owned()],
            "only the document the JSON API reads"
        );
        let blank = config_with(&[(INSTANCE_DOCUMENT_NAMES_VAR, "  ")]);
        assert!(
            blank.statics.documents.names.is_empty(),
            "a set-but-blank list names no documents"
        );
        assert!(
            config_with(&[(INSTANCE_DOCS_DIR_VAR, "  ")])
                .statics
                .documents
                .dir
                .ends_with("instance"),
            "a blank directory falls back to the default"
        );
    }

    #[test]
    fn reads_the_instance_document_settings() {
        let config = config_with(&[
            (INSTANCE_DOCS_DIR_VAR, " /srv/docs "),
            (
                INSTANCE_DOCUMENT_NAMES_VAR,
                "terms-of-service, privacy ,about",
            ),
        ]);
        assert_eq!(
            config.statics.documents.dir,
            PathBuf::from("/srv/docs"),
            "custom INSTANCE_DOCS_DIR, trimmed"
        );
        assert_eq!(
            config.statics.documents.names,
            vec![
                "terms-of-service".to_owned(),
                "privacy".to_owned(),
                "about".to_owned(),
            ],
            "the names are trimmed, blanks between the commas drop"
        );
        let debug = format!("{config:?}");
        assert!(
            debug.contains("statics: StaticSettings"),
            "the store renders in Debug: {debug}"
        );
        assert!(
            debug.contains("dir: \"/srv/docs\""),
            "the documents directory renders"
        );
    }
}
