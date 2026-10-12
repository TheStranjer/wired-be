//! A small in-memory cache whose entries go stale at a deadline.
//!
//! [`TimedCache`] is the whole of it: one slot holding one cloneable value
//! and the instant it stops being fresh. Callers pass the current time to
//! every read and write — there is no clock inside — so the expiry logic is
//! directly testable at any instant, and the production caller decides what
//! "now" means ([`chrono::Utc::now`] for the endpoints that use it). The
//! instance-stats cache of [`crate::api::instance`] is the first user;
//! later endpoints with expensive aggregate reads reuse the same helper.
//!
//! The slot sits behind a [`Mutex`] because the cache is shared state on
//! [`AppState`](crate::state::AppState) and handlers reach it from many
//! tasks at once. Lock contention is a single uncontended acquisition per
//! request, and a poisoned lock (a panic in some unrelated caller while
//! holding it) still yields the intact slot: entries are plain values, so
//! recovering the guard cannot propagate broken state.

use std::{
    fmt,
    sync::{Mutex, MutexGuard, PoisonError},
};

use chrono::{DateTime, TimeDelta, Utc};

/// One cached value and the instant it stops being fresh.
#[derive(Debug)]
struct Entry<T> {
    /// The end of the value's freshness window, as an insert-time plus the
    /// cache's TTL. A read at exactly this instant sees stale data.
    expires_at: DateTime<Utc>,
    /// The cached value itself.
    value: T,
}

/// A single-slot cache whose value expires `ttl` after each
/// [`insert`](Self::insert).
///
/// [`get`](Self::get) answers the stored value only while it is still
/// fresh; a stale or absent slot reads as `None` and the caller recomputes
/// and re-inserts. Nothing evicts eagerly: an expired entry simply stops
/// being returned until the next insert overwrites it.
pub struct TimedCache<T> {
    /// How long an inserted value stays fresh.
    ttl: TimeDelta,
    /// The one cached entry, `None` while the cache is empty.
    slot: Mutex<Option<Entry<T>>>,
}

impl<T> TimedCache<T> {
    /// An empty cache whose entries stay fresh for `ttl` after each insert.
    #[must_use]
    pub const fn new(ttl: TimeDelta) -> Self {
        Self {
            ttl,
            slot: Mutex::new(None),
        }
    }

    /// The slot's guard, recovering the contents if an earlier panic
    /// poisoned the lock: an [`Entry`] is plain owned data, never
    /// half-updated by a panicking writer.
    fn slot(&self) -> MutexGuard<'_, Option<Entry<T>>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: Clone> TimedCache<T> {
    /// The cached value, when one is stored and `now` has not reached its
    /// expiry. A read never extends freshness: the deadline is fixed at
    /// insert time.
    #[must_use]
    pub fn get(&self, now: DateTime<Utc>) -> Option<T> {
        let slot = self.slot();
        slot.as_ref()
            .filter(|entry| entry.expires_at > now)
            .map(|entry| entry.value.clone())
    }

    /// Stores `value`, fresh until `now + ttl`, replacing whatever the slot
    /// held before.
    pub fn insert(&self, now: DateTime<Utc>, value: T) {
        let mut slot = self.slot();
        *slot = Some(Entry {
            expires_at: now + self.ttl,
            value,
        });
    }
}

/// Reports the TTL and whether the slot holds anything, not the value: a
/// cached row can be large, and debug output should not hide that cost.
impl<T> fmt::Debug for TimedCache<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimedCache")
            .field("ttl", &self.ttl)
            .field("filled", &self.slot().is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    // Trait-only import: `TimeZone` is needed in scope for
    // `Utc::with_ymd_and_hms`, never named.
    use chrono::TimeZone as _;

    use super::*;

    /// A fixed instant the cache is read and written at: 2026-10-11T12:00:00Z.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    /// The instant `secs` seconds after [`now`].
    fn after(secs: i64) -> DateTime<Utc> {
        now() + TimeDelta::seconds(secs)
    }

    /// A cache with a 60 s window, the endpoint's TTL shape.
    fn cache() -> TimedCache<String> {
        TimedCache::new(TimeDelta::seconds(60))
    }

    #[test]
    fn a_fresh_cache_holds_nothing() {
        let cache = cache();
        assert_eq!(cache.get(now()), None, "an empty cache misses");
        assert_eq!(
            cache.get(after(-100)),
            None,
            "the past holds nothing either"
        );
    }

    #[test]
    fn an_inserted_value_is_fresh_until_the_window_closes() {
        let cache = cache();
        cache.insert(now(), "one".to_owned());
        assert_eq!(cache.get(now()).as_deref(), Some("one"), "fresh at once");
        assert_eq!(
            cache.get(after(59)).as_deref(),
            Some("one"),
            "still fresh one second before expiry"
        );
        assert_eq!(
            cache.get(after(60)),
            None,
            "at exactly the deadline the value is stale"
        );
        assert_eq!(cache.get(after(61)).as_deref(), None, "and after it");
    }

    #[test]
    fn reads_never_extend_the_window() {
        let cache = cache();
        cache.insert(now(), "one".to_owned());
        for step in [10, 20, 30, 40, 50] {
            assert_eq!(
                cache.get(after(step)).as_deref(),
                Some("one"),
                "reading at +{step}s answers from the cache"
            );
        }
        assert_eq!(
            cache.get(after(60)),
            None,
            "the deadline stayed where the insert put it"
        );
    }

    #[test]
    fn an_insert_replaces_the_value_and_restamps_the_window() {
        let cache = cache();
        cache.insert(now(), "one".to_owned());
        cache.insert(after(30), "two".to_owned());
        assert_eq!(
            cache.get(after(59)).as_deref(),
            Some("two"),
            "the newer insert answers inside the old window too"
        );
        assert_eq!(
            cache.get(after(89)).as_deref(),
            Some("two"),
            "the second insert's own window is still open"
        );
        assert_eq!(
            cache.get(after(90)),
            None,
            "and closes 60 s after that insert"
        );
    }

    #[test]
    fn a_zero_ttl_value_is_never_fresh() {
        let cache = TimedCache::new(TimeDelta::zero());
        cache.insert(now(), "now".to_owned());
        assert_eq!(
            cache.get(now()),
            None,
            "expiry at insert time leaves no window"
        );
    }

    #[test]
    fn debug_reports_the_window_not_the_value() {
        let cache = cache();
        cache.insert(now(), "a-cached-secret".to_owned());
        let text = format!("{cache:?}");
        assert!(
            text.contains("filled: true"),
            "the slot state is visible: {text}"
        );
        assert!(
            !text.contains("a-cached-secret"),
            "the cached value stays out of Debug output: {text}"
        );
        let empty = format!("{:?}", TimedCache::<String>::new(TimeDelta::seconds(60)));
        assert!(empty.contains("filled: false"), "an empty cache says so");
    }
}
