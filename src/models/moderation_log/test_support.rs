//! Fixtures more than one child's tests build on.

use std::slice;

use uuid::Uuid;

use crate::models::user::User;

pub(super) use crate::test_fixtures::local_user as stored;

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
