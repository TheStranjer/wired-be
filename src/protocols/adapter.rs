//! The trait every protocol implements to plug into the application.

use axum::Router;

use crate::{protocols::ProtocolId, state::AppState};

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
}
