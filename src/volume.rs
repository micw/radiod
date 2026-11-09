//! Hardware-volume mapping compatible with MPD's normalized ALSA scale.
//! Values are centi-dB (ALSA calls these millibels), never software gain.

// ALSA control.h: SND_CTL_TLV_DB_GAIN_MUTE, not a finite attenuation.
pub(crate) const DB_GAIN_MUTE: i64 = -9_999_999;

/// Invert the normalized scale. None means unusable metadata: use raw mapping.
/// Clamp before logarithms so zero/mute never produces negative infinity.
pub(crate) fn normalized_db(percent: u8, min: i64, max: i64) -> Option<i64> {
    if percent > 100 || min >= max || min < DB_GAIN_MUTE || max <= DB_GAIN_MUTE {
        return None;
    }
    if percent == 0 {
        return Some(min);
    }
    if percent == 100 {
        return Some(max);
    }
    let n = f64::from(percent) / 100.0;
    let span = max as f64 - min as f64;
    let target = if min != DB_GAIN_MUTE && span <= 2400.0 {
        min as f64 + span * n
    } else {
        let a = if min == DB_GAIN_MUTE {
            0.0
        } else {
            10.0_f64.powf(-span / 6000.0)
        };
        max as f64 + 6000.0 * (a + (1.0 - a) * n).log10()
    };
    // Round upward, matching the direction requested from ALSA. Float-to-int
    // conversion saturates; integer clamping also protects extreme metadata.
    Some((target.ceil() as i64).clamp(min, max))
}

/// Linear fallback for controls without usable dB metadata. i128 arithmetic
/// avoids overflow and floating-point excursions beyond the raw maximum.
pub(crate) fn raw_volume(percent: u8, min: i64, max: i64) -> Option<i64> {
    if percent > 100 || min > max {
        return None;
    }
    let span = i128::from(max) - i128::from(min);
    Some((i128::from(min) + (span * i128::from(percent) + 50) / 100) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_spans_are_linear_db_including_24_db_boundary() {
        assert_eq!(normalized_db(50, -2400, 0), Some(-1200));
        assert_eq!(normalized_db(25, -1200, 600), Some(-750));
        assert_eq!(normalized_db(50, -2401, 0), Some(-933));
    }

    #[test]
    fn wide_span_accounts_for_finite_minimum() {
        let a = 10.0_f64.powf(-1.0);
        let expected = (6000.0 * (a + (1.0 - a) * 0.5).log10()).ceil() as i64;
        assert_eq!(normalized_db(50, -6000, 0), Some(expected));
        assert_eq!(normalized_db(50, DB_GAIN_MUTE, 0), Some(-1806));
        assert_ne!(
            normalized_db(50, -6000, 0),
            normalized_db(50, DB_GAIN_MUTE, 0)
        );
    }

    #[test]
    fn all_percentages_are_bounded_monotonic_with_exact_endpoints() {
        for (min, max) in [
            (-6000, 0),
            (-2400, 0),
            (-2401, 0),
            (-1, 0),
            (DB_GAIN_MUTE, 0),
            (DB_GAIN_MUTE, 2400),
            (-9600, 1200),
            (0, i64::MAX),
            (DB_GAIN_MUTE, i64::MAX),
        ] {
            assert_eq!(normalized_db(0, min, max), Some(min));
            assert_eq!(normalized_db(100, min, max), Some(max));
            let mut previous = min;
            for percent in 0..=100 {
                let value = normalized_db(percent, min, max).unwrap();
                assert!((min..=max).contains(&value));
                assert!(value >= previous, "{min}..{max} at {percent}");
                previous = value;
            }
        }
    }

    #[test]
    fn invalid_metadata_does_not_fabricate_db_support() {
        for (min, max) in [
            (0, 0),
            (1, 0),
            (DB_GAIN_MUTE - 1, 0),
            (DB_GAIN_MUTE, DB_GAIN_MUTE),
            (i64::MIN, i64::MAX),
        ] {
            assert_eq!(normalized_db(50, min, max), None);
        }
        assert_eq!(normalized_db(101, -6000, 0), None);
    }

    #[test]
    fn raw_fallback_is_bounded_monotonic_even_at_integer_limits() {
        for (min, max) in [(0, 255), (10, 20), (-10, 10), (7, 7), (i64::MIN, i64::MAX)] {
            assert_eq!(raw_volume(0, min, max), Some(min));
            assert_eq!(raw_volume(100, min, max), Some(max));
            let mut previous = min;
            for percent in 0..=100 {
                let value = raw_volume(percent, min, max).unwrap();
                assert!((min..=max).contains(&value));
                assert!(value >= previous);
                previous = value;
            }
        }
        assert_eq!(raw_volume(101, 0, 10), None);
        assert_eq!(raw_volume(50, 10, 0), None);
        assert_eq!(raw_volume(50, 0, 255), Some(128));
    }
}
