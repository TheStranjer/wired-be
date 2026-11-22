//! The time source the limiter ages its windows against: the real clock
//! and the one tests move by hand.

use std::{
    fmt,
    time::{Duration, Instant},
};

/// Where the limiter reads the time: the seam that lets tests move the
/// clock instead of sleeping.
pub trait Clock: fmt::Debug + Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The production [`Clock`]: the monotonic `Instant::now`.
#[derive(Clone, Copy, Debug)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A [`Clock`] that stands still until a test advances it by hand.
#[derive(Debug)]
pub struct ManualClock {
    /// The instant the clock started at.
    start: Instant,
    /// Nanoseconds it has been advanced by.
    advanced: std::sync::atomic::AtomicU64,
}

impl ManualClock {
    /// A clock standing at the real [`Instant::now`], advanced by nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            advanced: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Moves the clock `by` into its future.
    pub fn advance(&self, by: Duration) {
        let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        let _total = self
            .advanced
            .fetch_add(nanos, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        let nanos = self.advanced.load(std::sync::atomic::Ordering::Relaxed);
        self.start + Duration::from_nanos(nanos)
    }
}
