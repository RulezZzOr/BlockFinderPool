use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde::Serialize;
use tracing::info;
use tower_http::cors::{Any, CorsLayer};

use crate::config::Config;
use crate::metrics::{MetricsStore, ShareEvent};
use crate::rpc::RpcClient;
use crate::storage::SqliteStore;
use crate::template::{JobTemplate, TemplateEngine};

#[derive(Clone)]
pub struct ApiServer {
    config: Config,
    metrics: MetricsStore,
    sqlite: SqliteStore,
    rpc: RpcClient,
    template_engine: Arc<TemplateEngine>,
    started_at: DateTime<Utc>,
}

impl ApiServer {
    pub fn new(
        config: Config,
        metrics: MetricsStore,
        sqlite: SqliteStore,
        rpc: RpcClient,
        template_engine: Arc<TemplateEngine>,
    ) -> Self {
        Self {
            config,
            metrics,
            sqlite,
            rpc,
            template_engine,
            started_at: Utc::now(),
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let state = ApiState {
            metrics: self.metrics.clone(),
            sqlite: self.sqlite.clone(),
            rpc: self.rpc.clone(),
            config: self.config.clone(),
            template_engine: self.template_engine.clone(),
            started_at: self.started_at,
        };

        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        let app = Router::new()
            .route("/health", get(health))
            .route("/metrics", get(metrics))
            .route("/miners", get(miners))
            .route("/shares", get(shares))
            .route("/hashrate", get(hashrate))
            .route("/blocks", get(blocks))
            .route("/pool", get(pool))
            .route("/network", get(network))
            .route("/sweetsolo/status", get(sweetsolo_status))
            .route("/sweetsolo/miners", get(sweetsolo_miners))
            .route("/sweetsolo/mempool", get(sweetsolo_mempool))
            .route("/sweetsolo/connection-status", get(sweetsolo_connection_status))
            .route("/sweetsolo/template-info", get(sweetsolo_template_info))
            .with_state(state)
            .layer(cors);

        let bind = format!("{}:{}", self.config.api_bind, self.config.api_port);
        info!("api listening on {bind}");
        let listener = tokio::net::TcpListener::bind(&bind).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct ApiState {
    metrics: MetricsStore,
    sqlite: SqliteStore,
    rpc: RpcClient,
    config: Config,
    template_engine: Arc<TemplateEngine>,
    started_at: DateTime<Utc>,
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

async fn metrics(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    Json(snapshot)
}

async fn miners(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    Json(snapshot.miners)
}

#[derive(Serialize)]
struct SharesResponse {
    total: u64,
    rejected: u64,
    blocks: u64,
    updated_at: String,
}

async fn shares(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    Json(SharesResponse {
        total: snapshot.total_shares,
        rejected: snapshot.total_rejected,
        blocks: snapshot.total_blocks,
        updated_at: snapshot.updated_at.to_rfc3339(),
    })
}

#[derive(Serialize)]
struct HashratePoint {
    timestamp: String,
    shares: usize,
}

#[derive(Serialize)]
struct HashrateResponse {
    total_hashrate_gh: f64,
    updated_at: String,
    recent: Vec<HashratePoint>,
}

async fn hashrate(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    let recent = build_share_series(state.metrics.recent_events(Duration::minutes(30)).await);

    Json(HashrateResponse {
        total_hashrate_gh: snapshot.total_hashrate_gh,
        updated_at: snapshot.updated_at.to_rfc3339(),
        recent,
    })
}

#[derive(Serialize)]
struct BlockRow {
    height: i64,
    hash: String,
    status: String,
}

async fn blocks(State(state): State<ApiState>) -> impl IntoResponse {
    let rows = match state.sqlite.fetch_blocks(50).await {
        Ok(rows) => rows,
        Err(_) => vec![],
    };

    let blocks = rows
        .into_iter()
        .map(|(height, hash, status)| BlockRow { height, hash, status })
        .collect::<Vec<_>>();
    Json(blocks)
}

#[derive(Serialize)]
struct PoolInfo {
    totalHashRate: f64,
    totalMiners: usize,
    blockHeight: u64,
    blocksFound: u64,
    fee: u64,
    /// ISO-8601 UTC timestamp when the pool process started.
    /// Use poolStartedAt to compute exact Δ windows between snapshots.
    poolStartedAt: String,
    /// Seconds since pool start. Use as the denominator for rate calculations:
    ///   rate/min  = counter / (uptimeSecs / 60)
    ///   rate/hour = counter / (uptimeSecs / 3600)
    uptimeSecs: u64,

    // ── Storm/health counters — ALL cumulative since pool start ──────────
    // For threshold comparisons always use the *Per* fields (normalized) or
    // compute Δ between two snapshots divided by Δ uptimeSecs.
    jobsSent: u64,
    cleanJobsSent: u64,
    /// Cumulative jobsSent / connectedMiners (snapshot-time normalization).
    jobsSentPerMiner: f64,
    /// jobsSent / (uptimeSecs/60) — average notifies per miner per minute
    /// since pool start.  Compare against threshold ≤ 6/10min = ≤ 0.6/min.
    jobsSentPerMinerPerMin: f64,
    // Notify suppression (three distinct reasons)
    notifyDeduped: u64,      // same template_key from two sources
    notifyRateLimited: u64,  // token bucket full (mempool-only floods)
    // zmqTxSuppressed = post-block 15s window (see zmqTxSuppressed in ZMQ section)
    duplicateShares: u64,
    reconnectsTotal: u64,
    submitblockAccepted: u64,
    submitblockRejected: u64,
    submitblockRpcFail: u64,
    versionRollingViolations: u64,
    // Stale classification
    stalesNewBlock: u64,
    stalesExpired: u64,
    stalesReconnect: u64,
    // ZMQ activity
    zmqBlockReceived: u64,
    zmqTxTriggered: u64,
    /// Suppressed by ZMQ_DEBOUNCE_MS (normal; always high — thousands/hour is fine).
    zmqTxDebounced: u64,
    /// Suppressed by post-block 15s window.
    /// Should correlate with zmqBlockReceived: ~N per block × mempool burst rate.
    /// If this is non-zero WITHOUT zmqBlockReceived growing → investigate.
    zmqTxPostBlockSuppressed: u64,
    /// staleRatio = (stalesNewBlock + stalesExpired + stalesReconnect)
    ///            / (accepted_shares + stale_shares)
    ///
    /// Denominator includes only accepted + stale (NOT invalid/duplicate rejections).
    /// A value of 0.05 means 5% of submitted work was timing-discarded.
    staleRatio: f64,
}

async fn pool(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    let total_hashrate = snapshot.total_hashrate_gh * 1_000_000_000.0;
    let total_miners = snapshot.miners.len();
    let blocks_found = snapshot.total_blocks;
    let block_height = fetch_mining_info(&state.rpc)
        .await
        .map(|info| info.blocks)
        .unwrap_or(0);
    let c = &state.metrics.counters;

    // ── staleRatio definition ────────────────────────────────────────────────
    // Numerator  : total stale submissions (all three reasons combined)
    //              = stalesNewBlock + stalesExpired + stalesReconnect
    //              (from atomic counters, same source as the counters in this response)
    //
    // Denominator: total *submissions* = accepted + stale
    //              (rejected/invalid shares are NOT included: they represent a
    //               different failure mode — wrong hash, duplicate nonce, etc.)
    //
    // Result: fraction of submitted work that was discarded due to timing (stale).
    //         staleRatio = 0.05 means 5 out of every 100 submitted shares were stale.
    //
    // Note: `m.shares` = accepted; `m.stale` = stale (both from MetricsStore::record_share
    //       and record_stale, which use the same per-worker MinerStats entry).
    let total_stales_all = c.stales_new_block() + c.stales_expired() + c.stales_reconnect();
    let total_accepted: u64 = snapshot.miners.iter().map(|m| m.shares).sum();
    let total_stale_from_stats: u64 = snapshot.miners.iter().map(|m| m.stale).sum();
    // Denominator = accepted + stale (excludes invalid/duplicate rejections)
    let total_submissions = total_accepted + total_stale_from_stats;
    let stale_ratio = if total_submissions > 0 {
        // Use per-miner stats (total_stale_from_stats) for denominator consistency,
        // and atomic counters (total_stales_all) for numerator to catch all reasons.
        // Both should match; the atomic counters are the authoritative source.
        (total_stales_all as f64 / total_submissions as f64).min(1.0)
    } else {
        0.0
    };

    let uptime_secs = (Utc::now() - state.metrics.started_at)
        .num_seconds()
        .max(1) as u64;
    let uptime_min = uptime_secs as f64 / 60.0;
    let miners_count = total_miners.max(1) as f64;
    let jobs_sent_per_miner = c.jobs_sent() as f64 / miners_count;
    let jobs_sent_per_miner_per_min = jobs_sent_per_miner / uptime_min;

    Json(PoolInfo {
        totalHashRate: total_hashrate,
        totalMiners: total_miners,
        blockHeight: block_height,
        blocksFound: blocks_found,
        fee: 0,
        poolStartedAt:          state.metrics.started_at.to_rfc3339(),
        uptimeSecs:             uptime_secs,
        jobsSent:               c.jobs_sent(),
        cleanJobsSent:          c.clean_jobs_sent(),
        jobsSentPerMiner:       jobs_sent_per_miner,
        jobsSentPerMinerPerMin: jobs_sent_per_miner_per_min,
        notifyDeduped:          c.notify_deduped(),
        notifyRateLimited:  c.notify_rate_limited(),
        duplicateShares:    c.duplicate_shares(),
        reconnectsTotal:      c.reconnects_total(),
        submitblockAccepted:  c.submitblock_accepted(),
        submitblockRejected:  c.submitblock_rejected(),
        submitblockRpcFail:   c.submitblock_rpc_fail(),
        versionRollingViolations: c.version_rolling_violations(),
        stalesNewBlock:       c.stales_new_block(),
        stalesExpired:        c.stales_expired(),
        stalesReconnect:      c.stales_reconnect(),
        zmqBlockReceived:          c.zmq_block_received(),
        zmqTxTriggered:            c.zmq_tx_triggered(),
        zmqTxDebounced:            c.zmq_tx_debounced(),
        zmqTxPostBlockSuppressed:  c.zmq_tx_post_block_suppressed(),
        staleRatio:           stale_ratio,
    })
}

#[derive(Serialize, Deserialize, Clone)]
struct MiningInfo {
    blocks: u64,
    difficulty: f64,
    networkhashps: Option<f64>,
}

async fn network(State(state): State<ApiState>) -> impl IntoResponse {
    let info = fetch_mining_info(&state.rpc).await.ok();
    Json(info)
}

#[derive(Serialize)]
struct SweetSoloStatus {
    name: &'static str,
    poolIdentifier: String,
    network: String,
    internalPayoutMode: bool,
    payoutAddressConfigured: bool,
    payoutAddress: Option<String>,
    apiPort: u16,
    stratumPort: u16,
    templateRefreshIntervalMs: u64,
    vardiff: SweetSoloVardiff,
    uptime: DateTime<Utc>,
}

#[derive(Serialize)]
struct SweetSoloVardiff {
    targetShareTimeSec: f64,
    checkIntervalSec: f64,
    minDifficulty: f64,
    maxDifficulty: f64,
}

async fn sweetsolo_status(State(state): State<ApiState>) -> impl IntoResponse {
    let payout = state.config.payout_address.trim().to_string();
    let payout_configured = !payout.is_empty();
    let payout_address = if payout_configured { Some(payout) } else { None };
    let network = match state.config.network {
        bitcoin::Network::Bitcoin => "mainnet",
        bitcoin::Network::Testnet => "testnet",
        bitcoin::Network::Signet => "signet",
        bitcoin::Network::Regtest => "regtest",
        _ => "unknown",
    }
    .to_string();

    Json(SweetSoloStatus {
        name: "Solo",
        poolIdentifier: state.config.pool_tag.clone(),
        network,
        internalPayoutMode: false,
        payoutAddressConfigured: payout_configured,
        payoutAddress: payout_address,
        apiPort: state.config.api_port,
        stratumPort: state.config.stratum_port,
        templateRefreshIntervalMs: state.config.template_poll_ms,
        vardiff: SweetSoloVardiff {
            targetShareTimeSec: state.config.target_share_time_secs,
            checkIntervalSec: state.config.vardiff_retarget_time_secs,
            minDifficulty: state.config.min_difficulty,
            maxDifficulty: state.config.max_difficulty,
        },
        uptime: state.started_at,
    })
}

#[derive(Serialize)]
struct SweetSoloMinerList {
    address: String,
    bestDifficulty: f64,
    workers: Vec<SweetSoloWorker>,
}

#[derive(Serialize)]
struct SweetSoloWorker {
    sessionId: Option<String>,
    name: String,
    userAgent: Option<String>,
    bestDifficulty: f64,
    hashRate: f64,
    startTime: Option<String>,
    lastSeen: String,
}

async fn sweetsolo_miners(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    let best = snapshot
        .miners
        .iter()
        .map(|m| m.best_submitted_difficulty)
        .fold(0.0, f64::max);

    let mut workers = snapshot
        .miners
        .into_iter()
        .map(|miner| SweetSoloWorker {
            sessionId: miner.session_id,
            name: miner.worker,
            userAgent: miner.user_agent,
            bestDifficulty: miner.best_submitted_difficulty,
            hashRate: miner.hashrate_gh * 1_000_000_000.0,
            startTime: None,
            lastSeen: miner.last_seen.to_rfc3339(),
        })
        .collect::<Vec<_>>();

    workers.sort_by(|a, b| b.hashRate.partial_cmp(&a.hashRate).unwrap_or(std::cmp::Ordering::Equal));

    Json(SweetSoloMinerList {
        address: state.config.payout_address.clone(),
        bestDifficulty: best,
        workers,
    })
}

async fn sweetsolo_mempool(State(state): State<ApiState>) -> impl IntoResponse {
    let result: serde_json::Value = match state.rpc.call("getmempoolinfo", serde_json::json!([])).await {
        Ok(val) => val,
        Err(_) => serde_json::json!({}),
    };
    Json(result)
}

#[derive(Serialize)]
struct RpcStatus {
    connected: bool,
    latency: Option<u64>,
    url: String,
    port: Option<u16>,
}

#[derive(Serialize)]
struct ZmqStatus {
    enabled: bool,
    host: String,
}

#[derive(Serialize)]
struct StratumStatus {
    port: u16,
    connected: bool,
}

#[derive(Serialize)]
struct ConnectionStatus {
    bitcoinRpc: RpcStatus,
    zmq: ZmqStatus,
    stratum: StratumStatus,
}

async fn sweetsolo_connection_status(State(state): State<ApiState>) -> impl IntoResponse {
    let start = std::time::Instant::now();
    let connected = fetch_mining_info(&state.rpc).await.is_ok();
    let latency = if connected { Some(start.elapsed().as_millis() as u64) } else { None };
    let port = parse_port(&state.config.rpc_url);

    Json(ConnectionStatus {
        bitcoinRpc: RpcStatus {
            connected,
            latency,
            url: state.config.rpc_url.clone(),
            port,
        },
        zmq: ZmqStatus {
            enabled: !state.config.zmq_block_urls.is_empty(),
            host: if state.config.zmq_block_urls.is_empty() {
                "Not configured".to_string()
            } else {
                state.config.zmq_block_urls.join(",")
            },
        },
        stratum: StratumStatus {
            port: state.config.stratum_port,
            connected: true,
        },
    })
}

#[derive(Serialize)]
struct TemplateInfo {
    height: u64,
    version: u32,
    bits: String,
    previousblockhash: String,
    coinbasevalue: u64,
    mintime: u64,
    curtime: u64,
    transactions: usize,
    target: String,
    job_id: String,
    created_at: String,
}

async fn sweetsolo_template_info(State(state): State<ApiState>) -> impl IntoResponse {
    // Read the last cached template from TemplateEngine — zero extra RPC calls.
    let job: Arc<JobTemplate> = state.template_engine.subscribe().borrow().clone();
    let info = if job.ready {
        let ntime_u32 = u32::from_str_radix(job.ntime.trim_start_matches("0x"), 16).unwrap_or(0);
        Some(TemplateInfo {
            height: job.height,
            version: job.version_u32,
            bits: job.nbits.clone(),
            previousblockhash: job.prevhash_le.clone(),
            coinbasevalue: job.coinbase_value,
            mintime: job.mintime_u32 as u64,
            curtime: ntime_u32 as u64,
            transactions: job.transactions.len(),
            target: job.target.clone(),
            job_id: job.job_id.clone(),
            created_at: job.created_at.to_rfc3339(),
        })
    } else {
        None
    };

    Json(info)
}

fn build_share_series(events: Vec<ShareEvent>) -> Vec<HashratePoint> {
    let mut buckets = std::collections::BTreeMap::new();
    for event in events {
        let key = event.created_at.format("%H:%M").to_string();
        *buckets.entry(key).or_insert(0usize) += 1;
    }
    buckets
        .into_iter()
        .map(|(timestamp, shares)| HashratePoint { timestamp, shares })
        .collect()
}

async fn fetch_mining_info(rpc: &RpcClient) -> anyhow::Result<MiningInfo> {
    rpc.call("getmininginfo", serde_json::json!([])).await
}

fn parse_port(url: &str) -> Option<u16> {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    host_port.split(':').nth(1)?.parse().ok()
}
