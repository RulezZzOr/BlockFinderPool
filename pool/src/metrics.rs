use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::{mpsc, Mutex as AsyncMutex, RwLock};

const HASHES_PER_DIFF: f64 = 4_294_967_296.0;
const MAX_EVENTS: usize = 4096;
const SUBMIT_RTT_WINDOW_SIZE: usize = 4096;
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
    pub best_accepted_difficulty: f64,
    pub best_block_candidate_difficulty: f64,
    pub public_pool_style_best: f64,
    pub cgminer_style_best: f64,
    pub raw_best: f64,
    pub accepted_best: f64,
    pub session_best_submitted_difficulty: f64,
    pub session_best_accepted_difficulty: f64,
    pub worker_best_submitted_difficulty: f64,
    pub worker_best_accepted_difficulty: f64,
    pub current_block_best_submitted_difficulty: f64,
    pub current_block_best_accepted_difficulty: f64,
    pub current_block_best_candidate_difficulty: f64,
    pub previous_block_best_submitted_difficulty: f64,
    pub previous_block_best_accepted_difficulty: f64,
    pub previous_block_best_candidate_difficulty: f64,
    pub last_share_status: Option<String>,
    pub last_share_difficulty: f64,
    pub last_share_at: Option<DateTime<Utc>>,
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
    pub submit_rtt_ms: f64,
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
pub struct BlockScopeSnapshot {
    pub height: u64,
    pub prevhash: String,
    pub template_key: String,
    pub job_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
struct WorkerMeta {
    user_agent: Option<String>,
    session_id: Option<String>,
}

#[derive(Debug, Default)]
struct AtomicF64(AtomicU64);

impl AtomicF64 {
    fn new(value: f64) -> Self {
        Self(AtomicU64::new(value.to_bits()))
    }

    fn load(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn store(&self, value: f64) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }

    fn fetch_max(&self, value: f64) -> bool {
        let mut current = self.0.load(Ordering::Relaxed);
        loop {
            let current_value = f64::from_bits(current);
            if value <= current_value {
                return false;
            }
            match self.0.compare_exchange(
                current,
                value.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    fn update_ema(&self, sample: f64, alpha: f64) {
        let alpha = alpha.clamp(0.0, 1.0);
        let mut current = self.0.load(Ordering::Relaxed);
        loop {
            let current_value = f64::from_bits(current);
            let next = if current_value == 0.0 {
                sample
            } else {
                (current_value * (1.0 - alpha)) + (sample * alpha)
            };
            match self.0.compare_exchange(
                current,
                next.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
}

#[derive(Debug)]
struct WorkerState {
    worker: String,
    difficulty: AtomicF64,
    best_difficulty: AtomicF64,
    best_submitted_difficulty: AtomicF64,
    best_accepted_difficulty: AtomicF64,
    best_block_candidate_difficulty: AtomicF64,
    session_best_submitted_difficulty: AtomicF64,
    session_best_accepted_difficulty: AtomicF64,
    worker_best_submitted_difficulty: AtomicF64,
    worker_best_accepted_difficulty: AtomicF64,
    current_block_best_submitted_difficulty: AtomicF64,
    current_block_best_accepted_difficulty: AtomicF64,
    current_block_best_candidate_difficulty: AtomicF64,
    previous_block_best_submitted_difficulty: AtomicF64,
    previous_block_best_accepted_difficulty: AtomicF64,
    previous_block_best_candidate_difficulty: AtomicF64,
    last_share_status_code: AtomicU64,
    last_share_difficulty: AtomicF64,
    shares: AtomicU64,
    rejected: AtomicU64,
    stale: AtomicU64,
    hashrate_gh: AtomicF64,
    notify_to_submit_ms: AtomicF64,
    submit_rtt_ms: AtomicF64,
    last_seen_secs: AtomicI64,
    last_share_time_secs: AtomicI64,
    session_start_secs: AtomicI64,
    meta: StdMutex<WorkerMeta>,
    share_samples: StdMutex<ShareWindow>,
}

impl WorkerState {
    fn new(worker: String, now: DateTime<Utc>, difficulty: f64) -> Self {
        let now_secs = now.timestamp();
        Self {
            worker,
            difficulty: AtomicF64::new(difficulty),
            best_difficulty: AtomicF64::new(0.0),
            best_submitted_difficulty: AtomicF64::new(0.0),
            best_accepted_difficulty: AtomicF64::new(0.0),
            best_block_candidate_difficulty: AtomicF64::new(0.0),
            session_best_submitted_difficulty: AtomicF64::new(0.0),
            session_best_accepted_difficulty: AtomicF64::new(0.0),
            worker_best_submitted_difficulty: AtomicF64::new(0.0),
            worker_best_accepted_difficulty: AtomicF64::new(0.0),
            current_block_best_submitted_difficulty: AtomicF64::new(0.0),
            current_block_best_accepted_difficulty: AtomicF64::new(0.0),
            current_block_best_candidate_difficulty: AtomicF64::new(0.0),
            previous_block_best_submitted_difficulty: AtomicF64::new(0.0),
            previous_block_best_accepted_difficulty: AtomicF64::new(0.0),
            previous_block_best_candidate_difficulty: AtomicF64::new(0.0),
            last_share_status_code: AtomicU64::new(0),
            last_share_difficulty: AtomicF64::new(0.0),
            shares: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            hashrate_gh: AtomicF64::new(0.0),
            notify_to_submit_ms: AtomicF64::new(0.0),
            submit_rtt_ms: AtomicF64::new(0.0),
            last_seen_secs: AtomicI64::new(now_secs),
            last_share_time_secs: AtomicI64::new(0),
            session_start_secs: AtomicI64::new(now_secs),
            meta: StdMutex::new(WorkerMeta::default()),
            share_samples: StdMutex::new(ShareWindow::default()),
        }
    }

    fn reset_session(&self, now: DateTime<Utc>) {
        let now_secs = now.timestamp();
        self.shares.store(0, Ordering::Relaxed);
        self.rejected.store(0, Ordering::Relaxed);
        self.stale.store(0, Ordering::Relaxed);
        self.hashrate_gh.store(0.0);
        self.notify_to_submit_ms.store(0.0);
        self.submit_rtt_ms.store(0.0);
        self.session_best_submitted_difficulty.store(0.0);
        self.session_best_accepted_difficulty.store(0.0);
        self.last_share_status_code.store(0, Ordering::Relaxed);
        self.last_share_difficulty.store(0.0);
        self.last_share_time_secs.store(0, Ordering::Relaxed);
        self.session_start_secs.store(now_secs, Ordering::Relaxed);
        if let Ok(mut samples) = self.share_samples.lock() {
            samples.clear();
        }
    }

    fn rotate_block_scope(&self) {
        self.previous_block_best_submitted_difficulty
            .store(self.current_block_best_submitted_difficulty.load());
        self.previous_block_best_accepted_difficulty
            .store(self.current_block_best_accepted_difficulty.load());
        self.previous_block_best_candidate_difficulty
            .store(self.current_block_best_candidate_difficulty.load());
        self.current_block_best_submitted_difficulty.store(0.0);
        self.current_block_best_accepted_difficulty.store(0.0);
        self.current_block_best_candidate_difficulty.store(0.0);
    }

    fn update_submit_seen(
        &self,
        now: DateTime<Utc>,
        target_difficulty: f64,
        share_difficulty: f64,
        is_block: bool,
        is_stale_block: bool,
    ) {
        self.difficulty.store(target_difficulty);
        self.last_seen_secs.store(now.timestamp(), Ordering::Relaxed);

        self.best_submitted_difficulty.fetch_max(share_difficulty);
        self.worker_best_submitted_difficulty.fetch_max(share_difficulty);
        self.session_best_submitted_difficulty.fetch_max(share_difficulty);
        if is_block {
            self.best_block_candidate_difficulty.fetch_max(share_difficulty);
        }

        if is_stale_block {
            self.previous_block_best_submitted_difficulty.fetch_max(share_difficulty);
            if is_block {
                self.previous_block_best_candidate_difficulty.fetch_max(share_difficulty);
            }
        } else {
            self.current_block_best_submitted_difficulty.fetch_max(share_difficulty);
            if is_block {
                self.current_block_best_candidate_difficulty.fetch_max(share_difficulty);
            }
        }
    }

    fn update_result_seen(
        &self,
        now: DateTime<Utc>,
        target_difficulty: f64,
        share_difficulty: f64,
        accepted: bool,
        is_block: bool,
        notify_to_submit_ms: i64,
        submit_rtt_ms: f64,
        is_stale_block: bool,
    ) {
        self.difficulty.store(target_difficulty);
        self.last_seen_secs.store(now.timestamp(), Ordering::Relaxed);
        self.notify_to_submit_ms.update_ema(notify_to_submit_ms as f64, 0.2);
        self.submit_rtt_ms.update_ema(submit_rtt_ms, 0.2);

        if accepted {
            self.shares.fetch_add(1, Ordering::Relaxed);
            self.best_difficulty.fetch_max(share_difficulty);
            self.best_accepted_difficulty.fetch_max(share_difficulty);
            self.worker_best_accepted_difficulty.fetch_max(share_difficulty);
            self.session_best_accepted_difficulty.fetch_max(share_difficulty);

            if is_stale_block {
                self.previous_block_best_accepted_difficulty.fetch_max(share_difficulty);
                if is_block {
                    self.previous_block_best_candidate_difficulty.fetch_max(share_difficulty);
                }
            } else {
                self.current_block_best_accepted_difficulty.fetch_max(share_difficulty);
                if is_block {
                    self.current_block_best_candidate_difficulty.fetch_max(share_difficulty);
                }
            }

            if let Ok(mut samples) = self.share_samples.lock() {
                samples.push(ShareSample {
                    time: now,
                    difficulty: target_difficulty,
                });
                if let Some(hashrate) = samples.hashrate_gh() {
                    self.hashrate_gh.store(hashrate);
                    self.last_share_time_secs.store(now.timestamp(), Ordering::Relaxed);
                }
            }
            self.last_share_status_code.store(if is_block { 4 } else { 1 }, Ordering::Relaxed);
        } else {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            self.last_share_status_code.store(2, Ordering::Relaxed);
        }
        self.last_share_difficulty.store(share_difficulty);
        if !accepted {
            self.last_share_time_secs.store(now.timestamp(), Ordering::Relaxed);
        }
    }

    fn mark_stale(&self, now: DateTime<Utc>) {
        self.stale.fetch_add(1, Ordering::Relaxed);
        self.last_seen_secs.store(now.timestamp(), Ordering::Relaxed);
        self.last_share_status_code.store(3, Ordering::Relaxed);
        self.last_share_difficulty.store(0.0);
        self.last_share_time_secs.store(now.timestamp(), Ordering::Relaxed);
    }

    fn snapshot(&self, cutoff: DateTime<Utc>) -> Option<MinerStats> {
        let last_seen_secs = self.last_seen_secs.load(Ordering::Relaxed);
        let last_seen = DateTime::<Utc>::from_timestamp(last_seen_secs, 0)?;
        if last_seen < cutoff {
            return None;
        }
        let session_start_secs = self.session_start_secs.load(Ordering::Relaxed);
        let session_start = DateTime::<Utc>::from_timestamp(session_start_secs, 0);
        let last_share_secs = self.last_share_time_secs.load(Ordering::Relaxed);
        let last_share_time = if last_share_secs > 0 {
            DateTime::<Utc>::from_timestamp(last_share_secs, 0)
        } else {
            None
        };
        let meta = self.meta.lock().ok();
        let (user_agent, session_id) = meta
            .as_ref()
            .map(|m| (m.user_agent.clone(), m.session_id.clone()))
            .unwrap_or((None, None));
        let hashrate_gh = self
            .share_samples
            .lock()
            .ok()
            .and_then(|samples| samples.hashrate_gh())
            .unwrap_or_else(|| self.hashrate_gh.load());

        let best_accepted = self.best_accepted_difficulty.load();
        let best_submitted = self.best_submitted_difficulty.load();
        let best_block_candidate = self.best_block_candidate_difficulty.load();
        let current_block_best_submitted = self.current_block_best_submitted_difficulty.load();
        let current_block_best_accepted = self.current_block_best_accepted_difficulty.load();
        let previous_block_best_submitted = self.previous_block_best_submitted_difficulty.load();
        let previous_block_best_accepted = self.previous_block_best_accepted_difficulty.load();
        let last_share_status = match self.last_share_status_code.load(Ordering::Relaxed) {
            1 => Some("accepted".to_string()),
            2 => Some("rejected".to_string()),
            3 => Some("stale".to_string()),
            4 => Some("block_candidate".to_string()),
            _ => None,
        };
        let last_share_at = if last_share_secs > 0 {
            DateTime::<Utc>::from_timestamp(last_share_secs, 0)
        } else {
            None
        };

        Some(MinerStats {
            worker: self.worker.clone(),
            difficulty: self.difficulty.load(),
            best_difficulty: best_submitted,
            best_submitted_difficulty: best_submitted,
            best_accepted_difficulty: best_accepted,
            best_block_candidate_difficulty: best_block_candidate,
            public_pool_style_best: best_submitted,
            cgminer_style_best: best_accepted,
            raw_best: best_submitted,
            accepted_best: best_accepted,
            session_best_submitted_difficulty: self.session_best_submitted_difficulty.load(),
            session_best_accepted_difficulty: self.session_best_accepted_difficulty.load(),
            worker_best_submitted_difficulty: self.worker_best_submitted_difficulty.load(),
            worker_best_accepted_difficulty: self.worker_best_accepted_difficulty.load(),
            current_block_best_submitted_difficulty: current_block_best_submitted,
            current_block_best_accepted_difficulty: current_block_best_accepted,
            current_block_best_candidate_difficulty: self.current_block_best_candidate_difficulty.load(),
            previous_block_best_submitted_difficulty: previous_block_best_submitted,
            previous_block_best_accepted_difficulty: previous_block_best_accepted,
            previous_block_best_candidate_difficulty: self.previous_block_best_candidate_difficulty.load(),
            last_share_status,
            last_share_difficulty: self.last_share_difficulty.load(),
            last_share_at,
            shares: self.shares.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            stale: self.stale.load(Ordering::Relaxed),
            hashrate_gh,
            last_seen,
            notify_to_submit_ms: self.notify_to_submit_ms.load(),
            submit_rtt_ms: self.submit_rtt_ms.load(),
            last_share_time,
            user_agent,
            session_id,
            session_start,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub miners: Vec<MinerStats>,
    pub total_hashrate_gh: f64,
    pub total_shares: u64,
    pub total_rejected: u64,
    pub total_blocks: u64,
    pub submit_rtt_p50_ms: f64,
    pub submit_rtt_p95_ms: f64,
    pub submit_rtt_p99_ms: f64,
    pub submit_rtt_max_ms: f64,
    pub submit_rtt_over_50ms_count: u64,
    pub submit_rtt_over_100ms_count: u64,
    pub global_best_submitted_difficulty: f64,
    pub global_best_accepted_difficulty: f64,
    pub global_best_block_candidate_difficulty: f64,
    pub current_block_best_submitted_difficulty: f64,
    pub current_block_best_accepted_difficulty: f64,
    pub current_block_best_candidate_difficulty: f64,
    pub previous_block_best_submitted_difficulty: f64,
    pub previous_block_best_accepted_difficulty: f64,
    pub previous_block_best_candidate_difficulty: f64,
    pub current_scope: BlockScopeSnapshot,
    pub previous_scope: Option<BlockScopeSnapshot>,
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
    /// Wall-clock timestamp of the last clean_jobs=true mining.notify.
    pub last_clean_jobs_notify_at_secs: AtomicI64,
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
    pub fn last_clean_jobs_notify_at(&self) -> Option<DateTime<Utc>> {
        let secs = self.last_clean_jobs_notify_at_secs.load(Ordering::Relaxed);
        DateTime::<Utc>::from_timestamp(secs, 0)
    }

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
    pub fn set_last_clean_jobs_notify_at(&self, now: DateTime<Utc>) {
        self.last_clean_jobs_notify_at_secs.store(now.timestamp(), Ordering::Relaxed);
    }
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
    workers: Arc<DashMap<String, Arc<WorkerState>>>,
    events: Arc<AsyncMutex<VecDeque<ShareEvent>>>,
    event_tx: mpsc::Sender<ShareEvent>,
    current_scope: Arc<RwLock<BlockScopeSnapshot>>,
    previous_scope: Arc<RwLock<Option<BlockScopeSnapshot>>>,
    total_blocks: Arc<AtomicU64>,
    global_best_submitted: Arc<AtomicF64>,
    global_best_accepted: Arc<AtomicF64>,
    global_best_block_candidate: Arc<AtomicF64>,
    current_block_best_submitted: Arc<AtomicF64>,
    current_block_best_accepted: Arc<AtomicF64>,
    current_block_best_candidate: Arc<AtomicF64>,
    previous_block_best_submitted: Arc<AtomicF64>,
    previous_block_best_accepted: Arc<AtomicF64>,
    previous_block_best_candidate: Arc<AtomicF64>,
    pub counters: Arc<PoolCounters>,
    /// UTC timestamp when the pool process started.
    /// Used by the API to compute uptime and per-minute rates.
    pub started_at: DateTime<Utc>,
}

impl MetricsStore {
    pub fn new() -> Self {
        let (event_tx, mut event_rx) = mpsc::channel::<ShareEvent>(1024);
        let events = Arc::new(AsyncMutex::new(VecDeque::with_capacity(SUBMIT_RTT_WINDOW_SIZE.max(MAX_EVENTS))));
        let events_worker = events.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let mut guard = events_worker.lock().await;
                guard.push_back(event);
                let overflow = guard.len().saturating_sub(MAX_EVENTS);
                if overflow > 0 {
                    guard.drain(0..overflow);
                }
            }
        });

        Self {
            inner:      Arc::new(RwLock::new(MetricsState::default())),
            workers:    Arc::new(DashMap::new()),
            events,
            event_tx,
            current_scope: Arc::new(RwLock::new(BlockScopeSnapshot {
                height: 0,
                prevhash: String::new(),
                template_key: String::new(),
                job_id: String::new(),
                created_at: Utc::now(),
            })),
            previous_scope: Arc::new(RwLock::new(None)),
            total_blocks: Arc::new(AtomicU64::new(0)),
            global_best_submitted: Arc::new(AtomicF64::new(0.0)),
            global_best_accepted: Arc::new(AtomicF64::new(0.0)),
            global_best_block_candidate: Arc::new(AtomicF64::new(0.0)),
            current_block_best_submitted: Arc::new(AtomicF64::new(0.0)),
            current_block_best_accepted: Arc::new(AtomicF64::new(0.0)),
            current_block_best_candidate: Arc::new(AtomicF64::new(0.0)),
            previous_block_best_submitted: Arc::new(AtomicF64::new(0.0)),
            previous_block_best_accepted: Arc::new(AtomicF64::new(0.0)),
            previous_block_best_candidate: Arc::new(AtomicF64::new(0.0)),
            counters:   Arc::new(PoolCounters::default()),
            started_at: Utc::now(),
        }
    }

    fn worker_state(&self, worker: &str, now: DateTime<Utc>, difficulty: f64) -> Arc<WorkerState> {
        if let Some(existing) = self.workers.get(worker) {
            existing.value().clone()
        } else {
            let state = Arc::new(WorkerState::new(worker.to_string(), now, difficulty));
            self.workers.insert(worker.to_string(), state.clone());
            state
        }
    }

    pub async fn set_template_scope(&self, job: &crate::template::JobTemplate) {
        let new_scope = BlockScopeSnapshot {
            height: job.height,
            prevhash: job.prevhash.clone(),
            template_key: job.template_key.clone(),
            job_id: job.job_id.clone(),
            created_at: job.created_at,
        };

        let mut current = self.current_scope.write().await;
        let rotate = current.height != 0
            && (current.height != new_scope.height || current.prevhash != new_scope.prevhash);

        if rotate {
            let previous = current.clone();
            *self.previous_scope.write().await = Some(previous);
            self.previous_block_best_submitted.store(self.current_block_best_submitted.load());
            self.previous_block_best_accepted.store(self.current_block_best_accepted.load());
            self.previous_block_best_candidate.store(self.current_block_best_candidate.load());
            self.current_block_best_submitted.store(0.0);
            self.current_block_best_accepted.store(0.0);
            self.current_block_best_candidate.store(0.0);

            for worker in self.workers.iter() {
                worker.value().rotate_block_scope();
            }
        }

        *current = new_scope;
    }

    pub async fn record_raw_share(
        &self,
        worker: &str,
        target_difficulty: f64,
        share_difficulty: f64,
        is_block: bool,
        is_stale_block: bool,
    ) {
        let now = Utc::now();
        let state = self.worker_state(worker, now, target_difficulty);
        state.update_submit_seen(now, target_difficulty, share_difficulty, is_block, is_stale_block);

        self.global_best_submitted.fetch_max(share_difficulty);
        if is_block {
            self.global_best_block_candidate.fetch_max(share_difficulty);
        }

        if is_stale_block {
            self.previous_block_best_submitted.fetch_max(share_difficulty);
            if is_block {
                self.previous_block_best_candidate.fetch_max(share_difficulty);
            }
        } else {
            self.current_block_best_submitted.fetch_max(share_difficulty);
            if is_block {
                self.current_block_best_candidate.fetch_max(share_difficulty);
            }
        }
    }

    pub async fn record_share_result(
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
        is_stale_block: bool,
    ) {
        let now = Utc::now();
        let state = self.worker_state(worker, now, target_difficulty);
        state.update_result_seen(
            now,
            target_difficulty,
            share_difficulty,
            accepted,
            is_block,
            notify_to_submit_ms,
            submit_rtt_ms,
            is_stale_block,
        );

        if accepted {
            self.global_best_accepted.fetch_max(share_difficulty);
            if is_stale_block {
                self.previous_block_best_accepted.fetch_max(share_difficulty);
                if is_block {
                    self.previous_block_best_candidate.fetch_max(share_difficulty);
                }
            } else {
                self.current_block_best_accepted.fetch_max(share_difficulty);
                if is_block {
                    self.current_block_best_candidate.fetch_max(share_difficulty);
                }
            }
            if is_block {
                self.total_blocks.fetch_add(1, Ordering::Relaxed);
            }
        }

        let _ = self.event_tx.try_send(ShareEvent {
            worker: worker.to_string(),
            difficulty: share_difficulty,
            accepted,
            is_block,
            submit_rtt_ms,
            created_at: now,
            job_age_secs,
            notify_delay_ms,
            reconnect_recent,
        });
    }

    pub async fn current_scope_snapshot(&self) -> BlockScopeSnapshot {
        self.current_scope.read().await.clone()
    }

    pub async fn previous_scope_snapshot(&self) -> Option<BlockScopeSnapshot> {
        self.previous_scope.read().await.clone()
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
                best_accepted_difficulty: 0.0,
                best_block_candidate_difficulty: 0.0,
                public_pool_style_best: 0.0,
                cgminer_style_best: 0.0,
                raw_best: 0.0,
                accepted_best: 0.0,
                session_best_submitted_difficulty: 0.0,
                session_best_accepted_difficulty: 0.0,
                worker_best_submitted_difficulty: 0.0,
                worker_best_accepted_difficulty: 0.0,
                current_block_best_submitted_difficulty: 0.0,
                current_block_best_accepted_difficulty: 0.0,
                current_block_best_candidate_difficulty: 0.0,
                previous_block_best_submitted_difficulty: 0.0,
                previous_block_best_accepted_difficulty: 0.0,
                previous_block_best_candidate_difficulty: 0.0,
                last_share_status: None,
                last_share_difficulty: 0.0,
                last_share_at: None,
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
            stats.public_pool_style_best = stats.best_submitted_difficulty;
            stats.raw_best = stats.best_submitted_difficulty;
            stats.last_share_difficulty = share_difficulty;

            if accepted {
                stats.shares += 1;
                if share_difficulty > stats.best_difficulty {
                    stats.best_difficulty = share_difficulty;
                    new_best = Some(share_difficulty);
                }
                stats.best_accepted_difficulty = stats.best_accepted_difficulty.max(share_difficulty);
                stats.cgminer_style_best = stats.best_accepted_difficulty;
                stats.accepted_best = stats.best_accepted_difficulty;
                stats.last_share_status = Some(if is_block {
                    "block_candidate".to_string()
                } else {
                    "accepted".to_string()
                });
            } else {
                stats.rejected += 1;
                stats.last_share_status = Some("rejected".to_string());
            }

            // EMA α=0.2 for both metrics — smooth over ~5 shares.
            stats.notify_to_submit_ms = (stats.notify_to_submit_ms * 0.8) + (notify_to_submit_ms as f64 * 0.2);
            stats.submit_rtt_ms       = (stats.submit_rtt_ms       * 0.8) + (submit_rtt_ms             * 0.2);

            if accepted {
                if let Some(hashrate) = new_hashrate {
                    stats.hashrate_gh = hashrate;
                }
                stats.last_share_time = Some(now);
            } else {
                stats.last_share_time = Some(now);
            }
            stats.last_share_at = Some(now);
        }

        guard.events.push(ShareEvent {
            worker: worker.to_string(),
            difficulty: share_difficulty,
            accepted,
            is_block,
            submit_rtt_ms,
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
    pub async fn set_worker_best(
        &self,
        worker: &str,
        best_submitted: f64,
        best_accepted: f64,
        best_block_candidate: f64,
    ) {
        let now = Utc::now();
        let state = self.worker_state(worker, now, best_submitted.max(best_accepted));
        state.best_difficulty.fetch_max(best_submitted);
        state.best_submitted_difficulty.fetch_max(best_submitted);
        state.best_accepted_difficulty.fetch_max(best_accepted);
        state.best_block_candidate_difficulty.fetch_max(best_block_candidate);
        state.worker_best_submitted_difficulty.fetch_max(best_submitted);
        state.worker_best_accepted_difficulty.fetch_max(best_accepted);
        self.global_best_submitted.fetch_max(best_submitted);
        self.global_best_accepted.fetch_max(best_accepted);
        self.global_best_block_candidate.fetch_max(best_block_candidate);
        state.last_seen_secs.store(0, Ordering::Relaxed);
        state.session_start_secs.store(0, Ordering::Relaxed);
        state.last_share_time_secs.store(0, Ordering::Relaxed);
    }

    pub async fn record_stale(&self, worker: &str, reason: StaleReason) {
        let now = Utc::now();
        self.counters.inc_stale(reason);
        let state = self.worker_state(worker, now, 0.0);
        state.mark_stale(now);
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
        let state = self.worker_state(worker, now, difficulty);
        let mut reset_session = false;

        if let Ok(meta) = state.meta.lock() {
            if state.last_seen_secs.load(Ordering::Relaxed) < cutoff.timestamp() {
                reset_session = true;
            }
            if session_id.is_some() && meta.session_id != session_id {
                reset_session = true;
            }
        }

        if reset_session {
            state.reset_session(now);
        }

        state.difficulty.store(difficulty);
        state.last_seen_secs.store(now.timestamp(), Ordering::Relaxed);
        state.session_start_secs.store(now.timestamp(), Ordering::Relaxed);

        if let Ok(mut meta) = state.meta.lock() {
            if user_agent.is_some() {
                meta.user_agent = user_agent;
            }
            if session_id.is_some() {
                meta.session_id = session_id;
            }
        };
    }

    pub async fn snapshot(&self) -> MetricsSnapshot {
        let cutoff = Utc::now() - Duration::seconds(MINER_INACTIVE_SECS);
        let miners = self
            .workers
            .iter()
            .filter_map(|entry| entry.value().snapshot(cutoff))
            .collect::<Vec<_>>();
        let total_hashrate_gh = miners.iter().map(|m| m.hashrate_gh).sum();
        let total_shares = miners.iter().map(|m| m.shares).sum();
        let total_rejected = miners.iter().map(|m| m.rejected).sum();
        let current_scope = self.current_scope.read().await.clone();
        let previous_scope = self.previous_scope.read().await.clone();
        let events = self.events.lock().await;
        let mut submit_rtts: Vec<f64> = events
            .iter()
            .map(|event| event.submit_rtt_ms)
            .filter(|value| value.is_finite() && *value >= 0.0)
            .collect();
        submit_rtts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let submit_rtt_max = submit_rtts.iter().copied().fold(0.0, f64::max);
        let submit_rtt_over_50ms_count = submit_rtts.iter().filter(|v| **v > 50.0).count() as u64;
        let submit_rtt_over_100ms_count = submit_rtts.iter().filter(|v| **v > 100.0).count() as u64;
        let percentile = |p: f64| -> f64 {
            if submit_rtts.is_empty() {
                return 0.0;
            }
            let p = p.clamp(0.0, 1.0);
            let idx = ((submit_rtts.len() - 1) as f64 * p).round() as usize;
            submit_rtts[idx]
        };

        MetricsSnapshot {
            miners,
            total_hashrate_gh,
            total_shares,
            total_rejected,
            total_blocks: self.total_blocks.load(Ordering::Relaxed),
            submit_rtt_p50_ms: percentile(0.50),
            submit_rtt_p95_ms: percentile(0.95),
            submit_rtt_p99_ms: percentile(0.99),
            submit_rtt_max_ms: submit_rtt_max,
            submit_rtt_over_50ms_count,
            submit_rtt_over_100ms_count,
            global_best_submitted_difficulty: self.global_best_submitted.load(),
            global_best_accepted_difficulty: self.global_best_accepted.load(),
            global_best_block_candidate_difficulty: self.global_best_block_candidate.load(),
            current_block_best_submitted_difficulty: self.current_block_best_submitted.load(),
            current_block_best_accepted_difficulty: self.current_block_best_accepted.load(),
            current_block_best_candidate_difficulty: self.current_block_best_candidate.load(),
            previous_block_best_submitted_difficulty: self.previous_block_best_submitted.load(),
            previous_block_best_accepted_difficulty: self.previous_block_best_accepted.load(),
            previous_block_best_candidate_difficulty: self.previous_block_best_candidate.load(),
            current_scope,
            previous_scope,
            updated_at: Utc::now(),
        }
    }

    pub async fn recent_events(&self, window: Duration) -> Vec<ShareEvent> {
        let cutoff = Utc::now() - window;
        let guard = self.events.lock().await;
        guard
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

#[derive(Debug, Default)]
struct ShareWindow {
    samples: VecDeque<ShareSample>,
    total_difficulty: f64,
}

impl ShareWindow {
    fn clear(&mut self) {
        self.samples.clear();
        self.total_difficulty = 0.0;
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::SqliteStore;
    use crate::template::JobTemplate;

    fn test_template(height: u64, prevhash: &str, template_key: &str, job_id: &str) -> JobTemplate {
        let mut job = JobTemplate::empty();
        job.ready = true;
        job.height = height;
        job.prevhash = prevhash.to_string();
        job.template_key = template_key.to_string();
        job.job_id = job_id.to_string();
        job.created_at = Utc::now();
        job
    }

    async fn visible_miner(metrics: &MetricsStore, worker: &str) {
        metrics
            .record_miner_seen(worker, 64.0, None, None)
            .await;
    }

    #[tokio::test]
    async fn rejected_share_updates_raw_best_only() {
        let metrics = MetricsStore::new();
        visible_miner(&metrics, "worker-a").await;

        metrics
            .record_raw_share("worker-a", 64.0, 123.0, false, false)
            .await;
        metrics
            .record_share_result(
                "worker-a",
                64.0,
                123.0,
                false,
                false,
                10,
                1.0,
                0,
                0,
                false,
                false,
            )
            .await;

        let snapshot = metrics.snapshot().await;
        let miner = snapshot.miners.iter().find(|m| m.worker == "worker-a").unwrap();
        assert_eq!(miner.best_submitted_difficulty, 123.0);
        assert_eq!(miner.best_accepted_difficulty, 0.0);
        assert_eq!(snapshot.global_best_submitted_difficulty, 123.0);
        assert_eq!(snapshot.global_best_accepted_difficulty, 0.0);
    }

    #[tokio::test]
    async fn accepted_share_updates_both_best_views() {
        let metrics = MetricsStore::new();
        visible_miner(&metrics, "worker-b").await;

        metrics
            .record_raw_share("worker-b", 128.0, 456.0, false, false)
            .await;
        metrics
            .record_share_result(
                "worker-b",
                128.0,
                456.0,
                true,
                false,
                12,
                0.8,
                1,
                1,
                false,
                false,
            )
            .await;

        let snapshot = metrics.snapshot().await;
        let miner = snapshot.miners.iter().find(|m| m.worker == "worker-b").unwrap();
        assert_eq!(miner.best_submitted_difficulty, 456.0);
        assert_eq!(miner.best_accepted_difficulty, 456.0);
        assert_eq!(snapshot.global_best_submitted_difficulty, 456.0);
        assert_eq!(snapshot.global_best_accepted_difficulty, 456.0);
    }

    #[tokio::test]
    async fn block_candidate_updates_candidate_views() {
        let metrics = MetricsStore::new();
        visible_miner(&metrics, "worker-block").await;

        metrics.set_template_scope(&test_template(200, "ccc", "scope-block", "9")).await;
        metrics
            .record_raw_share("worker-block", 512.0, 1_234.0, true, false)
            .await;
        metrics
            .record_share_result(
                "worker-block",
                512.0,
                1_234.0,
                true,
                true,
                8,
                0.6,
                0,
                0,
                false,
                false,
            )
            .await;

        let snapshot = metrics.snapshot().await;
        let miner = snapshot.miners.iter().find(|m| m.worker == "worker-block").unwrap();
        assert_eq!(miner.best_block_candidate_difficulty, 1_234.0);
        assert_eq!(miner.current_block_best_candidate_difficulty, 1_234.0);
        assert_eq!(snapshot.global_best_block_candidate_difficulty, 1_234.0);
        assert_eq!(snapshot.current_block_best_candidate_difficulty, 1_234.0);
    }

    #[tokio::test]
    async fn stale_high_diff_share_keeps_accepted_best_unchanged() {
        let metrics = MetricsStore::new();
        visible_miner(&metrics, "worker-c").await;

        metrics
            .record_raw_share("worker-c", 64.0, 99.0, false, false)
            .await;
        metrics
            .record_share_result(
                "worker-c",
                64.0,
                99.0,
                true,
                false,
                10,
                1.0,
                0,
                0,
                false,
                false,
            )
            .await;

        metrics
            .record_raw_share("worker-c", 64.0, 999.0, false, true)
            .await;
        metrics
            .record_share_result(
                "worker-c",
                64.0,
                999.0,
                false,
                false,
                11,
                1.1,
                0,
                0,
                false,
                true,
            )
            .await;

        let snapshot = metrics.snapshot().await;
        let miner = snapshot.miners.iter().find(|m| m.worker == "worker-c").unwrap();
        assert_eq!(miner.best_submitted_difficulty, 999.0);
        assert_eq!(miner.best_accepted_difficulty, 99.0);
    }

    #[tokio::test]
    async fn block_scope_resets_current_best_but_preserves_previous_and_all_time() {
        let metrics = MetricsStore::new();
        visible_miner(&metrics, "worker-d").await;

        metrics.set_template_scope(&test_template(100, "aaa", "scope-1", "1")).await;
        metrics
            .record_raw_share("worker-d", 64.0, 77.0, false, false)
            .await;
        metrics
            .record_share_result(
                "worker-d",
                64.0,
                77.0,
                true,
                false,
                10,
                1.0,
                0,
                0,
                false,
                false,
            )
            .await;

        metrics.set_template_scope(&test_template(101, "bbb", "scope-2", "2")).await;
        let snapshot = metrics.snapshot().await;
        assert_eq!(snapshot.current_block_best_submitted_difficulty, 0.0);
        assert_eq!(snapshot.current_block_best_accepted_difficulty, 0.0);
        assert_eq!(snapshot.previous_block_best_submitted_difficulty, 77.0);
        assert_eq!(snapshot.previous_block_best_accepted_difficulty, 77.0);
        assert_eq!(snapshot.global_best_submitted_difficulty, 77.0);
        assert_eq!(snapshot.global_best_accepted_difficulty, 77.0);
    }

    #[tokio::test]
    async fn persisted_best_round_trips_through_sqlite() {
        let metrics = MetricsStore::new();
        visible_miner(&metrics, "worker-e").await;
        metrics.set_worker_best("worker-e", 123.0, 111.0, 99.0).await;
        metrics.record_miner_seen("worker-e", 64.0, None, None).await;

        let snapshot = metrics.snapshot().await;
        let sqlite = SqliteStore::connect(Some("sqlite::memory:")).await.unwrap();
        assert!(sqlite.is_enabled());
        sqlite.persist_best_snapshot(&snapshot).await.unwrap();
        let bests = sqlite.load_worker_bests().await.unwrap();
        let (submitted, accepted, candidate) = bests.get("worker-e").copied().unwrap();
        assert_eq!(submitted, 123.0);
        assert_eq!(accepted, 111.0);
        assert_eq!(candidate, 99.0);
    }
}
