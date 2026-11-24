//! Helpers for tests that push activities through the inbound dispatcher and
//! read back the deliveries it queued.

use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    federation::{delivery::KIND_DELIVER, dispatch::handle_incoming},
    models::user::User,
    state::AppState,
};

use super::{test_app_state, test_config};

/// The job args the inbox routes write for a delivery signed by `who`.
#[must_use]
pub fn incoming_args(activity: &Value, who: &User) -> Value {
    json!({"activity": activity, "signer_id": who.id.to_string(), "inbox_owner": null})
}

/// Runs one activity through the real dispatcher as `who`.
///
/// Fails the test if the job wants a retry or refuses the document. (An
/// ignored activity completes the same: the fold's refusals are outcomes, not
/// errors.)
///
/// # Panics
///
/// If the dispatcher returns an error for the job.
pub async fn deliver(state: &AppState, activity: &Value, who: &User) {
    let kind = activity["type"].as_str().unwrap_or("activity");
    handle_incoming(state.clone(), incoming_args(activity, who))
        .await
        .unwrap_or_else(|err| panic!("the {kind} delivery completes the job: {err}"));
}

/// The dispatcher state over the stock test config.
#[must_use]
pub fn delivering_state(pool: &PgPool) -> AppState {
    test_app_state(pool.clone(), test_config())
}

/// The args of every queued `federator.deliver` job, in insertion order.
///
/// # Panics
///
/// If the jobs table cannot be read.
pub async fn delivery_jobs(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar::<_, Value>("SELECT args FROM jobs WHERE kind = $1 ORDER BY inserted_at, id")
        .bind(KIND_DELIVER)
        .fetch_all(pool)
        .await
        .expect("the delivery jobs read")
}
