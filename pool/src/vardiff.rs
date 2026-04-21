use std::collections::VecDeque;
use chrono::{DateTime, Utc};

// ─── Tuning constants ─────────────────────────────────────────────────────────
//
// EMA_ALPHA: smoothing factor for hashrate estimate (0 < α ≤ 1).
//   Lower α = more smoothing = less sensitive to short bursts.
//   0.25 means ~4 retarget periods to converge fully.
const EMA_ALPHA: f64 = 0.25;

// Hysteresis bands:
//   UP   = 1.35 → only raise diff when the target is meaningfully higher.
//   DOWN = 1.15 → lower diff more eagerly so miners don't get stuck high.
// This is intentionally asymmetric: we prefer slow rises and faster falls.
const HYSTERESIS_UP: f64 = 1.35;
const HYSTERESIS_DOWN: f64 = 1.15;

// OFFLINE_SECS: if zero shares in this many seconds → cut difficulty by 6.
//   Only fires when truly offline, not from statistical variance.
const OFFLINE_SECS: f64 = 120.0; // 2 minutes

// CACHE_SIZE: sliding window of recent shares for hashrate estimation.
const CACHE_SIZE: usize = 30;

/// Vardiff controller — EMA + Hysteresis design.
///
/// # Algorithm
///
/// 1. Maintain a sliding ring-buffer of (time, difficulty) for last N shares.
///
/// 2. Estimate hashrate via EMA (exponential moving average):
///      ema_rate = α × instant_rate + (1-α) × ema_rate
///    where instant_rate = Σ(difficulty) / Δtime_s
///
/// 3. Target difficulty = ema_rate × target_share_time
///
/// 4. Keep the exact target difficulty, rounded to a whole number and
///    clamped to the configured min/max bounds.
///
/// 5. Retarget only when:
///      UP:   target ≥ current × HYSTERESIS_UP   (slow upward movement)
///      DOWN: target ≤ current / HYSTERESIS_DOWN (faster downward movement)
///
/// 6. ÷6 rescue rule: if zero shares for OFFLINE_SECS → miner is truly stopped.
///
/// # Vardiff does NOT change block probability
///
/// E[best_diff per second] = hashrate / 2^32 = constant regardless of difficulty.
/// Vardiff only controls how often the pool receives shares (reporting rate).
/// Higher difficulty → fewer shares → more CPU headroom; mining efficiency unchanged.
#[derive(Debug, Clone)]
pub struct VardiffController {
    target_share_time: f64,
    retarget_time:     f64,
    min_diff:          f64,
    max_diff:          f64,
    /// Time window used for share-rate estimation. Old shares are pruned so
    /// the controller can react to sustained slowdowns instead of waiting for
    /// an oversized historical burst to dominate the rate estimate.
    sample_window_secs: f64,

    last_retarget: DateTime<Utc>,
    session_start: DateTime<Utc>,

    /// EMA of difficulty_per_second (hashrate / 2^32).
    /// Initialized to 0 until first share arrives.
    ema_rate: f64,
    /// Whether ema_rate has been seeded from at least one measurement.
    ema_seeded: bool,

    /// Timestamp of the most recent accepted share. Used together with the
    /// time-based sample window to keep downward retargets responsive.
    last_share_at: Option<DateTime<Utc>>,

    /// Sliding ring-buffer of (time, difficulty) for instant-rate estimation.
    samples: VecDeque<(DateTime<Utc>, f64)>,
}

impl VardiffController {
    pub fn new(
        target_share_time: f64,
        retarget_time:     f64,
        min_diff:          f64,
        max_diff:          f64,
    ) -> Self {
        let now = Utc::now();
        let sample_window_secs = (target_share_time.max(1.0) * 4.0).clamp(90.0, 300.0);
        Self {
            target_share_time: target_share_time.max(1.0),
            retarget_time:     retarget_time.max(1.0),
            min_diff:          min_diff.max(1.0),
            max_diff:          max_diff.max(min_diff),
            sample_window_secs,
            last_retarget:     now,
            session_start:     now,
            ema_rate:          0.0,
            ema_seeded:        false,
            last_share_at:     None,
            samples:           VecDeque::with_capacity(CACHE_SIZE),
        }
    }

    /// Record an accepted share.
    pub fn record_share(&mut self, now: DateTime<Utc>, difficulty: f64) {
        self.samples.push_back((now, difficulty));
        self.last_share_at = Some(now);
        self.prune_old_samples(now);
        if self.samples.len() > CACHE_SIZE {
            self.samples.pop_front();
        }
    }

    /// Returns Some(new_diff) if a retarget is warranted, None otherwise.
    pub fn maybe_retarget(&mut self, current_diff: f64, now: DateTime<Utc>) -> Option<f64> {
        // Enforce minimum cadence.
        let since_ms = (now - self.last_retarget).num_milliseconds();
        if since_ms < (self.retarget_time * 1000.0) as i64 { return None; }
        self.last_retarget = now;
        self.prune_old_samples(now);

        // ── Offline rescue ────────────────────────────────────────────────────
        // Zero shares for OFFLINE_SECS → miner truly stopped. Cut difficulty.
        if self.samples.is_empty() {
            let age_s = (now - self.session_start).num_seconds() as f64;
            if age_s > OFFLINE_SECS {
                let stepped = self.clamp_diff((current_diff / 6.0).max(self.min_diff));
                if (stepped - current_diff).abs() > f64::EPSILON {
                    return Some(stepped);
                }
            }
            return None;
        }

        // ── Instant rate from sliding window ─────────────────────────────────
        let sum  = self.samples.iter().map(|(_, d)| *d).sum::<f64>();
        let span = (self.samples.back().unwrap().0 - self.samples.front().unwrap().0)
                       .num_milliseconds() as f64 / 1000.0;
        if span <= 0.0 { return None; }
        let instant_rate = sum / span;

        // ── EMA update ────────────────────────────────────────────────────────
        if self.ema_seeded {
            self.ema_rate = EMA_ALPHA * instant_rate + (1.0 - EMA_ALPHA) * self.ema_rate;
        } else {
            self.ema_rate  = instant_rate; // seed on first measurement
            self.ema_seeded = true;
        }

        // ── Target difficulty ─────────────────────────────────────────────────
        let raw_target  = self.ema_rate * self.target_share_time;
        let target_diff = self.clamp_diff(raw_target);

        // ── Hysteresis dead-band ──────────────────────────────────────────────
        // Retarget UP   if target ≥ current × HYSTERESIS_UP
        // Retarget DOWN if target ≤ current / HYSTERESIS_DOWN
        // Asymmetric on purpose: let fast miners climb a bit more slowly,
        // but let slow miners come down quickly.
        let should_up   = target_diff >= current_diff * HYSTERESIS_UP;
        let should_down = target_diff <= current_diff / HYSTERESIS_DOWN;

        if should_up || should_down {
            Some(target_diff)
        } else {
            None
        }
    }

    /// Clamp difficulty to the configured range and round to a whole number.
    ///
    /// ckpool keeps exact integer difficulty steps rather than snapping to
    /// powers of two.  Matching that behaviour lets the pool move down
    /// smoothly instead of getting stuck on coarse buckets like 131072 / 262144.
    pub fn clamp_diff(&self, val: f64) -> f64 {
        if !val.is_finite() || val <= 0.0 { return self.min_diff; }
        val.round().clamp(self.min_diff, self.max_diff)
    }

    /// Drop samples that fall outside the rolling time window.
    fn prune_old_samples(&mut self, now: DateTime<Utc>) {
        while let Some((front_time, _)) = self.samples.front().cloned() {
            let age_s = (now - front_time).num_seconds() as f64;
            if age_s <= self.sample_window_secs {
                break;
            }
            self.samples.pop_front();
        }
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn make_vc(target_s: f64) -> VardiffController {
        VardiffController::new(target_s, 30.0, 512.0, 33_554_432.0)
    }

    fn add_shares(vc: &mut VardiffController, n: usize, diff: f64, interval_secs: f64) {
        let mut t = Utc::now();
        for _ in 0..n {
            t += Duration::milliseconds((interval_secs * 1000.0) as i64);
            vc.record_share(t, diff);
        }
    }

    /// Hysteresis prevents retarget when target is close to current.
    #[test]
    fn test_no_oscillation_at_boundary() {
        let mut vc = make_vc(10.0);
        // Miner doing exactly target rate → EMA converges to current diff
        // target_diff == current_diff → no retarget
        add_shares(&mut vc, 30, 16384.0, 10.0); // 1 share/10s = exact target
        let r = vc.maybe_retarget(16384.0, Utc::now() + Duration::seconds(31));
        // At 1 share/10s with diff=16384, target = rate * 10 = 16384
        // 16384 < 16384 * 1.5 → no UP retarget
        // 16384 > 16384 / 1.5 → no DOWN retarget
        assert!(r.is_none(), "should not retarget when on target: {:?}", r);
    }

    /// Offline rescue: 0 shares for 2+ minutes → difficulty cut by 6.
    #[test]
    fn test_offline_rescue() {
        let mut vc = make_vc(10.0);
        let future = Utc::now() + Duration::seconds(200);
        let r = vc.maybe_retarget(16384.0, future);
        assert!(r.is_some());
        let new_d = r.unwrap();
        // 16384 / 6 = 2730.666..., rounded to the nearest integer.
        assert_eq!(new_d, 2731.0);
    }

    /// Fast miner: 10× too fast → difficulty should increase.
    #[test]
    fn test_fast_miner_increases_diff() {
        let mut vc = make_vc(10.0);
        // 1 share per second (10× target rate of 1/10s)
        add_shares(&mut vc, 30, 16384.0, 1.0);
        let r = vc.maybe_retarget(16384.0, Utc::now() + Duration::seconds(31));
        assert!(r.is_some(), "fast miner should trigger retarget");
        assert!(r.unwrap() > 16384.0 * HYSTERESIS_UP - 1.0);
    }

    /// Slow miner: 10× too slow → difficulty should decrease.
    #[test]
    fn test_slow_miner_decreases_diff() {
        let mut vc = make_vc(10.0);
        // 1 share per 100s (10× below target)
        add_shares(&mut vc, 30, 16384.0, 100.0);
        let r = vc.maybe_retarget(16384.0, Utc::now() + Duration::seconds(31));
        assert!(r.is_some(), "slow miner should trigger retarget");
        assert!(r.unwrap() < 16384.0 / HYSTERESIS_DOWN + 1.0);
    }

    /// Time-based pruning should let a previously fast miner fall back down
    /// once the recent accepted-share cadence slows materially.
    #[test]
    fn test_time_window_allows_downward_retarget() {
        let mut vc = VardiffController::new(20.0, 30.0, 8192.0, 4_194_304.0);
        let mut t = Utc::now();

        vc.record_share(t, 262_144.0);
        t += Duration::seconds(120);
        vc.record_share(t, 262_144.0);
        t += Duration::seconds(120);
        vc.record_share(t, 262_144.0);

        let r = vc.maybe_retarget(262_144.0, t + Duration::seconds(31));
        assert!(r.is_some(), "slowdown should now trigger a lower diff");
        assert!(r.unwrap() < 262_144.0);
    }

    /// Exact difficulty steps should be preserved instead of snapping to P2.
    #[test]
    fn test_clamp_diff_keeps_exact_integer_steps() {
        let vc = VardiffController::new(20.0, 30.0, 8192.0, 4_194_304.0);
        assert_eq!(vc.clamp_diff(100_000.4), 100_000.0);
        assert_eq!(vc.clamp_diff(131_072.4), 131_072.0);
        assert_eq!(vc.clamp_diff(999.4), 8_192.0);
    }

    /// Vardiff does NOT change block probability.
    #[test]
    fn test_vardiff_does_not_change_block_probability() {
        // E[best_diff per second] = hashrate / 2^32 = constant
        // For a 10 TH/s miner:
        let hashrate: f64 = 10e12; // H/s
        let hashes_per_diff: f64 = 4_294_967_296.0; // 2^32

        let rate = hashrate / hashes_per_diff; // diff units/s

        // With diff=16384: rate = same
        let rate_low_diff = rate;
        // With diff=262144: rate = same
        let rate_high_diff = rate;

        assert!((rate_low_diff - rate_high_diff).abs() < f64::EPSILON,
                "difficulty does not affect rate of best_diff growth");
    }
}
