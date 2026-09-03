//! Saturating conversions for metrics and deadline accounting.

/// Convert a duration to whole milliseconds without narrowing wraparound.
pub fn duration_millis_saturating(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_milliseconds_saturate_at_the_wire_counter_width() {
        assert_eq!(
            duration_millis_saturating(std::time::Duration::from_micros(1_999)),
            1
        );
        assert_eq!(
            duration_millis_saturating(std::time::Duration::MAX),
            u64::MAX
        );
    }
}
