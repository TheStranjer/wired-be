//! Fixtures shared by the unit tests of several emoji pack submodules.

use super::remote::DownloadParams;

/// A download body for the request-shape tests.
pub(super) fn download_body(url: &str, name: &str, as_name: Option<&str>) -> DownloadParams {
    DownloadParams {
        url: url.to_owned(),
        name: name.to_owned(),
        as_name: as_name.map(str::to_owned),
    }
}
