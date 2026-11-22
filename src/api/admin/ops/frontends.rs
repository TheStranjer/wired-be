//! The frontend routes: the configured frontends with their installed
//! flag, and the install request merged over the configuration.

use axum::{Json, extract::State};
use bytes::Bytes;
use serde::Deserialize;
use tracing::{debug, warn};

use super::{
    AREA_FRONTENDS, CLASS_READ, CLASS_WRITE, DEFAULT_REF, ERROR_STATUS_MIN, FILE_FIELD,
    MAX_BUILD_BYTES, MISSING_NAME, REF_PLACEHOLDER,
    frontend_archive::{check_relative_path, check_segment, install_failure, land_archive},
};
use crate::{
    auth::admin::AdminUser,
    config::{Config, FrontendConfig},
    entities::admin::{AdminFrontend, FrontendInstallStatus},
    error::{AppError, AppResult},
    extract::multipart::{MultipartForm, UploadedFiles as _},
    state::AppState,
};

/// `GET /api/v1/pleroma/admin/frontends` — Pleroma's `FrontendController.index`
/// narrowed to the ticket's array: the configured frontends, each with
/// its `installed` flag read off the disk.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate.
pub(super) async fn list_frontends(
    State(state): State<AppState>,
    admin: AdminUser,
) -> AppResult<Json<Vec<AdminFrontend>>> {
    admin.require(CLASS_READ, AREA_FRONTENDS)?;
    let frontends = state
        .config
        .statics
        .frontends
        .available
        .iter()
        .map(|entry| AdminFrontend::from_config(entry, frontend_installed(&state.config, entry)))
        .collect();
    Ok(Json(frontends))
}

/// Whether `entry` is installed: `FRONTENDS_DIR/<name>/<ref>` is a
/// directory (Pleroma's `installed()` / `installed_refs()` ls checks,
/// `frontend_controller.ex`). An entry without a ref is flagged by its
/// `<name>` directory alone.
fn frontend_installed(config: &Config, entry: &FrontendConfig) -> bool {
    let mut path = config.statics.frontends.dir.join(&entry.name);
    if let Some(reference) = &entry.reference {
        path.push(reference);
    }
    path.is_dir()
}

/// The text parameters of `POST /frontends/install`: every field but
/// `name` optional, present values winning over the config entry's
/// (Pleroma's `Frontend.install` merge, `lib/pleroma/frontend.ex`). The
/// `file` part is not a field: it arrives through [`MultipartForm`]'s
/// file half.
#[derive(Debug, Default, Deserialize)]
pub(super) struct InstallParams {
    /// The frontend to install; required.
    #[serde(default)]
    pub(super) name: Option<String>,
    /// The build ref — the JSON/form key `ref`, a Rust keyword.
    #[serde(default, rename = "ref")]
    pub(super) reference: Option<String>,
    /// Where to download the build archive, overriding the config.
    #[serde(default)]
    pub(super) build_url: Option<String>,
    /// The archive subdirectory to install, overriding the config.
    #[serde(default)]
    pub(super) build_dir: Option<String>,
}

/// A resolved install: what the request and the `FRONTENDS_AVAILABLE`
/// entry together decide, before a byte moves.
#[derive(Debug)]
pub(super) struct Install {
    /// The `<name>` directory segment.
    name: String,
    /// The `<ref>` directory segment under it.
    reference: String,
    /// The archive subdirectory to install; `None` means the whole
    /// archive root.
    build_dir: Option<String>,
    /// Where to download the archive when the body uploads no file.
    build_url: Option<String>,
}

/// `POST /api/v1/pleroma/admin/frontends/install` — Pleroma's
/// `FrontendController.install` over `Pleroma.Frontend.install`: take the
/// uploaded `file` or download the `build_url` archive, extract it safely
/// and land it at `FRONTENDS_DIR/<name>/<ref>`; 200 `{"status":"ok"}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::BadRequest`] with
/// [`MISSING_NAME`] for a body without `name`, and with
/// `Could not install frontend: ...` for an unknown name without a
/// `build_url`, an unsafe name, ref or build directory, a failed
/// download, and an archive that is unreadable, over-full or unsafe;
/// [`AppError::PayloadTooLarge`] for an uploaded file past the upload
/// limits. Database work belongs to none of this.
pub(super) async fn install_frontend(
    State(state): State<AppState>,
    admin: AdminUser,
    mut form: MultipartForm<InstallParams>,
) -> AppResult<Json<FrontendInstallStatus>> {
    admin.require(CLASS_WRITE, AREA_FRONTENDS)?;
    let upload = form.files.take(FILE_FIELD).map(|file| file.bytes);
    let install = resolve_install(&state.config, &form.fields, upload.is_some())?;
    let archive = match upload {
        Some(bytes) => bytes,
        None => download_build(&state, &install).await?,
    };
    let destination = state
        .config
        .statics
        .frontends
        .dir
        .join(&install.name)
        .join(&install.reference);
    land_archive(
        &state.config.statics.frontends.dir,
        destination,
        install.build_dir.as_deref(),
        archive,
    )
    .await?;
    debug!(
        frontend = %install.name,
        reference = %install.reference,
        "frontend installed"
    );
    Ok(Json(FrontendInstallStatus::OK))
}

/// The request-over-config merge, Pleroma's `Map.merge(config, cmd)` with
/// the `cmd || config` fallback: the name must be there and safe, the ref
/// falls to [`DEFAULT_REF`], the `build_url` to the config entry — and
/// the ticket's refusal answers before anything else when the name matches
/// no entry and the request carries neither a `build_url` nor a file.
///
/// # Errors
///
/// [`AppError::BadRequest`] with [`MISSING_NAME`], or with the
/// `Could not install frontend: ` prefix for the unknown name, the
/// source-less install and the unsafe name/ref/build directory.
pub(super) fn resolve_install(
    config: &Config,
    params: &InstallParams,
    has_file: bool,
) -> AppResult<Install> {
    let name = params
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::BadRequest(MISSING_NAME.to_owned()))?
        .to_owned();
    let entry = config
        .statics
        .frontends
        .available
        .iter()
        .find(|candidate| candidate.name == name);
    let build_url = first_of(
        params.build_url.as_deref(),
        entry.and_then(|e| e.build_url.as_deref()),
    )
    .map(str::to_owned);
    if entry.is_none() && build_url.is_none() && !has_file {
        return Err(install_failure(&format!("unknown frontend {name:?}")));
    }
    let reference = first_of(
        params.reference.as_deref(),
        entry.and_then(|e| e.reference.as_deref()),
    )
    .unwrap_or(DEFAULT_REF)
    .to_owned();
    let build_dir = first_of(
        params.build_dir.as_deref(),
        entry.and_then(|e| e.build_dir.as_deref()),
    )
    .map(str::to_owned);
    if build_url.is_none() && !has_file {
        return Err(install_failure(
            "no build_url is configured and no file was uploaded",
        ));
    }
    check_segment(&name, "frontend name")?;
    check_segment(&reference, "frontend ref")?;
    if let Some(build_dir) = &build_dir {
        check_relative_path(build_dir, "build directory")?;
    }
    Ok(Install {
        name,
        reference,
        build_dir,
        build_url,
    })
}

/// The request value when it carries text, the config value when the
/// request omits it; `None` when neither does.
pub(super) fn first_of<'a>(sent: Option<&'a str>, configured: Option<&'a str>) -> Option<&'a str> {
    let usable = |text: &'a str| (!text.trim().is_empty()).then(|| text.trim());
    sent.and_then(usable)
        .or_else(|| configured.and_then(usable))
}

/// Fetches the build archive from `install`'s `build_url` — `${ref}`
/// substituted, Pleroma's `download_build` — through the instance's
/// SSRF-guarded outbound client, capped at [`MAX_BUILD_BYTES`].
///
/// # Errors
///
/// [`AppError::BadRequest`] with the install refusal for a missing
/// source, a failed or refused fetch, and a body over the cap.
async fn download_build(state: &AppState, install: &Install) -> AppResult<Bytes> {
    let Some(build_url) = &install.build_url else {
        return Err(install_failure(
            "no build_url is configured and no file was uploaded",
        ));
    };
    let url = build_url.replace(REF_PLACEHOLDER, &install.reference);
    match state
        .http_client
        .get_limited(&url, &[], MAX_BUILD_BYTES)
        .await
    {
        Ok(response) if response.status < ERROR_STATUS_MIN => Ok(response.body),
        Ok(response) => {
            warn!(status = response.status, %url, "frontend install: the build server refused");
            Err(install_failure(&format!(
                "the build url answered {}",
                response.status
            )))
        }
        Err(err) => {
            warn!(%err, %url, "frontend install: the build could not be downloaded");
            Err(install_failure(&format!(
                "the build could not be downloaded: {err}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::{config_with_frontends, is_install_failure, resolved};
    use super::*;

    #[test]
    fn an_unknown_frontend_without_a_build_url_is_the_pinned_refusal() {
        let config = config_with_frontends(&[("plume", Some("stable"))]);
        let err = resolve_install(
            &config,
            &InstallParams {
                name: Some("ghost".to_owned()),
                ..InstallParams::default()
            },
            false,
        )
        .expect_err("no config entry and no build_url install nothing");
        assert!(is_install_failure(&err, "unknown frontend"), "{err:?}");
    }

    #[test]
    fn the_body_without_a_name_is_the_missing_parameter_400() {
        let config = Config::for_tests();
        let err = resolve_install(&config, &InstallParams::default(), false)
            .expect_err("no name is no install");
        assert_eq!(err.to_string(), MISSING_NAME, "the named parameter");
    }

    #[test]
    fn an_unknown_name_with_a_build_url_installs_anyway() {
        let config = Config::for_tests();
        let install = resolved(
            &config,
            &InstallParams {
                name: Some("custom-fe".to_owned()),
                reference: Some("v2.0.0".to_owned()),
                build_url: Some("https://build.example/${ref}.zip".to_owned()),
                build_dir: Some("webout".to_owned()),
            },
            false,
        );
        assert_eq!(install.name, "custom-fe", "the name passes through");
        assert_eq!(install.reference, "v2.0.0", "and the sent ref");
        assert_eq!(
            install.build_dir.as_deref(),
            Some("webout"),
            "and the sent build dir"
        );
        assert!(
            install
                .build_url
                .is_some_and(|url| url.contains(REF_PLACEHOLDER)),
            "the url keeps its placeholder for the fetch"
        );
    }

    #[test]
    fn the_config_entry_supplies_what_the_request_omits() {
        let mut config = Config::for_tests();
        config.statics.frontends.available = vec![FrontendConfig {
            name: "plume".to_owned(),
            git: Some("https://example.org/plume".to_owned()),
            build_url: Some("https://build.example/${ref}.zip".to_owned()),
            reference: Some("stable".to_owned()),
            build_dir: Some("output".to_owned()),
        }];

        let from_config = resolved(
            &config,
            &InstallParams {
                name: Some("plume".to_owned()),
                ..InstallParams::default()
            },
            false,
        );
        assert_eq!(from_config.reference, "stable", "the config ref");
        assert_eq!(
            from_config.build_dir.as_deref(),
            Some("output"),
            "the config build dir"
        );

        let overridden = resolved(
            &config,
            &InstallParams {
                name: Some("plume".to_owned()),
                reference: Some("nightly".to_owned()),
                build_url: None,
                build_dir: None,
            },
            false,
        );
        assert_eq!(overridden.reference, "nightly", "the request wins");

        let mut no_ref = Config::for_tests();
        no_ref.statics.frontends.available = vec![FrontendConfig {
            name: "bare".to_owned(),
            git: None,
            build_url: Some("https://build.example/x.zip".to_owned()),
            reference: None,
            build_dir: None,
        }];
        let defaulted = resolved(
            &no_ref,
            &InstallParams {
                name: Some("bare".to_owned()),
                ..InstallParams::default()
            },
            false,
        );
        assert_eq!(defaulted.reference, DEFAULT_REF, "the ref defaults");
        assert_eq!(
            defaulted.build_dir, None,
            "and no build dir means the dist-then-root choice"
        );
    }

    #[test]
    fn a_known_name_without_any_source_is_refused() {
        let config = config_with_frontends(&[("plume", Some("stable"))]);
        let err = resolve_install(
            &config,
            &InstallParams {
                name: Some("plume".to_owned()),
                ..InstallParams::default()
            },
            false,
        )
        .expect_err("no url and no file installs nothing");
        assert!(
            is_install_failure(&err, "no build_url"),
            "the source-less refusal: {err:?}"
        );
    }

    #[test]
    fn a_known_name_with_an_uploaded_file_needs_no_build_url() {
        let config = config_with_frontends(&[("plume", Some("stable"))]);
        let install = resolved(
            &config,
            &InstallParams {
                name: Some("plume".to_owned()),
                ..InstallParams::default()
            },
            true,
        );
        assert_eq!(install.build_url, None, "the config entry carries no url");
        assert_eq!(install.reference, "stable", "the config ref still stands");
    }

    #[test]
    fn the_frontends_read_both_build_paths() {
        let root = tempfile::tempdir().expect("a frontends dir");
        let mut config = Config::for_tests();
        config.statics.frontends.dir = root.path().to_path_buf();
        config.statics.frontends.available = vec![
            FrontendConfig {
                name: "plume".to_owned(),
                git: None,
                build_url: None,
                reference: Some("stable".to_owned()),
                build_dir: None,
            },
            FrontendConfig {
                name: "soap".to_owned(),
                git: None,
                build_url: None,
                reference: Some("master".to_owned()),
                build_dir: None,
            },
            FrontendConfig {
                name: "bare".to_owned(),
                git: None,
                build_url: None,
                reference: None,
                build_dir: None,
            },
        ];
        std::fs::create_dir_all(root.path().join("plume").join("stable"))
            .expect("the installed dir");
        std::fs::write(
            root.path().join("plume").join("stable").join("index.html"),
            b"",
        )
        .expect("the installed file");
        std::fs::create_dir_all(root.path().join("bare")).expect("the bare dir");

        assert!(
            frontend_installed(&config, &config.statics.frontends.available[0]),
            "plume/stable stands"
        );
        assert!(
            !frontend_installed(&config, &config.statics.frontends.available[1]),
            "soap/master was never built"
        );
        assert!(
            frontend_installed(&config, &config.statics.frontends.available[2]),
            "a refless entry reads its name directory"
        );
    }

    /// The entity renders exactly the ticket's five keys, `ref` spelled
    /// as JSON demands and the nulls as nulls.
    #[test]
    fn a_frontend_entity_carries_the_five_pinned_keys() {
        let entry = FrontendConfig {
            name: "plume".to_owned(),
            git: Some("https://example.org/plume".to_owned()),
            build_url: None,
            reference: Some("stable".to_owned()),
            build_dir: Some("dist".to_owned()),
        };
        let rendered = serde_json::to_value(AdminFrontend::from_config(&entry, true))
            .expect("the entity serializes");
        assert_eq!(
            rendered,
            json!({
                "name": "plume",
                "git": "https://example.org/plume",
                "build_url": null,
                "ref": "stable",
                "installed": true,
            }),
            "build_dir stays server-side, ref is the JSON key"
        );
    }
}
