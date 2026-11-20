//! Fixtures more than one child's tests build on.

use std::slice;

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    models::user::{NewUser, User, insert, local_ap_id},
};

/// An inserted local user named `nickname`.
pub(super) async fn stored(pool: &PgPool, nickname: &str) -> User {
    insert(
        pool,
        &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
    )
    .await
    .expect("the user fixture inserts")
}

/// The nickname list every sentence test reads.
pub(super) fn names(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| (*text).to_owned()).collect()
}

/// A one-user subject slice, for the helpers a test calls on a lone
/// account.
pub(super) fn one(user: &User) -> &[User] {
    slice::from_ref(user)
}

/// The fixed report id the report and status sentence tests read.
pub(super) fn report_id() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000042").expect("test uuid")
}
