//! The job runtime's retry registry and backoff schedule.

use std::{collections::HashMap, time::Duration};

use super::HandlerFn;

/// Retry delay after the first failed attempt: `backoff(1)`.
const BACKOFF_BASE_SECS: u64 = 15;
/// Ceiling for [`backoff`]: six hours.
const BACKOFF_CAP_SECS: u64 = 6 * 60 * 60;

/// Maps job kinds to their handlers.
#[derive(Debug, Default)]
pub struct JobRegistry {
    /// Kind to handler table.
    handlers: HashMap<&'static str, HandlerFn>,
}

impl JobRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds `kind` to `handler`, replacing any previous binding.
    pub fn register(&mut self, kind: &'static str, handler: HandlerFn) {
        let _previous = self.handlers.insert(kind, handler);
    }

    /// The handler bound to `kind`, `None` when the kind is unknown.
    #[must_use]
    pub fn get(&self, kind: &str) -> Option<HandlerFn> {
        self.handlers.get(kind).copied()
    }
}

/// The exponential retry delay for the `attempt`-th claim of a job:
/// 15 s doubled per attempt (`15 s * 2^(attempt - 1)`), capped at six hours.
///
/// Deliberately deterministic — no jitter — so tests can assert exact
/// values. Attempts below 1 (which cannot occur after a claim) get the base
/// delay; large attempts saturate at the cap without overflowing.
#[must_use]
pub fn backoff(attempt: i32) -> Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1)).unwrap_or(0);
    let factor = 2_u64.checked_pow(exponent).unwrap_or(u64::MAX);
    let secs = BACKOFF_BASE_SECS
        .saturating_mul(factor)
        .min(BACKOFF_CAP_SECS);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::test_support::{ok_handler, retry_handler};

    #[test]
    fn backoff_doubles_per_attempt() {
        assert_eq!(backoff(1), Duration::from_secs(15), "first retry");
        assert_eq!(backoff(2), Duration::from_secs(30), "second retry");
        assert_eq!(backoff(3), Duration::from_secs(60), "third retry");
        assert_eq!(backoff(4), Duration::from_secs(120), "fourth retry");
        assert_eq!(
            backoff(11),
            Duration::from_mins(256),
            "last step under the cap"
        );
    }

    #[test]
    fn backoff_caps_at_six_hours() {
        assert_eq!(backoff(12), Duration::from_hours(6), "first capped attempt");
        assert_eq!(backoff(50), Duration::from_hours(6), "far beyond the cap");
        assert_eq!(
            backoff(i32::MAX),
            Duration::from_hours(6),
            "huge exponents saturate at the cap instead of overflowing"
        );
    }

    #[test]
    fn backoff_below_the_first_attempt_is_the_base_delay() {
        assert_eq!(backoff(0), Duration::from_secs(15), "attempt zero");
        assert_eq!(
            backoff(i32::MIN),
            Duration::from_secs(15),
            "negative attempts"
        );
    }

    #[test]
    fn registry_maps_kinds_to_handlers() {
        let mut registry = JobRegistry::new();
        assert!(
            registry.get("greet").is_none(),
            "unknown kinds resolve to nothing"
        );
        registry.register("greet", ok_handler);
        assert!(registry.get("greet").is_some(), "registered kinds resolve");
        registry.register("greet", retry_handler);
        assert!(
            registry.get("greet").is_some(),
            "re-registering a kind keeps it bound"
        );
    }
}
