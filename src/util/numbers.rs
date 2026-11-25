//! Count conversions into the `integer` column's width.

/// Narrows a count to the `integer` column's width, saturating at both ends.
///
/// A count past `i32::MAX` becomes `i32::MAX`, and a negative one becomes
/// `0`: none of the counts this serves (votes, voters) can be negative, so
/// a nonsense negative from a peer reads as none rather than as a wrapped
/// or refused value.
#[must_use]
pub fn saturating_i32(value: i64) -> i32 {
    i32::try_from(value.max(0)).unwrap_or(i32::MAX)
}

/// Narrows a length or index to the `integer` column's width.
///
/// The same saturation as [`saturating_i32`]: past `i32::MAX` it is
/// `i32::MAX`. A `usize` is never negative.
#[must_use]
pub fn saturating_i32_from_usize(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A count inside the range passes through unchanged.
    #[test]
    fn a_count_in_range_is_unchanged() {
        assert_eq!(saturating_i32(0), 0);
        assert_eq!(saturating_i32(42), 42);
        assert_eq!(saturating_i32(i64::from(i32::MAX)), i32::MAX);
    }

    /// A count past `i32::MAX` clamps to it.
    #[test]
    fn a_count_above_the_maximum_clamps() {
        assert_eq!(saturating_i32(i64::from(i32::MAX) + 1), i32::MAX);
        assert_eq!(saturating_i32(i64::MAX), i32::MAX);
    }

    /// A negative count clamps to zero.
    #[test]
    fn a_negative_count_clamps_to_zero() {
        assert_eq!(saturating_i32(-1), 0);
        assert_eq!(saturating_i32(i64::MIN), 0);
    }

    /// A length converts in range and clamps above it.
    #[test]
    fn a_length_converts_and_clamps() {
        assert_eq!(saturating_i32_from_usize(0), 0);
        assert_eq!(saturating_i32_from_usize(7), 7);
        assert_eq!(saturating_i32_from_usize(usize::MAX), i32::MAX);
    }
}
