use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use tokio::sync::RwLock;

const HASHES_PER_DIFF: f64 = 4_294_967_296.0;
const MAX_EVENTS: usize = 2000;
const SHARE_CACHE_SIZE: usize = 40;

/// Seconds of inactivity after which a miner is hidden from the API/dashboard.
/// The miner's record is kept in memory (for best_difficulty persistence) but
/// excluded from /miners responses until the miner resubmits a share.
/// On reconnect after this window the session stats are reset to zero.
const MINER_INACTIVE_SECS: i64 = 300; // 5 minutes

#[derive(Debug, Clone, Serialize)]
pub struct MinerStats {
    pub worker: String,
    pub difficulty: f64,
    pub best_difficulty: f64,
    pub best_submitted_difficulty: f64,
    pub shares: u64,
    pub rejected: u64,
    pub stale: u64,
    pub hashrate_gh: f64,
    pub last_seen: DateTime<Utc>,
    /// Time from last `mining.notify` to `mining.submit` arriving (ms, EMA).
    /// Dominated by hashing time at the current difficulty — NOT network RTT.
    /// At TARGET_SHARE_TIME_SECS=10 this will naturally read ~10,000 ms.
    /// Renamed from `latency_ms_avg` to prevent confusion with TCP ping.
    pub notify_to_submit_ms: f64,
    /// Actual server-side processing time per submit (ms, EMA).
    /// Measures pool overhead only: parse → validate → respond.
    /// Typical healthy value: 1–5 ms. Spike > 50 ms suggests CPU pressure.
    pub submit_rtt_ms: f64,
    pub last_share_time: Option<DateTime<Utc>>,
    pub user_agent: Option<String>,
    pub session_id: Option<String>,
    /// Wall-clock time when the miner's current session started (TCP connect).
    pub session_start: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShareEvent {
    pub worker: String,
    pub difficulty: f64,
    pub accepted: bool,
    pub is_block: bool,
    pub created_at: DateTime<Utc>,
    /// Age of the GBT job template when this share was submitted (seconds).
    /// High values (> 30 s) indicate ZMQ latency or miner replaying stale work.
    pub job_age_secs: u64,
    /// Milliseconds between the last `mining.notify` and this share arriving.
    /// This is an upper bound on miner round-trip latency.
    pub notify_delay_ms: u64,
    /// True when the session was younger than 30 s at submit time.
    /// Indicates the miner recently reconnected (replay of pre-reconnect shares).
    pub reconnect_recent: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub miners: Vec<MinerStats>,
    pub total_hashrate_gh: f64,
    pub total_shares: u64,
    pub total_rejected: u64,
    pub total_blocks: u64,
    pub updated_at: DateTime<Utc>,
}

/// Reason a share was counted as stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// Job ID not found: new block arrived and clean_jobs cleared the table.
    /// Indicated by a clean_jobs=true notify sent within the last 60 s.
    NewBlock,
    /// Job ID not found: job just expired / unknown (no clean_jobs signal seen).
    Expired,
    /// Job submitted on a session younger than 30 s.
    /// Miner likely reconnected and replayed a share from the previous session.
    Reconnect,
}

/// Pool-wide counters for job-storm detection and submitblock diagnostics.
/// All fields are lock-free atomics — safe to read from any thread without
/// taking the metrics write-lock.
#[derive(Default)]
pub struct PoolCounters {
    /// Total mining.notify messages sent to all miners.
    pub jobs_sent:              AtomicU64,
    /// mining.notify with clean_jobs=true (new block detected).
    pub clean_jobs_sent:        AtomicU64,

    // ── Notify suppression counters (three distinct reasons) ──────────────
    /// Suppressed because a different source already sent the SAME template_key
    /// (content-based dedup: ZMQ_TX + timer both fire for identical content).
    pub notify_deduped:         AtomicU64,
    /// Suppressed by per-session token bucket (rate > 1 notify/500ms, clean_jobs=false).
    pub notify_rate_limited:    AtomicU64,
    /// Suppressed by post-block TX window (ZMQ hashtx fired within 15s of hashblock).
    /// Counter already exists as zmq_tx_suppressed — aliased here for API symmetry.
    // (zmq_tx_suppressed is the canonical field; this is not a separate counter)

    /// Duplicate share submissions rejected (same nonce+ntime+en2).
    pub duplicate_shares:       AtomicU64,
    /// Total miner reconnections since pool start.
    pub reconnects_total:       AtomicU64,
    /// submitblock calls accepted by Bitcoin Core (null result = success).
    pub submitblock_accepted:   AtomicU64,
    /// submitblock calls that returned an error string from Bitcoin Core.
    pub submitblock_rejected:   AtomicU64,
    /// submitblock RPC calls that failed entirely (network/timeout).
    pub submitblock_rpc_fail:   AtomicU64,
    /// Shares rejected because version bits outside the negotiated BIP310 mask
    /// were modified by the miner — indicates broken firmware.
    pub version_rolling_violations: AtomicU64,
    /// Stales caused by a new block clearing the job table.
    pub stales_new_block:       AtomicU64,
    /// Stales where the job was simply expired / unknown.
    pub stales_expired:         AtomicU64,
    /// Stales on fresh sessions (< 30 s): miner reconnected and sent an old share.
    pub stales_reconnect:       AtomicU64,
    /// ZMQ TX notifications suppressed by ZMQ_DEBOUNCE_MS (normal inter-tx debounce).
    /// High values are expected and healthy — Bitcoin mempool sends hundreds of
    /// hashtx/s; the debounce collapses them to at most 1 GBT refresh per 10s.
    pub zmq_tx_debounced:            AtomicU64,
    /// ZMQ TX notifications suppressed by the post-block 15s window.
    /// Fires only in the 15s after a hashblock ZMQ; separating it from
    /// zmq_tx_debounced lets operators confirm: burst IS correlated with a block.
    pub zmq_tx_post_block_suppressed: AtomicU64,
    /// ZMQ TX notifications that triggered a GBT refresh.
    pub zmq_tx_triggered:            AtomicU64,
    /// ZMQ block notifications received (raw count across ALL endpoints).
    /// With dual ZMQ (hashblock:28334 + rawblock:28332), this is typically
    /// 2× the number of actual blocks (each block fires one notification per port).
    pub zmq_block_received:          AtomicU64,
    /// Unique blocks actually detected via ZMQ (after 10ms debounce).
    /// This equals the real number of new blocks seen via ZMQ — always 1 per block,
    /// regardless of how many ZMQ ports fired.
    pub zmq_blocks_detected:         AtomicU64,
    /// Set to true once the Stratum TCP listener successfully binds and is ready
    /// to accept connections. Used by the /blackhole/connection-status endpoint
    /// instead of a hardcoded `true`.
    pub stratum_ready:               std::sync::atomic::AtomicBool,
}

impl PoolCounters {
    pub fn jobs_sent(&self)              -> u64 { self.jobs_sent.load(Ordering::Relaxed) }
    pub fn clean_jobs_sent(&self)        -> u64 { self.clean_jobs_sent.load(Ordering::Relaxed) }
    pub fn notify_deduped(&self)         -> u64 { self.notify_deduped.load(Ordering::Relaxed) }
    pub fn notify_rate_limited(&self)    -> u64 { self.notify_rate_limited.load(Ordering::Relaxed) }
    pub fn duplicate_shares(&self)       -> u64 { self.duplicate_shares.load(Ordering::Relaxed) }
    pub fn reconnects_total(&self)       -> u64 { self.reconnects_total.load(Ordering::Relaxed) }
    pub fn submitblock_accepted(&self)   -> u64 { self.submitblock_accepted.load(Ordering::Relaxed) }
    pub fn submitblock_rejected(&self)   -> u64 { self.submitblock_rejected.load(Ordering::Relaxed) }
    pub fn submitblock_rpc_fail(&self)   -> u64 { self.submitblock_rpc_fail.load(Ordering::Relaxed) }
    pub fn version_rolling_violations(&self) -> u64 { self.version_rolling_violations.load(Ordering::Relaxed) }
    pub fn stales_new_block(&self)       -> u64 { self.stales_new_block.load(Ordering::Relaxed) }
    pub fn stales_expired(&self)         -> u64 { self.stales_expired.load(Ordering::Relaxed) }
    pub fn stales_reconnect(&self)       -> u64 { self.stales_reconnect.load(Ordering::Relaxed) }
    pub fn zmq_tx_debounced(&self)            -> u64 { self.zmq_tx_debounced.load(Ordering::Relaxed) }
    pub fn zmq_tx_post_block_suppressed(&self) -> u64 { self.zmq_tx_post_block_suppressed.load(Ordering::Relaxed) }
    pub fn zmq_tx_triggered(&self)            -> u64 { self.zmq_tx_triggered.load(Ordering::Relaxed) }
    pub fn zmq_block_received(&self)          -> u64 { self.zmq_block_received.load(Ordering::Relaxed) }
    pub fn zmq_blocks_detected(&self)         -> u64 { self.zmq_blocks_detected.load(Ordering::Relaxed) }

    pub fn inc_jobs_sent(&self, clean: bool) {
        self.jobs_sent.fetch_add(1, Ordering::Relaxed);
        if clean { self.clean_jobs_sent.fetch_add(1, Ordering::Relaxed); }
    }
    pub fn inc_notify_deduped(&self)      { self.notify_deduped.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_notify_rate_limited(&self) { self.notify_rate_limited.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_duplicate_share(&self) { self.duplicate_shares.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_reconnect(&self)       { self.reconnects_total.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_submitblock_accepted(&self) { self.submitblock_accepted.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_submitblock_rejected(&self) { self.submitblock_rejected.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_submitblock_rpc_fail(&self) { self.submitblock_rpc_fail.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_version_rolling_violation(&self) { self.version_rolling_violations.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_stale(&self, reason: StaleReason) {
        match reason {
            StaleReason::NewBlock  => self.stales_new_block.fetch_add(1, Ordering::Relaxed),
            StaleReason::Expired   => self.stales_expired.fetch_add(1, Ordering::Relaxed),
            StaleReason::Reconnect => self.stales_reconnect.fetch_add(1, Ordering::Relaxed),
        };
    }
    pub fn set_stratum_ready(&self) { self.stratum_ready.store(true, Ordering::Relaxed); }
    pub fn is_stratum_ready(&self) -> bool { self.stratum_ready.load(Ordering::Relaxed) }
    pub fn inc_zmq_tx_debounced(&self)            { self.zmq_tx_debounced.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_zmq_tx_post_block_suppressed(&self) { self.zmq_tx_post_block_suppressed.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_zmq_tx_triggered(&self)            { self.zmq_tx_triggered.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_zmq_block_received(&self)          { self.zmq_block_received.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_zmq_blocks_detected(&self)         { self.zmq_blocks_detected.fetch_add(1, Ordering::Relaxed); }
}

#[derive(Default)]
struct MetricsState {
    miners: HashMap<String, MinerStats>,
    events: Vec<ShareEvent>,
    total_blocks: u64,
    share_samples: HashMap<String, ShareWindow>,
}

#[derive(Clone)]
pub struct MetricsStore {
    inner:   Arc<RwLock<MetricsState>>,
    pub counters: Arc<PoolCounters>,
    /// UTC timestamp when the pool process started.
    /// Used by the API to compute uptime and per-minute rates.
    pub started_at: DateTime<Utc>,
}

impl MetricsStore {
    pub fn new() -> Self {
        Self {
            inner:      Arc::new(RwLock::new(MetricsState::default())),
            counters:   Arc::new(PoolCounters::default()),
            started_at: Utc::now(),
        }
    }

    /// Record a share submission. Returns `Some(new_best)` when the worker's
    /// all-time best difficulty improves — caller should persist this to SQLite.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_share(
        &self,
        worker: &str,
        target_difficulty: f64,
        share_difficulty: f64,
        accepted: bool,
        is_block: bool,
        notify_to_submit_ms: i64,
        submit_rtt_ms: f64,
        job_age_secs: u64,
        notify_delay_ms: u64,
        reconnect_recent: bool,
    ) -> Option<f64> {
        let now = Utc::now();
        let mut guard = self.inner.write().await;

        if is_block {
            guard.total_blocks += 1;
        }

        let mut new_hashrate = None;
        if accepted {
            let samples = guard
                .share_samples
                .entry(worker.to_string())
                .or_insert_with(ShareWindow::default);
            samples.push(ShareSample {
                time: now,
                difficulty: target_difficulty,
            });
            new_hashrate = samples.hashrate_gh();
        }

        let mut new_best: Option<f64> = None;

        {
            let stats = guard.miners.entry(worker.to_string()).or_insert_with(|| MinerStats {
                worker: worker.to_string(),
                difficulty: target_difficulty,
                best_difficulty: 0.0,
                best_submitted_difficulty: 0.0,
                shares: 0,
                rejected: 0,
                stale: 0,
                hashrate_gh: 0.0,
                last_seen: now,
                notify_to_submit_ms: notify_to_submit_ms as f64,
                submit_rtt_ms,
                last_share_time: None,
                user_agent: None,
                session_id: None,
                session_start: None,
            });

            stats.difficulty = target_difficulty;
            stats.last_seen = now;

            if share_difficulty > stats.best_submitted_difficulty {
                stats.best_submitted_difficulty = share_difficulty;
            }

            if accepted {
                stats.shares += 1;
                if share_difficulty > stats.best_difficulty {
                    stats.best_difficulty = share_difficulty;
                    new_best = Some(share_difficulty);
                }
            } else {
                stats.rejected += 1;
            }

            // EMA α=0.2 for both metrics — smooth over ~5 shares.
            stats.notify_to_submit_ms = (stats.notify_to_submit_ms * 0.8) + (notify_to_submit_ms as f64 * 0.2);
            stats.submit_rtt_ms       = (stats.submit_rtt_ms       * 0.8) + (submit_rtt_ms             * 0.2);

            if accepted {
                if let Some(hashrate) = new_hashrate {
                    stats.hashrate_gh = hashrate;
                }
                stats.last_share_time = Some(now);
            }
        }

        guard.events.push(ShareEvent {
            worker: worker.to_string(),
            difficulty: share_difficulty,
            accepted,
            is_block,
            created_at: now,
            job_age_secs,
            notify_delay_ms,
            reconnect_recent,
        });

        let overflow = guard.events.len().saturating_sub(MAX_EVENTS);
        if overflow > 0 {
            guard.events.drain(0..overflow);
        }

        new_best
    }

    /// Seed the in-memory best_difficulty from a persisted value (called on startup).
    /// Only raises the current value — never lowers it.
    pub async fn set_worker_best(&self, worker: &str, best_diff: f64) {
        let now = Utc::now();
        let mut guard = self.inner.write().await;
        let stats = guard.miners.entry(worker.to_string()).or_insert_with(|| MinerStats {
            worker: worker.to_string(),
            difficulty: 0.0,
            best_difficulty: 0.0,
            best_submitted_difficulty: 0.0,
            shares: 0,
            rejected: 0,
            stale: 0,
            hashrate_gh: 0.0,
            last_seen: now,
            notify_to_submit_ms: 0.0,
            submit_rtt_ms: 0.0,
            last_share_time: None,
            user_agent: None,
            session_id: None,
            session_start: None,
        });
        if best_diff > stats.best_difficulty {
            stats.best_difficulty = best_diff;
        }
        if best_diff > stats.best_submitted_difficulty {
            stats.best_submitted_difficulty = best_diff;
        }
    }

    pub async fn record_stale(&self, worker: &str, reason: StaleReason) {
        let now = Utc::now();
        self.counters.inc_stale(reason);
        let mut guard = self.inner.write().await;
        let stats = guard.miners.entry(worker.to_string()).or_insert_with(|| MinerStats {
            worker: worker.to_string(),
            difficulty: 0.0,
            best_difficulty: 0.0,
            best_submitted_difficulty: 0.0,
            shares: 0,
            rejected: 0,
            stale: 0,
            hashrate_gh: 0.0,
            last_seen: now,
            notify_to_submit_ms: 0.0,
            submit_rtt_ms: 0.0,
            last_share_time: None,
            user_agent: None,
            session_id: None,
            session_start: None,
        });
        stats.stale += 1;
        stats.last_seen = now;
    }

    pub async fn record_miner_seen(
        &self,
        worker: &str,
        difficulty: f64,
        user_agent: Option<String>,
        session_id: Option<String>,
    ) {
        let now = Utc::now();
        let cutoff = now - Duration::seconds(MINER_INACTIVE_SECS);
        let mut guard = self.inner.write().await;

        // If the miner already exists but was inactive beyond the window,
        // reset all session stats so the dashboard starts fresh.
        // best_difficulty is preserved (historical all-time best stays).
        if let Some(existing) = guard.miners.get_mut(worker) {
            if existing.last_seen < cutoff {
                // Miner was away for > MINER_INACTIVE_SECS — full session reset.
                existing.shares          = 0;
                existing.rejected        = 0;
                existing.stale           = 0;
                existing.hashrate_gh     = 0.0;
                existing.notify_to_submit_ms = 0.0;
                existing.submit_rtt_ms   = 0.0;
                existing.last_share_time = None;
                existing.session_start   = Some(now);
                // Also clear the share-rate window so hashrate starts fresh.
                guard.share_samples.remove(worker);
            }
        }

        let stats = guard.miners.entry(worker.to_string()).or_insert_with(|| MinerStats {
            worker: worker.to_string(),
            difficulty,
            best_difficulty: 0.0,
            best_submitted_difficulty: 0.0,
            shares: 0,
            rejected: 0,
            stale: 0,
            hashrate_gh: 0.0,
            last_seen: now,
            notify_to_submit_ms: 0.0,
            submit_rtt_ms: 0.0,
            last_share_time: None,
            user_agent: None,
            session_id: None,
            session_start: Some(now),
        });
        stats.difficulty = difficulty;
        stats.last_seen = now;
        if user_agent.is_some() {
            stats.user_agent = user_agent;
        }
        if session_id.is_some() {
            stats.session_id = session_id;
            // New session detected (new session_id) → reset session start time.
            stats.session_start = Some(now);
        }
    }

    pub async fn snapshot(&self) -> MetricsSnapshot {
        let guard = self.inner.read().await;
        let cutoff = Utc::now() - Duration::seconds(MINER_INACTIVE_SECS);
        // Only return miners that have been active within MINER_INACTIVE_SECS.
        // Inactive miners stay in the HashMap (for best_difficulty restart seeding)
        // but are hidden from the dashboard/API until they submit a new share.
        let miners = guard.miners.values()
            .filter(|m| m.last_seen >= cutoff)
            .cloned()
            .collect::<Vec<_>>();
        let total_hashrate_gh = miners.iter().map(|m| m.hashrate_gh).sum();
        let total_shares = miners.iter().map(|m| m.shares).sum();
        let total_rejected = miners.iter().map(|m| m.rejected).sum();

        MetricsSnapshot {
            miners,
            total_hashrate_gh,
            total_shares,
            total_rejected,
            total_blocks: guard.total_blocks,
            updated_at: Utc::now(),
        }
    }

    pub async fn recent_events(&self, window: Duration) -> Vec<ShareEvent> {
        let guard = self.inner.read().await;
        let cutoff = Utc::now() - window;
        guard
            .events
            .iter()
            .filter(|e| e.created_at >= cutoff)
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone)]
struct ShareSample {
    time: DateTime<Utc>,
    difficulty: f64,
}

#[derive(Default)]
struct ShareWindow {
    samples: VecDeque<ShareSample>,
    total_difficulty: f64,
}

impl ShareWindow {
    fn push(&mut self, sample: ShareSample) {
        self.total_difficulty += sample.difficulty;
        self.samples.push_back(sample);
        while self.samples.len() > SHARE_CACHE_SIZE {
            if let Some(removed) = self.samples.pop_front() {
                self.total_difficulty -= removed.difficulty;
            }
        }
    }

    fn hashrate_gh(&self) -> Option<f64> {
        if self.samples.len() < 2 {
            return None;
        }
        let first = self.samples.front()?;
        let last = self.samples.back()?;
        let window_ms = (last.time - first.time).num_milliseconds().max(1) as f64;
        let sum_diff = (self.total_difficulty - first.difficulty).max(0.0);
        if sum_diff <= 0.0 {
            return None;
        }
        let window_sec = window_ms / 1000.0;
        Some((sum_diff * HASHES_PER_DIFF) / window_sec / 1_000_000_000.0)
    }
}
