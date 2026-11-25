//! The trait every protocol implements to plug into the application.

use axum::{Router, routing::MethodRouter};
use futures::future::BoxFuture;
use sqlx::PgConnection;

use crate::{
    api::well_known::{FingerLink, WebfingerContribution},
    config::Config,
    error::AppResult,
    jobs::HandlerFn,
    models::user::User,
    protocols::{
        ProtocolId,
        identity::{ClaimOutcome, Identity},
    },
    state::AppState,
};

/// One protocol's entry point into the application.
///
/// The registry ([`crate::protocols::registry`]) hands out one adapter per
/// [`ProtocolId`], and [`crate::app`] mounts only the adapters the operator
/// enabled with `PROTOCOLS`.
///
/// # The convention for methods
///
/// - A **required** method (no default body) is something every protocol
///   must decide for itself. A new protocol cannot compile until it answers.
/// - A method **with a default** that does nothing is an *optional hook*: a
///   protocol overrides it only when it has something to contribute.
///
/// Later tickets add hooks by following this rule. Every hook added here must
/// also be implemented by the fixture adapter (`protocols::fixture`, in test
/// builds) and tested through it.
pub trait ProtocolAdapter: Sync {
    /// The protocol this adapter speaks.
    fn id(&self) -> ProtocolId;

    /// The protocol's own HTTP routes, or `Router::new()` if it has none.
    ///
    /// The routes are merged at the top level of the application router, so
    /// they must carry their full paths.
    fn router(&self, state: &AppState) -> Router<AppState>;

    /// The background job kinds this protocol handles, each with its handler.
    ///
    /// [`crate::job_registry`] registers them only while the protocol is
    /// enabled, so a disabled protocol has no handlers. A kind must not repeat
    /// one of core's or another protocol's. The default is no job kinds.
    fn job_handlers(&self) -> &'static [(&'static str, HandlerFn)] {
        &[]
    }

    /// The job queues this protocol enqueues onto.
    ///
    /// Each one must be in [`crate::jobs::DEFAULT_JOB_QUEUES`], or the default
    /// worker never drains it. The default is no queues.
    fn job_queues(&self) -> &'static [&'static str] {
        &[]
    }

    /// The `/.well-known` paths this protocol alone serves, each with its handler.
    ///
    /// [`crate::discovery::router`] mounts them only while the protocol is
    /// enabled. Every path must start with `/.well-known/` and must not be
    /// claimed by core or another protocol; a document several protocols
    /// contribute to is core's, not an owned path. The default is no paths.
    fn well_known_routes(&self) -> Vec<(&'static str, MethodRouter<AppState>)> {
        Vec::new()
    }

    /// The aliases and links this protocol adds to a local user's `WebFinger` document.
    ///
    /// Core answers the lookup and writes the subject; then, for each enabled
    /// protocol in registry order, it appends this contribution. The default
    /// contributes nothing.
    fn webfinger(&self, _config: &Config, _user: &User) -> WebfingerContribution {
        WebfingerContribution::default()
    }

    /// The links this protocol adds to the host-meta document.
    ///
    /// host-meta (`/.well-known/host-meta`) is a shared document: core writes
    /// the XRD root, then appends the links of each enabled protocol in
    /// registry order. `ActivityPub` adds its `WebFinger` template (the `lrdd`
    /// link); later, XMPP adds its `WebSocket` and BOSH connection links here
    /// (XEP-0156). The default contributes nothing.
    fn host_meta_links(&self, _config: &Config) -> Vec<FingerLink> {
        Vec::new()
    }

    /// The name `NodeInfo` lists this protocol under in `protocols`.
    ///
    /// `NodeInfo` has its own fixed vocabulary of protocol names
    /// (<https://nodeinfo.diaspora.software>), which is not always the
    /// `WiredBE` [`ProtocolId`]. A protocol `NodeInfo` has no name for returns
    /// `None` and is left out of the list. Core lists the names of the enabled
    /// protocols in registry order. The default is no name.
    fn nodeinfo_protocol(&self) -> Option<&'static str> {
        None
    }

    /// The [`FieldErrors`](crate::models::user::register::FieldErrors) key of this protocol's ID.
    ///
    /// Errors from [`Self::validate_identifier`] are reported under it.
    /// `ActivityPub` answers `"nickname"`, which keeps the keys of existing
    /// registration errors.
    fn identifier_field(&self) -> &'static str;

    /// Normalises `requested` and accepts it as this protocol's ID, or says why not.
    ///
    /// Returns the normalised ID on success and every message the ID earned
    /// on failure. It judges the shape only; whether the ID is free is
    /// [`Self::identifier_taken`].
    ///
    /// # Errors
    ///
    /// The messages, in the order the rules ran.
    fn validate_identifier(&self, requested: &str, config: &Config) -> Result<String, Vec<String>>;

    /// Converts any `WiredBE` nickname into an ID this protocol accepts.
    ///
    /// The result must pass [`Self::validate_identifier`] for every valid
    /// nickname. It need not be free:
    /// [`suggest_identifier`](crate::protocols::identity::suggest_identifier)
    /// makes it unique.
    fn derive_identifier(&self, nickname: &str, config: &Config) -> String;

    /// Whether `identifier` already belongs to an account of this protocol.
    ///
    /// # Errors
    ///
    /// Any database failure.
    fn identifier_taken<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        identifier: &'a str,
    ) -> BoxFuture<'a, AppResult<bool>>;

    /// `identifier` written as a URI, for linking identities across protocols.
    ///
    /// `ActivityPub` gives the actor URL.
    fn identifier_uri(&self, identifier: &str, config: &Config) -> String;

    /// The identity `user` has on this protocol, or `None` when not enrolled.
    ///
    /// The protocol's own main table is the identity record: a row there
    /// means enrolled, and its status says whether the identity is claimed.
    ///
    /// # Errors
    ///
    /// Any database failure.
    fn identity<'a>(
        &'a self,
        state: &'a AppState,
        user: &'a User,
    ) -> BoxFuture<'a, AppResult<Option<Identity>>>;

    /// Writes `user`'s identity on this protocol inside the caller's transaction.
    ///
    /// `identifier` is already validated. The write goes through `conn`, which
    /// is the caller's open transaction, so a claim that is later rolled back
    /// leaves nothing behind. It relies on the protocol's unique constraint
    /// to answer [`ClaimOutcome::Taken`] on a race; the claim rules are in
    /// [`crate::protocols::identity`].
    ///
    /// # Errors
    ///
    /// Any database failure other than the identity being taken.
    fn claim<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        user: &'a User,
        identifier: &'a str,
        state: &'a AppState,
    ) -> BoxFuture<'a, AppResult<ClaimOutcome>>;
}
