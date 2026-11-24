//! The admin-managed static content: the instance documents, with the
//! reader that fills one [`StaticSettings`] from the `INSTANCE_DOCUMENT*`
//! variables.

use std::path::PathBuf;

use super::{
    defaults::{DEFAULT_INSTANCE_DOCS_DIR, DEFAULT_INSTANCE_DOCUMENT_NAMES},
    parse::comma_list,
    urls::directory,
    vars::{INSTANCE_DOCS_DIR_VAR, INSTANCE_DOCUMENT_NAMES_VAR},
};

/// The two settings that locate the instance documents and say which names
/// exist, from `INSTANCE_DOCS_DIR` and `INSTANCE_DOCUMENT_NAMES`
/// (see [`crate::api::admin::apps_docs`]).
///
/// It rides inside [`StaticSettings`], the one [`Config`] field for the
/// admin-managed static trees, because the [`Config`] readers and the
/// [`Debug`] impl each have an 80-line budget (AGENTS.md).
///
/// [`Config`]: super::Config
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

/// The admin-managed static file trees: the documents the
/// instance-document routes serve.
///
/// They share one [`Config`] field because the [`Config`] readers and the
/// [`Debug`] impl each have an 80-line budget (AGENTS.md).
///
/// [`Config`]: super::Config
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticSettings {
    /// The instance documents
    /// (see [`crate::api::admin::apps_docs`]).
    pub documents: InstanceDocumentSettings,
}

/// Reads the `INSTANCE_DOCUMENT*` settings into the one
/// [`StaticSettings`].
pub(crate) fn statics_settings<F>(lookup: &F) -> StaticSettings
where
    F: Fn(&str) -> Option<String>,
{
    StaticSettings {
        documents: instance_document_settings(lookup),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::test_support::config_with;

    #[test]
    fn the_retired_frontend_catalogue_variable_is_ignored() {
        let retired = concat!("FRONTENDS", "_AVAILABLE");
        let config = config_with(&[(retired, "not json")]);
        assert_eq!(
            config.statics,
            config_with(&[]).statics,
            "the rest of the configuration still loads with its defaults"
        );
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
