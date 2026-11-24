//! The fixtures the instance documents' unit tests share: the stand-in VAPID
//! key, a plain `pleroma` block, a fixed instant, the empty v1 document and
//! a key-set reader. Test-only.

use chrono::{DateTime, TimeZone as _, Utc};
use serde_json::Value;

use crate::{config::Config, services::instance_meta::Federation};

use super::{InstanceStats, pleroma::PleromaBlock, v1::Instance};

/// The VAPID public key the document builders are tested with: a
/// stand-in for the pair [`crate::services::vapid`] keeps in
/// `instance_secrets`; the endpoints read the real one.
pub(super) const TEST_VAPID_KEY: &str = "BTESTvapidPUBLICkey_TEST";

/// The `pleroma` block the document builders are tested with: the one
/// `config` produces with the stand-in [`TEST_VAPID_KEY`] and a
/// plain, filter-free federation state — the endpoints describe the
/// live [MRF chain](crate::federation::mrf) instead, which is empty
/// on the test configuration.
pub(super) fn pleroma_block(config: &Config) -> PleromaBlock {
    PleromaBlock::build(config, TEST_VAPID_KEY, Federation::federating())
}

/// A fixed instant for the rules fixture rows; the timestamps are not
/// part of the document, so any valid one works.
pub(super) fn stamped() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
}

/// The empty-instance document on the test config: zeroed stats, no
/// rules, no admin.
pub(super) fn empty_document() -> Value {
    let config = Config::for_tests();
    let stats = InstanceStats {
        users: 0,
        statuses: 0,
        domains: 0,
    };
    serde_json::to_value(Instance::build(
        &config,
        &stats,
        &[],
        None,
        pleroma_block(&config),
    ))
    .unwrap()
}

/// The sorted object keys of `value`, for key-set assertions.
pub(super) fn keys_of(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("a document is an object")
        .keys()
        .map(String::as_str)
        .collect()
}
