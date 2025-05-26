use std::sync::atomic::{AtomicU64, Ordering};

/// Initial compression ratio in bps (1 / 10 000 = 0.01%).
///
/// 0.68 is chosen as an arbitrary starting point.
const INITIAL_RATIO_BPS: u64 = 6_800;

/// Retention factor for exponential weighted moving average (EWMA).
/// Each new sample keeps `1 − DECAY` of the weight.
const DECAY: f64 = 0.99;
const ALPHA: f64 = 1.0 - DECAY;

/// Trailing compression ratio, stored in bps (1 / 10 000 = 0.01%).
static ESTIMATE_BPS: AtomicU64 = AtomicU64::new(INITIAL_RATIO_BPS);

/// Zlib compression-ratio estimator backed by an EWMA.
/// Thread-safe, O(1) memory and update time.
#[derive(Debug, Clone, Copy)]
pub struct ZlibCompressionEstimate;

impl ZlibCompressionEstimate {
    /// Return the estimated compressed size in bytes for a given uncompressed size.
    pub fn get(uncompressed: usize) -> usize {
        let bps = ESTIMATE_BPS.load(Ordering::Relaxed) as f64;
        (uncompressed as f64 * (bps / 10_000.0)).round() as usize
    }

    /// Feed a new `(uncompressed_size, compressed_size)` sample.
    /// Samples with `uncompressed_size == 0` are ignored.
    /// The first real sample overrides the initial; subsequent ones use EWMA.
    pub fn update(uncompressed_size: usize, compressed_size: usize) {
        if uncompressed_size == 0 {
            return;
        }

        // Compute new ratio in bps (floating point for EWMA, integer for first sample).
        let new_bps_f = (compressed_size as f64 / uncompressed_size as f64) * 10_000.0;
        let new_bps = new_bps_f.round() as u64;

        let current = ESTIMATE_BPS.load(Ordering::Acquire);

        if current == INITIAL_RATIO_BPS {
            // First real sample overrides the initial guess.
            ESTIMATE_BPS.store(new_bps, Ordering::Release);
        } else {
            // Subsequent samples: atomically apply EWMA.
            let _ = ESTIMATE_BPS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                let updated_f = DECAY.mul_add(old as f64, ALPHA * new_bps_f);
                Some(updated_f.round() as u64)
            });
        }
    }

    /// Current average compression ratio as a float in [0, 1].
    pub fn current_average() -> f64 {
        ESTIMATE_BPS.load(Ordering::Relaxed) as f64 / 10_000.0
    }

    /// Reset to the initial 68% estimate.
    pub fn clear() {
        ESTIMATE_BPS.store(INITIAL_RATIO_BPS, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn test_all_serial() {
        initial_value();
        single_update();
        skip_zero_samples();
        first_sample_overrides_initial();
        ewma_second_update();
        estimate_calculation_rounds();
        large_input();
        weighted_average_favors_recent();
        rapid_updates_stable();
    }

    fn initial_value() {
        ZlibCompressionEstimate::clear();
        assert_relative_eq!(
            ZlibCompressionEstimate::current_average(),
            INITIAL_RATIO_BPS as f64 / 10_000.0,
            epsilon = 1e-4
        );
    }

    fn single_update() {
        ZlibCompressionEstimate::clear();
        ZlibCompressionEstimate::update(1_000, 500); // 50%
        assert_relative_eq!(ZlibCompressionEstimate::current_average(), 0.5, epsilon = 1e-4);
    }

    fn skip_zero_samples() {
        ZlibCompressionEstimate::clear();
        ZlibCompressionEstimate::update(0, 0);
        ZlibCompressionEstimate::update(0, 1_234);
        // Estimate remains the initial value
        assert_relative_eq!(
            ZlibCompressionEstimate::current_average(),
            INITIAL_RATIO_BPS as f64 / 10_000.0,
            epsilon = 1e-4
        );
    }

    fn first_sample_overrides_initial() {
        ZlibCompressionEstimate::clear();
        ZlibCompressionEstimate::update(1_000, 250); // 25%
        assert_relative_eq!(ZlibCompressionEstimate::current_average(), 0.25, epsilon = 1e-4);
    }

    fn ewma_second_update() {
        ZlibCompressionEstimate::clear();
        ZlibCompressionEstimate::update(1_000, 250); // first sample
        ZlibCompressionEstimate::update(1_000, 500); // second
        let avg = ZlibCompressionEstimate::current_average();
        // 0.99*0.25 + 0.01*0.50 = 0.2525
        assert_relative_eq!(avg, 0.2525, epsilon = 1e-4);
    }

    fn estimate_calculation_rounds() {
        ZlibCompressionEstimate::clear();
        ZlibCompressionEstimate::update(1_000, 250);
        assert_eq!(ZlibCompressionEstimate::get(2_000), 500);

        ZlibCompressionEstimate::update(1_000, 500);
        // Now average 0.2525, so get(1_000) = 253 after rounding
        assert_eq!(ZlibCompressionEstimate::get(1_000), 253);
    }

    fn large_input() {
        ZlibCompressionEstimate::clear();
        ZlibCompressionEstimate::update(usize::MAX, usize::MAX / 2);
        let avg = ZlibCompressionEstimate::current_average();
        assert!(avg.is_finite() && avg > 0.0 && avg < 1.0);
    }

    fn weighted_average_favors_recent() {
        ZlibCompressionEstimate::clear();
        for _ in 0..10 {
            ZlibCompressionEstimate::update(1_000, 800);
        }
        ZlibCompressionEstimate::update(1_000, 500);
        let avg = ZlibCompressionEstimate::current_average();
        assert!(0.5 < avg && avg < 0.8);
    }

    fn rapid_updates_stable() {
        ZlibCompressionEstimate::clear();
        for i in 0..100 {
            let ratio = if i % 2 == 0 { 0.8 } else { 0.2 };
            ZlibCompressionEstimate::update(1_000, (1_000.0 * ratio) as usize);
        }
        let avg = ZlibCompressionEstimate::current_average();
        assert!(avg.is_finite() && (0.2..0.8).contains(&avg));
    }
}
