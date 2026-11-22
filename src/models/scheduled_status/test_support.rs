//! The fixtures the `scheduled_status` module's unit tests share: the local
//! user, the schedule written for it, and the fixed UTC instants the
//! day-boundary and due-time fixtures hang on. Test-only.

use chrono::{DateTime, TimeZone as _, Utc};
use serde_json::Value;
use sqlx::PgPool;

use crate::{
    config::TEST_BASE_URL,
    models::user::{NewUser, User, insert as insert_user, local_ap_id},
};

use super::{NewScheduledStatus, ScheduledStatus, insert};

/// The instance origin the fixtures create users under.
pub(super) const BASE: &str = TEST_BASE_URL;

/// A local user named `nickname`.
pub(super) async fn user(pool: &PgPool, nickname: &str) -> User {
    insert_user(
        pool,
        &NewUser::local(nickname, &local_ap_id(BASE, nickname)),
    )
    .await
    .expect("the fixture user inserts")
}

/// A schedule for `who`, due `when`, with `params` stored as given.
pub(super) async fn schedule(
    pool: &PgPool,
    who: &User,
    when: DateTime<Utc>,
    params: Value,
) -> ScheduledStatus {
    insert(
        pool,
        &NewScheduledStatus {
            user_id: who.id,
            scheduled_at: when,
            params,
        },
    )
    .await
    .expect("the fixture schedule inserts")
}

/// A schedule written and left behind: these tests assert through the
/// model's counts, sweeps and gate, not through the row.
pub(super) async fn pending(pool: &PgPool, who: &User, when: DateTime<Utc>, params: Value) {
    let row = schedule(pool, who, when, params).await;
    assert_eq!(row.user_id, who.id, "the fixture row is its account's");
}

/// A fixed instant to hang day-boundary fixtures on: hour-minute-second
/// of 2026-05-17 UTC.
pub(super) fn at(h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 17, h, mi, s).unwrap()
}
