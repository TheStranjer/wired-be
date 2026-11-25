//! `NodeInfo`: the `/.well-known/nodeinfo` discovery document and the
//! `/nodeinfo/{version}` statistics (`docs/ARCHITECTURE.md` decision 1).
//!
//! The routers are mounted at the top level, not under `/api`
//! (`/.well-known/nodeinfo` by [`crate::discovery`], `/nodeinfo/{version}`
//! by [`crate::app`]): both addresses are fixed by the `NodeInfo` specification
//! (<https://nodeinfo.diaspora.software>). Pleroma serves them from
//! `lib/pleroma/web/nodeinfo/nodeinfo_controller.ex` and builds the
//! document in `lib/pleroma/web/nodeinfo/nodeinfo.ex`; its
//! `TrailingFormatPlug` (`lib/pleroma/web/plugs/trailing_format_plug.ex`)
//! answers `/nodeinfo/2.0.json` as `/nodeinfo/2.0`, so both spellings of
//! both versions are the same document here.
//!
//! The document reports what this instance is: software identity (name
//! `"wired"` — this server is Wired — with Pleroma named in the version's
//! compatibility comment as the interface it replicates), federation
//! protocols, registration state, usage counters served from the cached
//! [`instance::stats`] the instance document
//! shares, and the
//! Pleroma-compatible `metadata` block. Where the ticket's shape and
//! Pleroma 2.10.2's defaults meet, the ticket wins; values Pleroma reads
//! from its runtime config take this instance's [`Config`] where it has an
//! equivalent setting and Pleroma's shipped default otherwise. The
//! `features` and `restrict_unauthenticated` lists come from
//! [`crate::services::instance_meta`], the builder the `/api/v1/instance`
//! endpoints share, and `metadata.federation` is the very same answer
//! [`crate::api::instance`] publishes: the live [MRF
//! chain](crate::federation::mrf) under the operator's `MRF_TRANSPARENCY`
//! and `MRF_TRANSPARENCY_EXCLUSIONS` settings.
//!
//! [`Config`]: crate::config::Config

mod document;
mod metadata;
mod usage;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderValue, header},
    response::{IntoResponse as _, Response},
    routing::get,
};

use self::{
    document::{Discovery, NodeInfo},
    metadata::Metadata,
    usage::{Usage, read_staff_accounts},
};
use crate::{
    api::instance,
    error::{AppError, AppResult},
    federation::mrf,
    services::{instance_meta::Federation, runtime_config::RuntimeConfig},
    state::AppState,
};

/// The `rel` announcing the `NodeInfo` 2.0 schema (Pleroma's `schemas/2`).
const REL_20: &str = "http://nodeinfo.diaspora.software/ns/schema/2.0";
/// The `rel` announcing the `NodeInfo` 2.1 schema.
const REL_21: &str = "http://nodeinfo.diaspora.software/ns/schema/2.1";
/// `software.name`: this server identifies itself as Wired. Clients that
/// speak Pleroma's dialect detect it through the `features` list (see
/// [`crate::services::instance_meta`]), not by sniffing this name.
const SOFTWARE_NAME: &str = "wired";
/// 404 body of `/nodeinfo/{version}` for a schema this server does not
/// serve, Pleroma's `render_error(:not_found, …)` message.
const UNHANDLED_VERSION: &str = "Nodeinfo schema version not handled";
/// `Content-Type` of the 2.0 document: the JSON media type with the
/// schema's own URI as `profile` parameter (Pleroma's
/// `put_resp_header("content-type", …)`).
const CONTENT_TYPE_20: &str =
    "application/json; profile=\"http://nodeinfo.diaspora.software/ns/schema/2.0#\"";
/// The same for the 2.1 document.
const CONTENT_TYPE_21: &str =
    "application/json; profile=\"http://nodeinfo.diaspora.software/ns/schema/2.1#\"";
/// The only protocol this server federates with.
const PROTOCOLS: [&str; 1] = ["activitypub"];

/// The path of the `NodeInfo` discovery document.
pub const DISCOVERY_PATH: &str = "/.well-known/nodeinfo";

/// The `/nodeinfo/{version}` router, merged by [`crate::app`].
///
/// The address lives outside `/api` and outside `/.well-known`: it is the one
/// the discovery document hands out (all four spellings the spec and
/// Pleroma's links use — `2.0`, `2.0.json`, `2.1`, `2.1.json` — arrive at the
/// one route). The `/.well-known/nodeinfo` document is in
/// [`well_known_router`].
pub fn router() -> Router<AppState> {
    Router::new().route("/nodeinfo/{version}", get(nodeinfo))
}

/// The `/.well-known/nodeinfo` route, mounted by [`crate::discovery::router`].
///
/// It is the discovery address the `NodeInfo` spec fixes.
pub fn well_known_router() -> Router<AppState> {
    Router::new().route(DISCOVERY_PATH, get(schemas))
}

/// `GET /.well-known/nodeinfo` — the discovery document: the two schema
/// links pointing at this instance's document paths (Pleroma's
/// `NodeinfoController.schemas/2`). No authentication, no parameters, no
/// failure: the document only names this instance's own URLs.
async fn schemas(State(state): State<AppState>) -> Json<Discovery> {
    Json(Discovery::for_base_url(&state.config.base_url))
}

/// `GET /nodeinfo/2.0` (`…/2.0.json`, `…/2.1`, `…/2.1.json`) — the
/// instance statistics document, answered with the schema's URI as `profile`
/// `Content-Type` parameter. No authentication: `NodeInfo` is public.
///
/// # Errors
///
/// [`AppError::NotFoundMessage`] when the path names a schema version this
/// server does not serve (Pleroma's 404 "Nodeinfo schema version not
/// handled"). Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn nodeinfo(
    State(state): State<AppState>,
    Path(segment): Path<String>,
) -> AppResult<Response> {
    let version = SchemaVersion::from_segment(&segment)
        .ok_or_else(|| AppError::NotFoundMessage(UNHANDLED_VERSION.to_owned()))?;
    let config = RuntimeConfig::new(&state.db, &state.config)
        .effective_config()
        .await?;
    let counters = instance::stats(&state).await?;
    let staff_accounts = read_staff_accounts(&state.db).await?;
    let federation = federation_block(&state).await;
    let document = NodeInfo::build(
        version,
        &config,
        Usage::from(&counters),
        Metadata::build(&config, staff_accounts, federation),
    );
    document_response(version, &document)
}

/// The `metadata.federation` block: what the live [MRF
/// chain](crate::federation::mrf) publishes under the instance's
/// transparency settings — Pleroma-style policy names beside the
/// `mrf_simple`, `mrf_simple_info` and `mrf_<policy>` objects each policy
/// keys for itself — or, with `MRF_TRANSPARENCY` off, nothing but the
/// fixed keys. The configuration read is the pipeline's own cached one —
/// the same answer [`crate::api::instance`] serves.
async fn federation_block(state: &AppState) -> Federation {
    Federation::with_mrf(
        state.config.federation_enabled.get(),
        &mrf::describe(state).await,
    )
}

/// The two `NodeInfo` schema versions this server documents, the values the
/// `version` field and the `profile` content parameter carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SchemaVersion {
    /// `NodeInfo` 2.0: the plain document.
    V20,
    /// `NodeInfo` 2.1: the plain document plus the software links (Pleroma's
    /// "2.1 just adds a repository field under software", extended with
    /// `homepage` per the same schema).
    V21,
}

impl SchemaVersion {
    /// The version a `/nodeinfo/{version}` path segment names. Pleroma's
    /// `TrailingFormatPlug` strips the `.json` format suffix on `/nodeinfo`
    /// paths before routing, so the suffixed and bare spellings are the
    /// same document; anything else — `2.2`, `latest`, a second suffix —
    /// names no schema this server serves.
    fn from_segment(segment: &str) -> Option<Self> {
        match segment.strip_suffix(".json").unwrap_or(segment) {
            "2.0" => Some(Self::V20),
            "2.1" => Some(Self::V21),
            _ => None,
        }
    }

    /// The `version` field of the document.
    const fn as_str(self) -> &'static str {
        match self {
            Self::V20 => "2.0",
            Self::V21 => "2.1",
        }
    }

    /// The full `Content-Type` of the document: JSON carrying this
    /// schema's URI as `profile` parameter.
    const fn content_type(self) -> &'static str {
        match self {
            Self::V20 => CONTENT_TYPE_20,
            Self::V21 => CONTENT_TYPE_21,
        }
    }

    /// Whether the document carries the software `repository` and
    /// `homepage` links, which the 2.0 schema has no keys for.
    const fn has_software_links(self) -> bool {
        matches!(self, Self::V21)
    }
}

/// The [`NodeInfo`] document as a response with the `profile` typed
/// `Content-Type` its schema requires.
///
/// # Errors
///
/// [`AppError::Internal`] if serialization fails, which these plain
/// structs of database-read values never do.
fn document_response(version: SchemaVersion, document: &NodeInfo) -> AppResult<Response> {
    let body = serde_json::to_vec(document)
        .map_err(|err| AppError::Internal(format!("NodeInfo serialization failed: {err}")))?;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(version.content_type()),
        )],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_name_the_two_schemas_with_or_without_suffix() {
        for (segment, want) in [
            ("2.0", SchemaVersion::V20),
            ("2.0.json", SchemaVersion::V20),
            ("2.1", SchemaVersion::V21),
            ("2.1.json", SchemaVersion::V21),
        ] {
            assert_eq!(
                SchemaVersion::from_segment(segment),
                Some(want),
                "{segment} is the {want:?} document"
            );
        }
    }

    #[test]
    fn other_segments_name_no_served_schema() {
        for segment in [
            "",
            "json",
            ".json",
            "2.0.json.json",
            "2.2",
            "2.0.htm",
            "3.0",
            "latest",
            "2",
        ] {
            assert_eq!(
                SchemaVersion::from_segment(segment),
                None,
                "{segment} names no schema this server serves"
            );
        }
    }

    #[test]
    fn each_version_carries_its_own_profile() {
        assert!(
            SchemaVersion::V20
                .content_type()
                .ends_with("/schema/2.0#\""),
            "2.0 profiles itself"
        );
        assert!(
            SchemaVersion::V21
                .content_type()
                .ends_with("/schema/2.1#\""),
            "2.1 profiles itself"
        );
        assert_eq!(SchemaVersion::V20.as_str(), "2.0", "version field");
        assert_eq!(SchemaVersion::V21.as_str(), "2.1", "version field");
    }
}
