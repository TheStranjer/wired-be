//! The shared body of the [`super`] emitters: read the row's author, ask
//! [`targets`] and hand the finished [`StreamEvent`] to
//! [`Hub::publish`](crate::streaming::hub::Hub::publish).

use tracing::warn;

use crate::{
    models::{status::Status, user},
    services::streaming::targets,
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
pub(super) async fn publish_status(
    state: &AppState,
    row: &Status,
    event: &str,
    payload: Option<String>,
    render: Option<RenderSpec>,
) {
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
    let found = match targets(state, row, &author).await {
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
        event: event.to_owned(),
        payload,
        audience,
        render,
        local_only,
    });
}
