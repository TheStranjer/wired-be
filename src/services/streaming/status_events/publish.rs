//! The shared body of the [`super`] emitters: read the row's author, ask
//! [`targets`] and hand the finished [`StreamEvent`] to
//! [`Hub::publish`](crate::streaming::hub::Hub::publish).

use tracing::warn;

use crate::{
    models::{status::Status, user},
    services::streaming::{delete_targets, targets},
    state::AppState,
    streaming::hub::{RenderSpec, StreamEvent},
};

/// The shared body of the three emitters: read `row`'s author, ask
/// [`targets`], and hand the caller-shaped [`StreamEvent`] to
/// [`Hub::publish`](crate::streaming::hub::Hub::publish). Every refusal
/// is a return without publishing: a missing or unreadable author, a
/// failed target read (a log line each, since the write behind the event
/// already committed) and the target rules' own `None` (no log — the
/// [rules](crate::services::streaming#when-a-status-is-never-published) already say why).
///
pub(super) async fn publish_status(state: &AppState, row: &Status, emission: Emission) {
    publish(state, row, emission, false).await;
}

/// [`publish_status`] for a `delete`: the targets come from
/// [`delete_targets`], which accepts a reblog whose original was tombstoned
/// (the state every reblog tombstoned with its original is in), where the
/// ordinary [`targets`] rules refuse it.
pub(super) async fn publish_withdrawal(state: &AppState, row: &Status, emission: Emission) {
    publish(state, row, emission, true).await;
}

/// The caller-shaped part of a [`StreamEvent`]: its name, payload and render
/// hint; the streams and audience come from the target computation.
pub(super) struct Emission {
    /// The Mastodon event name.
    pub(super) event: &'static str,
    /// The plain-text payload, if the event carries one.
    pub(super) payload: Option<String>,
    /// What each receiving socket renders, if anything.
    pub(super) render: Option<RenderSpec>,
}

/// The body behind [`publish_status`] and [`publish_withdrawal`];
/// `original_may_be_gone` picks [`delete_targets`] over [`targets`].
async fn publish(state: &AppState, row: &Status, emission: Emission, original_may_be_gone: bool) {
    let author = match user::get_by_id(&state.db, row.user_id).await {
        Ok(Some(author)) => author,
        Ok(None) => {
            warn!(
                status = %row.id,
                user = %row.user_id,
                "status stream event skipped: the row has no author to gate it by"
            );
            return;
        }
        Err(err) => {
            warn!(
                status = %row.id,
                %err,
                "status stream event skipped: reading its author failed"
            );
            return;
        }
    };
    let computed = if original_may_be_gone {
        delete_targets(state, row, &author).await
    } else {
        targets(state, row, &author).await
    };
    let found = match computed {
        Ok(found) => found,
        Err(err) => {
            warn!(
                status = %row.id,
                %err,
                "status stream event skipped: the target computation failed"
            );
            return;
        }
    };
    let Some((streams, audience, local_only)) = found else {
        return;
    };
    state.hub.publish(StreamEvent {
        streams,
        event: emission.event.to_owned(),
        payload: emission.payload,
        audience,
        render: emission.render,
        local_only,
    });
}
