use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use bitcoin::Address;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, watch};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::config::Config;
use crate::hash::{double_sha256, merkle_step};
use crate::metrics::{MetricsStore, PoolCounters};
use crate::rpc::RpcClient;

#[derive(Debug, Clone)]
pub struct JobTemplate {
    // Stratum-facing fields (hex strings)
    pub ready: bool,
    pub job_id: String,
    pub prevhash: String,
    pub prevhash_le: String,
    pub coinbase1: String,
    pub coinbase2: String,
    pub merkle_branches: Vec<String>,
    pub version: String,
    pub nbits: String,
    pub ntime: String,
    pub target: String,
    pub height: u64,
    pub transactions: Vec<String>,
    pub has_witness_commitment: bool,
    pub coinbase_value: u64,
    pub created_at: DateTime<Utc>,

    // Pre-decoded / pre-parsed fields for fast share validation (bytes are little-endian)
    pub prevhash_le_bytes: [u8; 32],
    pub target_le: [u8; 32],
    pub coinbase1_bytes: Vec<u8>,
    pub coinbase2_bytes: Vec<u8>,
    pub merkle_branches_le: Vec<[u8; 32]>,
    pub version_u32: u32,
    pub nbits_u32: u32,
    /// Minimum allowed nTime for shares/blocks derived from this template (from getblocktemplate.mintime).
    /// Stored as seconds since epoch.
    pub mintime_u32: u32,
    /// Network difficulty as f64 — for metrics/UI display ONLY.
    /// Block detection uses `target_le` (256-bit integer comparison), NOT this field.
    pub network_difficulty: f64,
    /// Opaque key identifying the GBT snapshot this job was built from.
    /// Format: "{prevhash}:{bits}:{version}:{longpollid}:{txid_partial_root}"
    /// Logged on submitblock rejection for fast post-mortem.
    pub template_key: String,
    /// Merkle root of non-coinbase txids (big-endian hex, 64 chars).
    /// Combined with coinbase TXID to produce the full header merkle root.
    /// Logged on rejection to verify merkle branch computation.
    pub txid_partial_root: String,
    /// Hex of the witness commitment OUTPUT SCRIPT (6a24aa21a9ed + 32-byte hash).
    /// None for non-SegWit templates. Logged on witness-category rejections.
    pub witness_commitment_script: Option<String>,
}

impl JobTemplate {
    pub fn empty() -> Self {
        Self {
            ready: false,
            job_id: "0".to_string(),
            prevhash: String::new(),
            prevhash_le: String::new(),
            coinbase1: String::new(),
            coinbase2: String::new(),
            merkle_branches: vec![],
            version: String::new(),
            nbits: String::new(),
            ntime: String::new(),
            target: String::new(),
            height: 0,
            transactions: vec![],
            has_witness_commitment: false,
            coinbase_value: 0,
            created_at: Utc::now(),

            prevhash_le_bytes: [0u8; 32],
            target_le: [0u8; 32],
            coinbase1_bytes: Vec::new(),
            coinbase2_bytes: Vec::new(),
            merkle_branches_le: vec![],
            version_u32: 0,
            nbits_u32: 0,
            mintime_u32: 0,
            network_difficulty: 0.0,
            template_key: String::new(),
            txid_partial_root: String::new(),
            witness_commitment_script: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GbtResponse {
    longpollid: Option<String>,
    version: u32,
    previousblockhash: String,
    transactions: Vec<GbtTx>,
    coinbasevalue: u64,
    bits: String,
    curtime: u64,
    mintime: Option<u64>,
    height: u64,
    target: String,
    coinbaseaux: Option<GbtCoinbaseAux>,
    default_witness_commitment: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GbtCoinbaseAux {
    flags: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GbtTx {
    data: String,
    txid: Option<String>,
    hash: Option<String>,
}

/// Result of the TX ZMQ debounce/suppression check.
/// Returned by `zmq_tx_action()` so callers increment the correct counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxAction {
    /// Fire: debounce passed and not in post-block window → trigger GBT refresh.
    Fire,
    /// Suppressed by ZMQ_DEBOUNCE_MS (normal inter-tx gap too short).
    /// Expected and healthy — Bitcoin Core emits hashtx for every mempool entry.
    Debounced,
    /// Suppressed by the 15s post-block window armed by hashblock ZMQ.
    /// Healthy IFF correlated with a recent zmqBlockReceived increment.
    PostBlockSuppressed,
}

#[derive(Clone)]
pub struct TemplateEngine {
    config: Config,
    rpc: RpcClient,
    sender: watch::Sender<Arc<JobTemplate>>,
    job_counter: Arc<AtomicU64>,
    last_template_key: Arc<Mutex<Option<String>>>,
    refresh_sem: Arc<Semaphore>,
    /// If refresh triggers arrive while a refresh is already in-flight, we set this flag.
    /// The in-flight refresh will immediately run another refresh pass before releasing the permit.
    /// This prevents "missing" updates during bursts (e.g., new block arrives mid-refresh).
    refresh_pending: Arc<AtomicBool>,
    /// Debounce for block ZMQ notifications (hashblock/rawblock).
    /// Kept separate from TX debounce so that a burst of TX notifications
    /// can NEVER suppress or delay a new-block template refresh.
    last_zmq_block_trigger_ms: Arc<AtomicU64>,
    /// Debounce for TX ZMQ notifications (hashtx/rawtx).
    last_zmq_trigger_ms: Arc<AtomicU64>,
    /// Post-block TX suppression: after a hashblock ZMQ fires, suppress hashtx-driven
    /// GBT refreshes for POST_BLOCK_SUPPRESS_MS (8s default) so the mempool burst
    /// doesn't hammer bitcoind.  After the window expires, ONE refresh captures all
    /// accumulated high-fee txns.  Stored as absolute epoch-ms (0 = not suppressing).
    post_block_suppress_until_ms: Arc<AtomicU64>,
    /// Last longpollid returned by Bitcoin Core's GBT.
    /// Passed on the next poll_loop GBT call so Core blocks until template changes —
    /// this is the longpoll fallback when ZMQ is unavailable.
    last_longpollid: Arc<Mutex<Option<String>>>,
    /// Shared pool-wide counters (ZMQ activity, job storms, etc.).
    counters: Arc<PoolCounters>,
    /// Metrics store used to rotate per-block best-share scopes when templates change.
    metrics: MetricsStore,
    template_refresh_failures: Arc<AtomicU64>,
    rpc_healthy: Arc<AtomicBool>,
    last_template_refresh_ms: Arc<AtomicI64>,
    zmq_block_connected: Arc<AtomicBool>,
    zmq_tx_connected: Arc<AtomicBool>,
}

impl TemplateEngine {
    pub fn new(
        config: Config,
        rpc: RpcClient,
        metrics: MetricsStore,
        counters: Arc<PoolCounters>,
    ) -> Self {
        let (sender, _) = watch::channel(Arc::new(JobTemplate::empty()));
        Self {
            config,
            rpc,
            sender,
            job_counter: Arc::new(AtomicU64::new(1)),
            last_template_key: Arc::new(Mutex::new(None)),
            // Coalesce refresh triggers (poll + ZMQ block + ZMQ tx) to avoid RPC bursts.
            refresh_sem: Arc::new(Semaphore::new(1)),
            refresh_pending: Arc::new(AtomicBool::new(false)),
            last_zmq_block_trigger_ms: Arc::new(AtomicU64::new(0)),
            last_zmq_trigger_ms: Arc::new(AtomicU64::new(0)),
            post_block_suppress_until_ms: Arc::new(AtomicU64::new(0)),
            last_longpollid: Arc::new(Mutex::new(None)),
            metrics,
            counters,
            template_refresh_failures: Arc::new(AtomicU64::new(0)),
            rpc_healthy: Arc::new(AtomicBool::new(false)),
            last_template_refresh_ms: Arc::new(AtomicI64::new(0)),
            zmq_block_connected: Arc::new(AtomicBool::new(false)),
            zmq_tx_connected: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<JobTemplate>> {
        self.sender.subscribe()
    }

    pub fn last_zmq_block_trigger_at(&self) -> Option<DateTime<Utc>> {
        let ms = self.last_zmq_block_trigger_ms.load(Ordering::Relaxed);
        if ms == 0 {
            None
        } else {
            DateTime::<Utc>::from_timestamp_millis(ms as i64)
        }
    }

    pub fn last_zmq_tx_trigger_at(&self) -> Option<DateTime<Utc>> {
        let ms = self.last_zmq_trigger_ms.load(Ordering::Relaxed);
        if ms == 0 {
            None
        } else {
            DateTime::<Utc>::from_timestamp_millis(ms as i64)
        }
    }

    pub fn template_age_secs(&self) -> Option<u64> {
        let job = self.subscribe().borrow().clone();
        if !job.ready {
            return None;
        }
        let age = (Utc::now() - job.created_at).num_seconds().max(0) as u64;
        Some(age)
    }

    pub fn last_template_refresh_at(&self) -> Option<DateTime<Utc>> {
        let secs = self.last_template_refresh_ms.load(Ordering::Relaxed);
        DateTime::<Utc>::from_timestamp(secs, 0)
    }

    pub fn template_refresh_failures(&self) -> u64 {
        self.template_refresh_failures.load(Ordering::Relaxed)
    }

    pub fn rpc_healthy(&self) -> bool {
        self.rpc_healthy.load(Ordering::Relaxed)
    }

    pub fn zmq_connected(&self) -> bool {
        let block_required = !self.config.zmq_block_urls.is_empty();
        let tx_required = !self.config.zmq_tx_urls.is_empty();

        (!block_required || self.zmq_block_connected.load(Ordering::Relaxed))
            && (!tx_required || self.zmq_tx_connected.load(Ordering::Relaxed))
    }

    pub fn zmq_block_connected(&self) -> bool {
        self.zmq_block_connected.load(Ordering::Relaxed)
    }

    pub fn zmq_tx_connected(&self) -> bool {
        self.zmq_tx_connected.load(Ordering::Relaxed)
    }

    fn note_template_refresh_success(&self) {
        self.rpc_healthy.store(true, Ordering::Relaxed);
        self.last_template_refresh_ms.store(Utc::now().timestamp(), Ordering::Relaxed);
    }

    fn note_template_refresh_failure(&self) {
        self.template_refresh_failures.fetch_add(1, Ordering::Relaxed);
        self.rpc_healthy.store(false, Ordering::Relaxed);
    }


    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_millis() as u64
    }

    /// Block debounce: 100 ms to coalesce hashblock + rawblock arriving simultaneously.
    /// Also arms the post-block TX suppression window (POST_BLOCK_SUPPRESS_MS default 8 s).
    ///
    /// Separation from TX debounce is intentional: a flood of hashtx notifications
    /// CANNOT delay or suppress a new-block template refresh.
    fn should_fire_block_zmq_trigger(&self) -> bool {
        // 10ms coalesces hashblock+rawblock arriving simultaneously on dual ZMQ sockets
        // while minimising the stale window. With dual endpoints (28334+28332), both
        // topics fire within ~1ms of each other — 10ms is enough to deduplicate them.
        const BLOCK_DEBOUNCE_MS: u64 = 10;

        let now = Self::now_ms();
        let last = self.last_zmq_block_trigger_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < BLOCK_DEBOUNCE_MS {
            return false;
        }
        self.last_zmq_block_trigger_ms.store(now, Ordering::Relaxed);
        // Arm the TX suppression window so the post-block mempool burst is batched.
        let suppress_ms = self.config.post_block_suppress_ms;
        self.post_block_suppress_until_ms.store(now + suppress_ms, Ordering::Relaxed);
        true
    }

    /// TX debounce check. Returns the action to take so the caller can increment
    /// the correct counter without mixing suppression reasons.
    ///
    /// Two independent suppression mechanisms:
    ///   1. Post-block window (15 s after hashblock) — `TxAction::PostBlockSuppressed`
    ///      Countered by zmq_tx_post_block_suppressed.
    ///      Healthy if correlated with zmq_block_received.
    ///   2. ZMQ_DEBOUNCE_MS normal debounce — `TxAction::Debounced`
    ///      Countered by zmq_tx_debounced.
    ///      Always expected; Bitcoin Core emits hashtx for every mempool entry.
    ///   3. Fire → `TxAction::Fire` — countered by zmq_tx_triggered.
    fn zmq_tx_action(&self) -> TxAction {
        let now = Self::now_ms();
        if now < self.post_block_suppress_until_ms.load(Ordering::Relaxed) {
            return TxAction::PostBlockSuppressed;
        }
        let last = self.last_zmq_trigger_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < self.config.zmq_debounce_ms {
            return TxAction::Debounced;
        }
        self.last_zmq_trigger_ms.store(now, Ordering::Relaxed);
        TxAction::Fire
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        info!(
            "template engine starting: block_zmq_endpoints={} tx_zmq_endpoints={} template_poll_ms={}",
            self.config.zmq_block_urls.len(),
            self.config.zmq_tx_urls.len(),
            self.config.template_poll_ms
        );

        if self.config.template_poll_ms > 0 {
            let poll_engine = self.clone();
            tokio::spawn(async move {
                poll_engine.poll_loop().await;
            });
        }

        if !self.config.zmq_block_urls.is_empty() {
            let zmq_engine = self.clone();
            let urls = self.config.zmq_block_urls.clone();
            tokio::spawn(async move {
                zmq_engine.zmq_loop(urls).await;
            });
        }

        if !self.config.zmq_tx_urls.is_empty() {
            let zmq_engine = self.clone();
            let urls = self.config.zmq_tx_urls.clone();
            tokio::spawn(async move {
                // Debounce tx-driven refreshes to avoid hammering bitcoind.
                zmq_engine.zmq_tx_loop(urls).await;
            });
        }

        self.warm_initial_template().await?;

        Ok(())
    }

    async fn warm_initial_template(&self) -> anyhow::Result<()> {
        const MAX_ATTEMPTS: usize = 12;
        const ATTEMPT_TIMEOUT_SECS: u64 = 15;
        const RETRY_DELAY_SECS: u64 = 2;

        for attempt in 1..=MAX_ATTEMPTS {
            eprintln!(
                "blackhole-pool init: initial template warmup attempt {attempt}/{MAX_ATTEMPTS}"
            );
            info!(
                "initial template warmup attempt {attempt}/{MAX_ATTEMPTS} (timeout={}s)",
                ATTEMPT_TIMEOUT_SECS
            );

            match tokio::time::timeout(
                Duration::from_secs(ATTEMPT_TIMEOUT_SECS),
                self.refresh_template(),
            )
            .await
            {
                Ok(Ok(())) => {
                    eprintln!("blackhole-pool init: connected to bitcoind; initial template loaded");
                    info!("connected to bitcoind; initial template loaded");
                    self.note_template_refresh_success();
                    return Ok(());
                }
                Ok(Err(err)) => {
                    eprintln!(
                        "blackhole-pool init: initial template warmup failed on attempt {attempt}/{MAX_ATTEMPTS}: {err:?}"
                    );
                    warn!(
                        "initial template warmup failed on attempt {attempt}/{MAX_ATTEMPTS}: {err:?}"
                    );
                }
                Err(_) => {
                    eprintln!(
                        "blackhole-pool init: initial template warmup timed out after {}s on attempt {attempt}/{MAX_ATTEMPTS}",
                        ATTEMPT_TIMEOUT_SECS
                    );
                    warn!(
                        "initial template warmup timed out after {}s on attempt {attempt}/{MAX_ATTEMPTS}",
                        ATTEMPT_TIMEOUT_SECS
                    );
                }
            }

            if attempt < MAX_ATTEMPTS {
                tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
            }
        }

        eprintln!("blackhole-pool init: initial template warmup failed after {MAX_ATTEMPTS} attempts");
        Err(anyhow!(
            "initial template warmup failed after {MAX_ATTEMPTS} attempts; bitcoind/ZMQ may not be ready"
        ))
    }

    /// Submit a solved block to Bitcoin Core.
    ///
    /// `block_hex`       — serialised block (80B header + coinbase + txns), hex.
    /// `block_hash_hex`  — SHA256d of the 80B header (hex, for logging).
    /// `template_key`    — opaque GBT snapshot key, included in rejection logs.
    /// `coinbase_hex`    — full coinbase tx (hex) for witness/merkle post-mortem.
    /// `txid_partial_root` — partial Merkle root of non-coinbase txids (BE hex, 64 chars).
    /// `witness_script`  — witness commitment output script hex (6a24aa21a9ed...) or empty.
    pub async fn submit_block(
        &self,
        block_hex: &str,
        block_hash_hex: &str,
        template_key: &str,
        coinbase_hex: &str,
        txid_partial_root: &str,
        witness_script: &str,
    ) -> anyhow::Result<()> {
        // Retry the submitblock RPC call itself (network hiccup, Bitcoin Core briefly busy).
        // A missed block is catastrophic for solo mining — exhaust retries before giving up.
        let submit_delays_ms = [0u64, 500, 1000, 2000, 4000];
        let mut last_submit_err: Option<anyhow::Error> = None;
        for (attempt, delay_ms) in submit_delays_ms.into_iter().enumerate() {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            // call_optional returns Ok(None) for null (accepted), Ok(Some(reason)) for
            // rejection, and Err for network/RPC failures. Using call::<Option<String>>
            // would return Err for the null-success case (serde maps null→None, then
            // ok_or_else converts None to Err), causing false "BLOCK MAY BE LOST" logs.
            match self.rpc.call_optional::<String>("submitblock", json!([block_hex])).await {
                Ok(None) => {
                    // null result = Bitcoin Core accepted the block.
                    self.counters.inc_submitblock_accepted();
                    last_submit_err = None;
                    break;
                }
                Ok(Some(reason)) => {
                    // Non-null string = Bitcoin Core returned a rejection reason.
                    if reason == "duplicate" {
                        tracing::warn!("submitblock: already known (duplicate) — block counted as found");
                        self.counters.inc_submitblock_accepted();
                        return Ok(());
                    }

                    // ── Categorised rejection logging — repro bundle ────────
                    // Everything needed to reproduce the block outside the pool:
                    //   1. header_hex      → `bitcoin-cli getblocktemplate` + manual reconstruct
                    //   2. coinbase_hex    → decode and verify OP_RETURN 0xaa21a9ed + commitment
                    //   3. txid_root       → compare against header bytes [36..68] (merkle root, LE)
                    //   4. witness_script  → verify 6a24aa21a9ed prefix + 32-byte hash
                    //   5. template_key    → identifies the exact GBT snapshot
                    let category = categorise_reject_reason(&reason);
                    let header_hex = if block_hex.len() >= 160 { &block_hex[..160] } else { block_hex };
                    tracing::error!(
                        "SUBMITBLOCK REJECTED: reason=\"{}\" category={} hash={} template_key=\"{}\"",
                        reason, category, block_hash_hex, template_key
                    );
                    tracing::error!("  header_hex={}", header_hex);
                    tracing::error!("  coinbase_hex={}", coinbase_hex);
                    tracing::error!("  txid_partial_root={}", txid_partial_root);
                    if !witness_script.is_empty() {
                        tracing::error!("  witness_commitment_script={}", witness_script);
                    }
                    // Hint what to inspect for each category
                    match category {
                        "merkle" => tracing::error!(
                            "  → bad-txnmrklroot: TXID merkle root mismatch.\
                             \n     Expected header[36..68] (LE) = merkle(coinbase_txid, txid_partial_root branches)."),
                        "witness" => tracing::error!(
                            "  → witness commitment mismatch.\
                             \n     Decode coinbase_hex, find OP_RETURN (0x6a24aa21a9ed).\
                             \n     Verify: SHA256d(wtxid_merkle_root || 32_zero_bytes) matches last 32 bytes of witness_commitment_script."),
                        "coinbase" => tracing::error!(
                            "  → coinbase issue. Check scriptSig ≤ 100B and BIP34 height encoding."),
                        "time" => tracing::error!(
                            "  → ntime out of range. Check mintime ≤ ntime ≤ (MTP+7200)s."),
                        "stale" => tracing::error!(
                            "  → bad-prevblk: block built on stale tip. ZMQ block notification too slow?"),
                        _ => {}
                    }
                    self.counters.inc_submitblock_rejected();
                    return Err(anyhow!("submitblock rejected: {reason} (category: {category})"));
                }
                Err(err) => {
                    tracing::error!(
                        "submitblock RPC attempt {}/{} failed: {err:?} — retrying",
                        attempt + 1,
                        submit_delays_ms.len()
                    );
                    last_submit_err = Some(err);
                }
            }
        }
        if let Some(err) = last_submit_err {
            self.counters.inc_submitblock_rpc_fail();
            return Err(err).context("submitblock RPC failed after all retries — BLOCK MAY BE LOST");
        }

        // Verify the node accepted the block into its block index/mempool processing.
        // (This does not guarantee it became best tip, but it confirms the node knows it.)
        // Retry briefly because validation/processing can be asynchronous.
        for (i, delay_ms) in [300u64, 600, 1200, 2400, 4800].into_iter().enumerate() {
            let header_res: anyhow::Result<serde_json::Value> = self
                .rpc
                .call("getblockheader", json!([block_hash_hex, false]))
                .await;
            match header_res {
                Ok(_) => {
                    tracing::info!("BLOCK CONFIRMED in chain: {block_hash_hex}");
                    return Ok(());
                }
                Err(err) => {
                    // -5 is "Block not found" in Bitcoin Core. Anything else is likely real.
                    let msg = format!("{err}");
                    if !msg.contains("(-5)") && !msg.contains("Block not found") {
                        return Err(err).context("getblockheader failed after submit");
                    }
                }
            }
            // Avoid sleeping after last attempt
            if i < 4 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }

        // submitblock returned null (success) but block not yet visible in index.
        // The block was submitted to the network — it likely propagated even if we can't confirm it.
        tracing::warn!(
            "submitblock accepted but getblockheader timeout for {block_hash_hex} — block was submitted"
        );
        Ok(())
    }

    async fn poll_loop(&self) {
        // ── True longpoll fallback ────────────────────────────────────────────
        //
        // Design principle: poll_loop OWNS the blocking HTTP call; it passes the
        // already-fetched GbtResponse directly to apply_gbt_response().
        // This guarantees ONE GBT round-trip per template change — no double-fetch.
        //
        // Flow per iteration:
        //   1. Read last longpollid (updated by every successful apply_gbt_response).
        //   2. Call GBT with longpollid → Bitcoin Core BLOCKS the HTTP request
        //      until the template changes (new block OR significant mempool update).
        //   3. On return: call apply_gbt_response(gbt) — dedup key checked,
        //      if changed → build job → broadcast.  NO second GBT call.
        //   4. Outer timeout = 120 s (Core's server-side longpoll is ~90 s;
        //      we give 30 s extra to avoid a race with Core's own timeout).
        //      On timeout: do a plain refresh as safety net, then loop.
        //
        // With ZMQ active: ZMQ triggers apply_gbt_response() first.  By the time
        // poll_loop gets Core's longpoll response, apply_gbt_response() may already
        // have processed the same template — the dedup key ensures only one
        // notify_deduped++ and no duplicate job broadcast.

        const LONGPOLL_TIMEOUT_SECS: u64 = 120;

        loop {
            let lpid: Option<String> = {
                let guard = self.last_longpollid.lock().await;
                guard.clone()
            };

            let gbt_params = match lpid.as_deref() {
                Some(id) if !id.is_empty() => json!([{"rules": ["segwit"], "longpollid": id}]),
                _ => json!([{"rules": ["segwit"]}]),
            };

            // Use the longpoll-aware client (130s timeout) so Bitcoin Core can
            // hold the connection for its full ~90s before returning a new template.
            // The outer tokio::timeout is a belt-and-suspenders backstop only.
            let fetch_result = tokio::time::timeout(
                Duration::from_secs(LONGPOLL_TIMEOUT_SECS),
                self.rpc.call_longpoll::<GbtResponse>("getblocktemplate", gbt_params),
            ).await;

            match fetch_result {
                Ok(Ok(gbt)) => {
                    // apply_gbt_response: dedup → build → broadcast. ONE call, no re-fetch.
                    if let Err(err) = self.apply_gbt_response(gbt).await {
                        self.note_template_refresh_failure();
                        warn!("longpoll apply_gbt_response failed: {err:?}");
                    } else {
                        self.note_template_refresh_success();
                    }
                    // Loop immediately — no sleep. Core will block the next call.
                }
                Ok(Err(err)) => {
                    self.note_template_refresh_failure();
                    warn!("longpoll GBT call failed: {err:?}; retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(_elapsed) => {
                    // Core held longpoll for 120 s without a template change.
                    // Do a plain (non-blocking) refresh as safety net, then loop.
                    if let Err(err) = self.refresh_template().await {
                        warn!("template poll (post-longpoll timeout) failed: {err:?}");
                    }
                }
            }
        }
    }

    async fn zmq_loop(&self, zmq_urls: Vec<String>) {
        let engine = self.clone();
        tokio::task::spawn_blocking(move || {
            loop {
                eprintln!(
                    "blackhole-pool init: starting ZMQ block subscription setup ({} endpoint(s))",
                    zmq_urls.len()
                );
                info!(
                    "starting ZMQ block subscription setup ({} endpoint(s))",
                    zmq_urls.len()
                );
                let ctx = zmq::Context::new();
                let socket = match ctx.socket(zmq::SUB) {
                    Ok(sock) => sock,
                    Err(err) => {
                        warn!("ZMQ block socket create failed: {err:?}");
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };
                // Connect to ALL provided endpoints (Umbrel exposes hashblock on 28334,
                // rawblock on 28332). Connecting to multiple endpoints on one socket means
                // whichever fires first triggers the refresh — lower latency, better fallback.
                let mut connected = false;
                for url in &zmq_urls {
                    match socket.connect(url) {
                        Ok(_) => {
                            info!("ZMQ block connected: {url}");
                            engine.zmq_block_connected.store(true, Ordering::Relaxed);
                            connected = true;
                            // Don't break — connect to all URLs for multi-topic coverage.
                        }
                        Err(err) => warn!("ZMQ block connect failed ({url}): {err:?}"),
                    }
                }
                if !connected {
                    engine.zmq_block_connected.store(false, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                // Subscribe to both topics: hashblock arrives on port 28334,
                // rawblock arrives on port 28332 (per Umbrel Bitcoin Core config).
                // Each endpoint only publishes its own topic; subscribing to both
                // on a multi-connected socket guarantees coverage regardless of which
                // port(s) are reachable.
                if let Err(err) = socket.set_subscribe(b"hashblock") {
                    warn!("ZMQ block subscribe failed (hashblock): {err:?}");
                    engine.zmq_block_connected.store(false, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                if let Err(err) = socket.set_subscribe(b"rawblock") {
                    warn!("ZMQ block subscribe failed (rawblock): {err:?}");
                    engine.zmq_block_connected.store(false, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                eprintln!("blackhole-pool init: ZMQ block subscribed to hashblock/rawblock");
                info!("ZMQ block subscribed to hashblock/rawblock");

                loop {
                    // Bitcoin Core ZMQ notifications are multipart:
                    //   [topic][body][sequence]
                    // The sequence frame was added in Bitcoin Core 0.13 and is always the last
                    // element. If we don't drain it, the next recv will get misaligned.
                    // See: https://raw.githubusercontent.com/bitcoin/bitcoin/master/doc/zmq.md

                    // Read the topic frame first so we know what arrived.
                    let topic_msg = match socket.recv_msg(0) {
                        Ok(m) => m,
                        Err(e) => {
                            warn!("ZMQ block recv failed (topic frame): {e:?}; reconnecting");
                            engine.zmq_block_connected.store(false, Ordering::Relaxed);
                            break;
                        }
                    };

                    // Identify whether this is a block notification BEFORE reading more frames.
                    // This way, even if the connection drops mid-message, we can still fire
                    // a template refresh — preventing a missed block during ZMQ instability.
                    let is_block = topic_msg
                        .as_str()
                        .map_or(false, |t| t == "hashblock" || t == "rawblock");

                    // Read the body frame.
                    if socket.recv_msg(0).is_err() {
                        // Connection dropped between topic and body.
                        // If it was a block topic, fire immediately before reconnecting
                        // so we don't miss the block notification.
                        if is_block {
                            warn!("ZMQ block body recv failed — firing refresh before reconnect to avoid missed block");
                            let engine = engine.clone();
                            engine.counters.inc_zmq_block_received();
                            if engine.should_fire_block_zmq_trigger() {
                                engine.counters.inc_zmq_blocks_detected();
                    tokio::spawn(async move {
                        if let Err(e) = engine.refresh_template().await {
                            warn!("ZMQ block-miss refresh failed: {e:?} — longpoll will recover");
                        }
                    });
                            }
                        } else {
                            warn!("ZMQ block recv failed (body frame); reconnecting");
                        }
                        engine.zmq_block_connected.store(false, Ordering::Relaxed);
                        break;
                    }

                    // Drain optional extra frames (sequence number) for compatibility.
                    while socket.get_rcvmore().unwrap_or(false) {
                        if socket.recv_msg(0).is_err() {
                            warn!("ZMQ block recv failed while draining multipart; reconnecting");
                            break;
                        }
                    }

                    if is_block {
                        let engine = engine.clone();
                        engine.counters.inc_zmq_block_received();
                        if engine.should_fire_block_zmq_trigger() {
                            // A unique new block passed the debounce — count it once.
                            engine.counters.inc_zmq_blocks_detected();
                            tokio::spawn(async move {
                                if let Err(e) = engine.refresh_template().await {
                                    warn!("ZMQ block refresh failed: {e:?} — longpoll will recover");
                                }
                            });
                        }
                    }
                    // Non-block topics (e.g. stray subscriptions) are silently drained.
                }

                // Brief pause before reconnecting to avoid hammering Bitcoin Core.
                warn!("ZMQ block socket lost — reconnecting in 1s (longpoll fallback is active)");
                engine.zmq_block_connected.store(false, Ordering::Relaxed);
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .await
        .ok();
    }

    async fn zmq_tx_loop(&self, zmq_urls: Vec<String>) {
        let engine = self.clone();
        tokio::task::spawn_blocking(move || {
            loop {
                eprintln!(
                    "blackhole-pool init: starting ZMQ tx subscription setup ({} endpoint(s))",
                    zmq_urls.len()
                );
                info!(
                    "starting ZMQ tx subscription setup ({} endpoint(s))",
                    zmq_urls.len()
                );
                let ctx = zmq::Context::new();
                let socket = match ctx.socket(zmq::SUB) {
                    Ok(sock) => sock,
                    Err(err) => {
                        warn!("ZMQ tx socket create failed: {err:?}");
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };
                let mut connected = false;
                for url in &zmq_urls {
                    match socket.connect(url) {
                        Ok(_) => {
                            info!("ZMQ tx connected: {url}");
                            engine.zmq_tx_connected.store(true, Ordering::Relaxed);
                            connected = true;
                            // Connect to ALL provided endpoints for full coverage.
                        }
                        Err(err) => warn!("ZMQ tx connect failed ({url}): {err:?}"),
                    }
                }
                if !connected {
                    engine.zmq_tx_connected.store(false, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                // Bitcoin Core topics: rawtx / hashtx. We'll subscribe to hashtx (small payload).
                if let Err(err) = socket.set_subscribe(b"hashtx") {
                    warn!("ZMQ tx subscribe failed (hashtx): {err:?}");
                    engine.zmq_tx_connected.store(false, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                if let Err(err) = socket.set_subscribe(b"rawtx") {
                    warn!("ZMQ tx subscribe failed (rawtx): {err:?}");
                    engine.zmq_tx_connected.store(false, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                eprintln!("blackhole-pool init: ZMQ tx subscribed to hashtx/rawtx");
                info!("ZMQ tx subscribed to hashtx/rawtx");

                loop {
                    // Bitcoin Core ZMQ notifications are multipart:
                    //   [topic][body][sequence]
                    // Drain the sequence frame to keep the stream aligned.
                    if socket.recv_msg(0).is_err() || socket.recv_msg(0).is_err() {
                        warn!("ZMQ tx recv failed; reconnecting");
                        engine.zmq_tx_connected.store(false, Ordering::Relaxed);
                        break;
                    }
                    while socket.get_rcvmore().unwrap_or(false) {
                        if socket.recv_msg(0).is_err() {
                            warn!("ZMQ tx recv failed while draining multipart; reconnecting");
                            engine.zmq_tx_connected.store(false, Ordering::Relaxed);
                            break;
                        }
                    }

                    let engine = engine.clone();
                    match engine.zmq_tx_action() {
                        TxAction::Fire => {
                            engine.counters.inc_zmq_tx_triggered();
                            tokio::spawn(async move {
                                if let Err(e) = engine.refresh_template().await {
                                    warn!("ZMQ tx refresh failed: {e:?} — longpoll will recover");
                                }
                            });
                        }
                        TxAction::Debounced => {
                            engine.counters.inc_zmq_tx_debounced();
                        }
                        TxAction::PostBlockSuppressed => {
                            engine.counters.inc_zmq_tx_post_block_suppressed();
                        }
                    }
                }

                engine.zmq_tx_connected.store(false, Ordering::Relaxed);
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .await
        .ok();
    }
    /// Normal refresh: plain GBT call (no longpollid).
    /// Used by ZMQ triggers, the initial fetch, and the longpoll safety-net timeout.
    async fn refresh_template(&self) -> anyhow::Result<()> {
        self.refresh_template_fetch(None).await
    }

    /// Process a pre-fetched `GbtResponse`: compute dedup key, build job if
    /// content changed, broadcast to all miners.
    ///
    /// This is the single canonical "apply" path used by:
    ///   • `refresh_template_fetch` (ZMQ / timer path — fetches GBT then calls this)
    ///   • `poll_loop` (longpoll path — owns its own blocking fetch, then calls this)
    ///
    /// Acquiring the single-flight semaphore here ensures the ZMQ and longpoll paths
    /// never build two jobs concurrently.  If the semaphore is taken, we mark
    /// `refresh_pending` and the in-flight refresh will run another pass.
    async fn apply_gbt_response(&self, gbt: GbtResponse) -> anyhow::Result<()> {
        let permit = match self.refresh_sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                // Another fetch is in flight (ZMQ burst or concurrent poll).
                // Mark pending so the in-flight refresh re-runs after finishing.
                self.refresh_pending.store(true, Ordering::Relaxed);
                return Ok(());
            }
        };

        let _ = self.refresh_pending.swap(false, Ordering::Relaxed);

        // ── Template dedupe key (content-based) ─────────────────────────────
        // Properties:
        //   • Same content from two sources (ZMQ_TX + timer) → same key → skip
        //   • Any tx-set change (RBF, add, remove, reorder) → different txid_root
        //     → different key → new job broadcast with clean_jobs=false
        //   • New block → different prevhash → different key → clean_jobs=true
        //
        // Components: prevhash | bits | version | longpollid | txid_partial_root
        // Excluded: ntime/curtime (handled by JOB_REFRESH_MS timer separately)
        let txid_root = compute_txid_partial_root(&gbt.transactions);
        let key = format!(
            "{}:{}:{}:{}:{}",
            gbt.previousblockhash,
            gbt.bits,
            gbt.version,
            gbt.longpollid.clone().unwrap_or_default(),
            txid_root,
        );

        // Persist the new longpollid so the next poll_loop iteration passes it.
        if let Some(ref lpid) = gbt.longpollid {
            let mut guard = self.last_longpollid.lock().await;
            *guard = Some(lpid.clone());
        }

        let key_guard = self.last_template_key.lock().await;
        if key_guard.as_deref() == Some(&key) {
            // Same content seen before: two sources raced for the same template.
            self.counters.inc_notify_deduped();
            drop(key_guard);
            drop(permit);
            return Ok(());
        }
        drop(key_guard);

        // Build job first; only commit the key after a successful build so a
        // transient failure doesn't "poison" the key and stall future updates.
        let job = self.build_job(gbt, key.clone()).await?;
        let job_arc = Arc::new(job);

        // Solo hunter hot path: wake Stratum notify tasks before metrics/logging.
        // Metrics stay sequential for deterministic dashboard scope rotation.
        *self.last_template_key.lock().await = Some(key);
        self.sender.send_replace(job_arc.clone());

        self.metrics.set_template_scope(&job_arc).await;
        info!(
            "new template: height={} txs={} coinbase_value={} sat nbits={}",
            job_arc.height, job_arc.transactions.len(), job_arc.coinbase_value, job_arc.nbits
        );

        drop(permit);
        Ok(())
    }

    /// ZMQ / timer refresh path: fetch a fresh GBT then apply it.
    ///
    /// Single-flight: the semaphore inside `apply_gbt_response` serialises builds.
    /// If a trigger arrives mid-refresh, `refresh_pending` is set and we loop.
    async fn refresh_template_fetch(&self, longpollid: Option<&str>) -> anyhow::Result<()> {
        loop {
            let gbt_params = match longpollid {
                Some(id) if !id.is_empty() => json!([{"rules": ["segwit"], "longpollid": id}]),
                _ => json!([{"rules": ["segwit"]}]),
            };
            let gbt: GbtResponse = match self.rpc.call("getblocktemplate", gbt_params).await {
                Ok(gbt) => gbt,
                Err(err) => {
                    self.note_template_refresh_failure();
                    return Err(err.into());
                }
            };
            if let Err(err) = self.apply_gbt_response(gbt).await {
                self.note_template_refresh_failure();
                return Err(err);
            }
            self.note_template_refresh_success();

            // ZMQ path: re-run if another trigger arrived while we were fetching.
            // Longpoll path: break — Core already blocked until content changed.
            if longpollid.is_some() {
                break;
            }
            if !self.refresh_pending.swap(false, Ordering::Relaxed) {
                break;
            }
        }
        Ok(())
    }

    async fn build_job(&self, gbt: GbtResponse, template_key: String) -> anyhow::Result<JobTemplate> {
        // If the node provides a default witness commitment (SegWit), compute our own commitment
        // to guarantee it matches the witness reserved value we embed in the coinbase witness (32 zero bytes).
        let computed_witness_commitment = if let Some(ref core_commitment) = gbt.default_witness_commitment {
            let our_commitment = compute_witness_commitment_script_hex(&gbt.transactions)?;
            // Verify our computed commitment matches what Bitcoin Core provided.
            // A mismatch would cause block rejection at submitblock time.
            // Fall back to Bitcoin Core's known-good value to guarantee block validity.
            if our_commitment != *core_commitment {
                warn!(
                    "witness commitment mismatch: ours={} core={} — using Core's value as fallback",
                    our_commitment, core_commitment
                );
                Some(core_commitment.clone())
            } else {
                Some(our_commitment)
            }
        } else {
            None
        };

        let coinbase_parts = self.build_coinbase_parts(
            gbt.coinbasevalue,
            gbt.height,
            gbt.coinbaseaux.as_ref().and_then(|aux| aux.flags.as_deref()),
            computed_witness_commitment.as_deref(),
        )?;
        let (coinbase1, coinbase2, coinbase_tx, has_witness_commitment) = coinbase_parts;

        let coinbase_hash = double_sha256(&coinbase_tx);

        let mut tx_hashes = Vec::with_capacity(gbt.transactions.len());
        let mut tx_data = Vec::with_capacity(gbt.transactions.len());

        for tx in &gbt.transactions {
            // BIP141: block header merkle root MUST use TXIDs (non-witness).
            // Never fall back to hash (wtxid) — that would produce the wrong merkle
            // root and Bitcoin Core would reject the block as invalid.
            let txid = tx
                .txid
                .as_ref()
                .ok_or_else(|| anyhow!(
                    "getblocktemplate tx missing `txid` field — required for block header merkle. \
                     Do NOT use `hash` (wtxid) here."
                ))?;
            let mut hash = hex::decode(txid)?;
            hash.reverse();
            let hash_bytes: [u8; 32] = hash
                .try_into()
                .map_err(|_| anyhow!("invalid txid length"))?;
            tx_hashes.push(hash_bytes);
            tx_data.push(tx.data.clone());
        }

        let merkle_branches_le = build_merkle_branches(coinbase_hash, &tx_hashes);
        let merkle_branches: Vec<String> = merkle_branches_le.iter().map(|b| hex::encode(b)).collect();

        // Compute the partial Merkle root of non-coinbase txids (big-endian hex).
        // Used in submitblock repro bundle for post-mortem debugging.
        let txid_partial_root = if tx_hashes.is_empty() {
            "0".repeat(64)
        } else {
            let mut h = tx_hashes.clone();
            while h.len() > 1 {
                if h.len() % 2 == 1 { h.push(*h.last().unwrap()); }
                h = h.chunks(2).map(|p| merkle_step(&p[0], &p[1])).collect();
            }
            let mut r = h[0];
            r.reverse();
            hex::encode(r)
        };

        let prevhash_bytes = hex::decode(&gbt.previousblockhash)?;
        let prevhash_stratum = swap_words(&prevhash_bytes)?;
        let prevhash_le = to_little_endian(&prevhash_bytes)?;

        let version = format!("{:08x}", gbt.version);
        let ntime = format!("{:08x}", gbt.curtime as u32);
        // If mintime is absent (older nodes / unusual configs), fall back to curtime.
        let mintime_u32 = gbt.mintime.unwrap_or(gbt.curtime) as u32;
        let nbits = gbt.bits;

        // Pre-decode fields for fast share validation (little-endian bytes)
        let mut prevhash_le_vec = prevhash_bytes.clone();
        prevhash_le_vec.reverse();
        let prevhash_le_bytes: [u8; 32] = prevhash_le_vec
            .try_into()
            .map_err(|_| anyhow!("prevhash len != 32"))?;

        // gbt.target is hex big-endian; convert to 32-byte little-endian
        let mut target_be = hex::decode(&gbt.target).context("decode target")?;
        if target_be.len() > 32 {
            target_be = target_be[target_be.len() - 32..].to_vec();
        }
        let mut target_be_padded = vec![0u8; 32 - target_be.len()];
        target_be_padded.extend_from_slice(&target_be);
        target_be_padded.reverse();
        let target_le: [u8; 32] = target_be_padded
            .try_into()
            .map_err(|_| anyhow!("target len != 32"))?;

        let coinbase1_bytes = hex::decode(&coinbase1).context("decode coinbase1")?;
        let coinbase2_bytes = hex::decode(&coinbase2).context("decode coinbase2")?;

        let version_u32 = gbt.version;
        let nbits_u32 = u32::from_str_radix(&nbits, 16).context("parse nbits")?;

        // Network difficulty — same formula as Public Pool's calculateNetworkDifficulty()
        // and solostratum's network_difficulty calculation.
        // diff = (0xFFFF × 2^208) / expanded_target
        let network_difficulty = {
            let mantissa = (nbits_u32 & 0x007F_FFFF) as f64;
            let exponent = ((nbits_u32 >> 24) & 0xFF) as i32;
            let target_val = mantissa * 256.0f64.powi(exponent - 3);
            let diff1 = 65535.0 * 2.0f64.powi(208); // 0xFFFF × 2^208
            if target_val > 0.0 { diff1 / target_val } else { 0.0 }
        };

        let job_id = self.job_counter.fetch_add(1, Ordering::SeqCst).to_string();

        Ok(JobTemplate {
            ready: true,
            job_id,
            prevhash: prevhash_stratum,
            prevhash_le,
            coinbase1,
            coinbase2,
            merkle_branches,
            version,
            nbits,
            ntime,
            target: gbt.target,
            height: gbt.height,
            transactions: tx_data,
            has_witness_commitment,
            coinbase_value: gbt.coinbasevalue,
            created_at: Utc::now(),
            prevhash_le_bytes,
            target_le,
            coinbase1_bytes,
            coinbase2_bytes,
            merkle_branches_le,
            version_u32,
            nbits_u32,
            mintime_u32,
            network_difficulty,
            template_key,
            txid_partial_root,
            witness_commitment_script: computed_witness_commitment,
        })
    }

    fn build_coinbase_parts(
        &self,
        coinbase_value: u64,
        height: u64,
        coinbase_flags: Option<&str>,
        witness_commitment_hex: Option<&str>,
    ) -> anyhow::Result<(String, String, Vec<u8>, bool)> {
        let script_pubkey = if let Some(script_hex) = &self.config.payout_script_hex {
            // Explicit raw script — highest priority.
            hex::decode(script_hex).context("decode payout script")?
        } else if !self.config.payout_address.is_empty() {
            // Fallback address configured — parse and use.
            let address = Address::from_str(&self.config.payout_address)
                .context("parse payout address")?
                .require_network(self.config.network)
                .context("payout address network mismatch")?;
            address.script_pubkey().as_bytes().to_vec()
        } else {
            // No pool-wide payout address set.
            // Every miner MUST provide their own Bitcoin address as Stratum username.
            // This OP_RETURN placeholder is used only in the base template; it is
            // always overridden by build_coinbase2_for_payout() for each miner session
            // that has a valid per-miner address.  A miner without an address will be
            // rejected in handle_authorize() before any job is sent.
            vec![0x6a] // OP_RETURN (provably unspendable — never reaches a real block)
        };

        let mut script_sig = Vec::new();
        // BIP34: push the block height as a minimally-encoded CScript integer.
        // Bitcoin Core's CScript::push_int64 encoding rules (from script.h):
        //   n == 0        → OP_0     = 0x00
        //   1 ≤ n ≤ 16   → OP_n     = 0x51 + (n-1)  [single-byte opcode]
        //   otherwise     → OP_PUSH(len) + encode_script_num(n)
        //
        // On mainnet height > 16 is always true so only the third branch ever
        // fires in production.  The first two branches are needed for regtest
        // where the first mined block is at height 1.
        let height_i64 = height as i64;
        if height_i64 == 0 {
            script_sig.push(0x00); // OP_0
        } else if height_i64 >= 1 && height_i64 <= 16 {
            script_sig.push(0x50 + height_i64 as u8); // OP_1 (0x51) … OP_16 (0x60)
        } else {
            let height_bytes = encode_script_num(height_i64);
            script_sig.push(height_bytes.len() as u8);
            script_sig.extend_from_slice(&height_bytes);
        }

        if let Some(flags_hex) = coinbase_flags {
            let flags = hex::decode(flags_hex).context("decode coinbaseaux flags")?;
            if !flags.is_empty() {
                script_sig.push(flags.len() as u8);
                script_sig.extend_from_slice(&flags);
            }
        }

        // NOTE: extranonce1 is provided per-connection by the Stratum server.
        // Here we only need stable placeholders to locate coinbase1/coinbase2 offsets.
        // Using random bytes adds overhead and can be confusing when debugging.
        let extranonce1 = vec![0u8; self.config.extranonce1_size];
        let extranonce2 = vec![0u8; self.config.extranonce2_size];

        let mut tag = self.config.pool_tag.clone().into_bytes();
        let pool_tag_normalized = self.config.pool_tag.trim().trim_matches('/');
        let coinbase_message = self.config.coinbase_message.trim().trim_matches('/');
        if !coinbase_message.is_empty() && coinbase_message != pool_tag_normalized {
            if !tag.is_empty() && !tag.ends_with(b"/") {
                tag.extend_from_slice(b"/");
            }
            tag.extend_from_slice(coinbase_message.as_bytes());
        }
        if !tag.is_empty() {
            script_sig.push(tag.len() as u8);
            script_sig.extend_from_slice(&tag);
        }

        // Bitcoin Core enforces a max coinbase scriptSig length of 100 bytes.
        let max_scriptsig = 100usize;
        let total_scriptsig = script_sig.len() + self.config.extranonce1_size + self.config.extranonce2_size;
        if total_scriptsig > max_scriptsig {
            return Err(anyhow!(
                "coinbase scriptSig too long: {} bytes (max {}). Shorten POOL_TAG or COINBASE_MESSAGE.",
                total_scriptsig,
                max_scriptsig
            ));
        }

        let coinbase1;
        let coinbase2;
        let mut coinbase_tx = Vec::new();

        let witness_commitment = if let Some(hex_str) = witness_commitment_hex {
            Some(hex::decode(hex_str).context("decode witness commitment")?)
        } else {
            None
        };
        let has_witness_commitment = witness_commitment.is_some();

        coinbase_tx.extend_from_slice(&2u32.to_le_bytes());
        encode_varint(1, &mut coinbase_tx);
        coinbase_tx.extend_from_slice(&[0u8; 32]);
        coinbase_tx.extend_from_slice(&0xffff_ffffu32.to_le_bytes());

        let mut script_sig_full = script_sig.clone();
        script_sig_full.extend_from_slice(&extranonce1);
        script_sig_full.extend_from_slice(&extranonce2);

        encode_varint(script_sig_full.len() as u64, &mut coinbase_tx);
        coinbase_tx.extend_from_slice(&script_sig_full);
        coinbase_tx.extend_from_slice(&0xffff_ffffu32.to_le_bytes());

        let output_count = if has_witness_commitment { 2 } else { 1 };
        encode_varint(output_count as u64, &mut coinbase_tx);
        coinbase_tx.extend_from_slice(&coinbase_value.to_le_bytes());
        encode_varint(script_pubkey.len() as u64, &mut coinbase_tx);
        coinbase_tx.extend_from_slice(&script_pubkey);
        if let Some(commitment) = witness_commitment {
            coinbase_tx.extend_from_slice(&0u64.to_le_bytes());
            encode_varint(commitment.len() as u64, &mut coinbase_tx);
            coinbase_tx.extend_from_slice(&commitment);
        }
        coinbase_tx.extend_from_slice(&0u32.to_le_bytes());

        // Compute the exact byte offsets of extranonce1/2 inside the serialized tx.
        // Layout: version(4) | vin_count(varint) | prevout(32) | prevout_n(4)
        //         | script_len(varint) | scriptSig | sequence(4) | ...
        let extra_start = script_sig.len();
        let extra_end = extra_start + self.config.extranonce1_size + self.config.extranonce2_size;

        let mut cursor = 0usize;
        cursor += 4; // version
        cursor += varint_len(1); // vin count
        cursor += 32 + 4; // prevout + index
        cursor += varint_len(script_sig_full.len() as u64); // script length
        let script_start = cursor;

        let prefix_end = script_start + extra_start;
        let suffix_start = script_start + extra_end;

        let prefix = hex::encode(&coinbase_tx[..prefix_end]);
        let suffix = hex::encode(&coinbase_tx[suffix_start..]);

        coinbase1 = prefix;
        coinbase2 = suffix;

        Ok((coinbase1, coinbase2, coinbase_tx, has_witness_commitment))
    }
}


fn compute_witness_commitment_script_hex(txs: &[GbtTx]) -> anyhow::Result<String> {
    // BIP141:
    // commitment = SHA256d(witness_merkle_root || witness_reserved_value)
    // witness_merkle_root is computed over wtxids, with the coinbase wtxid treated as 0x00..00.
    // witness_reserved_value is 32 bytes placed in the coinbase witness stack.
    let mut leaves: Vec<[u8; 32]> = Vec::with_capacity(txs.len() + 1);
    leaves.push([0u8; 32]); // coinbase wtxid

    for tx in txs {
        // BIP141: witness merkle root requires wtxids (the `hash` field from getblocktemplate),
        // NOT txids. Using txid here would produce a wrong commitment that Bitcoin Core rejects.
        let wtxid_hex = tx
            .hash
            .as_ref()
            .ok_or_else(|| anyhow!(
                "getblocktemplate tx missing `hash` (wtxid) field — \
                 cannot compute witness commitment. \
                 Ensure Bitcoin Core is running with segwit rules enabled."
            ))?;
        let mut b = hex::decode(wtxid_hex).context("decode wtxid")?;
        if b.len() != 32 {
            return Err(anyhow!("invalid wtxid length: expected 32 bytes, got {}", b.len()));
        }
        b.reverse(); // internal little-endian
        let arr: [u8; 32] = b.try_into().unwrap();
        leaves.push(arr);
    }

    // Merkle root over leaves (little-endian).
    let mut level = leaves;
    while level.len() > 1 {
        let mut next = Vec::with_capacity((level.len() + 1) / 2);
        for i in (0..level.len()).step_by(2) {
            let left = level[i];
            let right = if i + 1 < level.len() { level[i + 1] } else { left };
            next.push(merkle_step(&left, &right));
        }
        level = next;
    }
    let witness_merkle_root = level[0];

    let reserved = [0u8; 32];
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&witness_merkle_root);
    buf[32..].copy_from_slice(&reserved);
    let commitment = double_sha256(&buf);

    // ScriptPubKey: OP_RETURN (0x6a) PUSHDATA(36=0x24) 0xaa21a9ed <32-byte commitment>
    let mut script = Vec::with_capacity(38);
    script.push(0x6a);
    script.push(0x24);
    script.extend_from_slice(&[0xaa, 0x21, 0xa9, 0xed]);
    script.extend_from_slice(&commitment);

    Ok(hex::encode(script))
}

/// Map a Bitcoin Core submitblock rejection reason to a short diagnostic category.
///
/// Bitcoin Core reasons (from validation.cpp):
///   bad-txnmrklroot          TXID merkle root mismatch  ← check coinbase split/branch
///   bad-witness-merkle-match witness commitment mismatch ← check wtxid computation
///   bad-witness-nonce-size   coinbase witness item ≠ 32B
///   bad-cb-length            coinbase scriptSig > 100B
///   bad-prevblk              stale block (not best tip)  ← ZMQ too slow?
///   time-too-old             ntime < MTP                 ← check mintime
///   time-too-new             ntime > now+2h
///   bad-diffbits             wrong nbits
/// Compute the Merkle root of the non-coinbase transaction set for use in the
/// template dedup key.
///
/// Algorithm: standard Bitcoin double-SHA256 Merkle tree over the txids (as
/// little-endian bytes), same as the block header merkle but without the
/// coinbase leaf at position 0.
///
/// Properties that make this the ideal key component:
///   • Empty mempool → all-zeros root (constant, no spurious notifies)
///   • Any tx added / removed / replaced → different root → new notify
///   • Order change (unlikely in practice) → different root
///   • Two different tx sets with same count and total fee → different root
///
/// When prevhash is unchanged but txid_root changes, the job update carries
/// `clean_jobs=false` — miners finish the current nonce space before switching.
fn compute_txid_partial_root(transactions: &[GbtTx]) -> String {
    if transactions.is_empty() {
        return "0".repeat(64);
    }
    let mut hashes: Vec<[u8; 32]> = transactions
        .iter()
        .filter_map(|tx| {
            let txid = tx.txid.as_ref()?;
            let mut bytes = hex::decode(txid).ok()?;
            bytes.reverse(); // GBT txids are BE; Merkle tree uses LE bytes
            bytes.try_into().ok()
        })
        .collect();
    if hashes.is_empty() {
        return "0".repeat(64);
    }
    // Standard Bitcoin Merkle tree: duplicate last node if odd count, then pair.
    while hashes.len() > 1 {
        if hashes.len() % 2 == 1 {
            hashes.push(*hashes.last().unwrap());
        }
        hashes = hashes
            .chunks(2)
            .map(|pair| merkle_step(&pair[0], &pair[1]))
            .collect();
    }
    // Return as big-endian hex (conventional display format).
    let mut root = hashes[0];
    root.reverse();
    hex::encode(root)
}

fn categorise_reject_reason(reason: &str) -> &'static str {
    if reason.contains("mrklroot") || reason.contains("merkle") { "merkle" }
    else if reason.contains("witness")   { "witness"    }
    else if reason.contains("cb-")       { "coinbase"   }
    else if reason.contains("time")      { "time"       }
    else if reason.contains("prevblk")   { "stale"      }
    else if reason.contains("diffbits")  { "difficulty" }
    else                                 { "unknown"    }
}

/// Public re-export for tests — see `build_merkle_branches`.
#[cfg(test)]
pub fn build_merkle_branches_pub(coinbase_hash: [u8; 32], tx_hashes: &[[u8; 32]]) -> Vec<[u8; 32]> {
    build_merkle_branches(coinbase_hash, tx_hashes)
}

/// Build Stratum merkle branches for the coinbase path.
///
/// In Stratum, the coinbase is always at index 0. At each level of the merkle
/// tree the branch is simply the sibling of the current node (index 0), i.e.
/// `level[1]` (or a duplicate of `level[0]` when the level has an odd length).
/// The miner concatenates `coinbase_hash || branch[0]`, hashes, takes
/// `result || branch[1]`, hashes … until the root is reached.
fn build_merkle_branches(coinbase_hash: [u8; 32], tx_hashes: &[[u8; 32]]) -> Vec<[u8; 32]> {
    // level[0] is always the coinbase (or its combined ancestor).
    let mut level: Vec<[u8; 32]> = std::iter::once(coinbase_hash)
        .chain(tx_hashes.iter().copied())
        .collect();

    let mut branches = Vec::new();

    while level.len() > 1 {
        // Sibling of the coinbase path node at this level is always level[1].
        // (level[0] is the coinbase / running combined hash; we never push it as a branch.)
        branches.push(level[1]);

        let mut next = Vec::with_capacity((level.len() + 1) / 2);
        for i in (0..level.len()).step_by(2) {
            let left = level[i];
            let right = if i + 1 < level.len() { level[i + 1] } else { left };
            next.push(merkle_step(&left, &right));
        }
        level = next;
    }

    branches
}

fn swap_words(bytes: &[u8]) -> anyhow::Result<String> {
    if bytes.len() != 32 {
        return Err(anyhow!("prevhash must be 32 bytes"));
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        let start = i * 4;
        let src = 28 - (i * 4);
        out[start..start + 4].copy_from_slice(&bytes[src..src + 4]);
    }
    Ok(hex::encode(out))
}

fn to_little_endian(bytes: &[u8]) -> anyhow::Result<String> {
    if bytes.len() != 32 {
        return Err(anyhow!("prevhash must be 32 bytes"));
    }
    let mut out = bytes.to_vec();
    out.reverse();
    Ok(hex::encode(out))
}

fn encode_script_num(value: i64) -> Vec<u8> {
    if value == 0 {
        return vec![];
    }

    let mut result = Vec::new();
    let mut absvalue = value.unsigned_abs();
    while absvalue > 0 {
        result.push((absvalue & 0xff) as u8);
        absvalue >>= 8;
    }

    let is_negative = value < 0;
    if let Some(last) = result.last_mut() {
        if *last & 0x80 != 0 {
            result.push(if is_negative { 0x80 } else { 0x00 });
        } else if is_negative {
            *last |= 0x80;
        }
    }

    result
}

fn encode_varint(value: u64, out: &mut Vec<u8>) {
    match value {
        0..=0xfc => out.push(value as u8),
        0xfd..=0xffff => {
            out.push(0xfd);
            out.extend_from_slice(&(value as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xfe);
            out.extend_from_slice(&(value as u32).to_le_bytes());
        }
        _ => {
            out.push(0xff);
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

/// Build the `coinbase2` hex string for a specific payout script.
///
/// `coinbase2` is the part of the coinbase transaction that comes **after**
/// `extranonce2` — i.e. input sequence + all outputs + locktime.
///
/// Only the outputs (specifically output 0's scriptPubKey) depend on the
/// payout address. `coinbase1` is the same for all miners sharing the same
/// block template, so only this function needs to be called when a miner
/// connects with a different address.
///
/// Parameters:
///   `coinbase_value`         — block subsidy + fees from GBT (satoshis).
///   `payout_script`          — scriptPubKey bytes for the miner's address.
///   `witness_commitment_hex` — full witness commitment output script hex
///                              (6a24aa21a9ed…) as returned by `build_job`.
///                              `None` for non-SegWit templates (rare on mainnet).
pub fn build_coinbase2_for_payout(
    coinbase_value: u64,
    payout_script: &[u8],
    witness_commitment_hex: Option<&str>,
) -> anyhow::Result<(String, Vec<u8>)> {
    let mut out: Vec<u8> = Vec::new();

    // Input sequence (always 0xffffffff for coinbase).
    out.extend_from_slice(&0xffff_ffffu32.to_le_bytes());

    // Output count: 1 (payout only) or 2 (payout + witness commitment).
    let has_witness = witness_commitment_hex.is_some();
    encode_varint(if has_witness { 2 } else { 1 }, &mut out);

    // Output 0: block reward → miner's payout address.
    out.extend_from_slice(&coinbase_value.to_le_bytes());
    encode_varint(payout_script.len() as u64, &mut out);
    out.extend_from_slice(payout_script);

    // Output 1 (SegWit only): witness commitment (OP_RETURN + 0xaa21a9ed + hash).
    if let Some(hex_str) = witness_commitment_hex {
        let commitment_script = hex::decode(hex_str)
            .context("decode witness commitment hex for custom coinbase2")?;
        out.extend_from_slice(&0u64.to_le_bytes()); // value = 0
        encode_varint(commitment_script.len() as u64, &mut out);
        out.extend_from_slice(&commitment_script);
    }

    // Locktime (always 0 for coinbase).
    out.extend_from_slice(&0u32.to_le_bytes());

    Ok((hex::encode(&out), out))
}

#[inline]
fn varint_len(value: u64) -> usize {
    match value {
        0..=0xfc => 1,
        0xfd..=0xffff => 3,
        0x1_0000..=0xffff_ffff => 5,
        _ => 9,
    }
}
