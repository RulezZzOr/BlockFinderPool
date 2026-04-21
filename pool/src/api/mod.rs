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

use crate::build_info::{self, BuildInfo};
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
            .route("/build-info", get(build_info_endpoint))
            .route("/metrics", get(metrics))
            .route("/miners", get(miners))
            .route("/shares", get(shares))
            .route("/hashrate", get(hashrate))
            .route("/blocks", get(blocks))
            .route("/pool", get(pool))
            .route("/network", get(network))
            .route("/blackhole/status",            get(blackhole_status))
            .route("/blackhole/miners",            get(blackhole_miners))
            .route("/blackhole/mempool",           get(blackhole_mempool))
            .route("/blackhole/connection-status", get(blackhole_connection_status))
            .route("/blackhole/template-info",     get(blackhole_template_info))
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
    Json(HealthResponse {
        ok: true,
        build: build_info::current(),
    })
}

#[derive(Serialize)]
struct HealthResponse {
    ok: bool,
    build: BuildInfo,
}

async fn build_info_endpoint() -> impl IntoResponse {
    Json(build_info::current())
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
    found_by: Option<String>,
    status: String,
    created_at: String,
}

async fn blocks(State(state): State<ApiState>) -> impl IntoResponse {
    let rows = match state.sqlite.fetch_blocks(50).await {
        Ok(rows) => rows,
        Err(_) => vec![],
    };

    let blocks = rows
        .into_iter()
        .map(|(height, hash, found_by, status, created_at)| BlockRow {
            height,
            hash,
            found_by,
            status,
            created_at,
        })
        .collect::<Vec<_>>();
    Json(blocks)
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct PoolInfo {
    totalHashRate: f64,
    totalMiners: usize,
    blockHeight: u64,
    blocksFound: u64,
    fee: u64,
    poolStartedAt: String,
    uptimeSecs: u64,
    jobsSent: u64,
    cleanJobsSent: u64,
    jobsSentPerMiner: f64,
    jobsSentPerMinerPerMin: f64,
    notifyDeduped: u64,
    notifyRateLimited: u64,
    duplicateShares: u64,
    reconnectsTotal: u64,
    submitblockAccepted: u64,
    submitblockRejected: u64,
    submitblockRpcFail: u64,
    versionRollingViolations: u64,
    stalesNewBlock: u64,
    stalesExpired: u64,
    stalesReconnect: u64,
    zmqBlocksDetected: u64,
    zmqBlockNotifications: u64,
    zmqTxTriggered: u64,
    zmqTxDebounced: u64,
    zmqTxPostBlockSuppressed: u64,
    staleRatio: f64,
    /// Bitcoin Core current block height (from getmininginfo).
    /// Reused to populate /network data — no second RPC call needed.
    networkDifficulty: f64,
    networkHashps: f64,
}

async fn pool(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    let total_hashrate = snapshot.total_hashrate_gh * 1_000_000_000.0;
    let total_miners = snapshot.miners.len();
    let blocks_found = snapshot.total_blocks;
    // One call; fields are reused for networkDifficulty/networkHashps below.
    let mining_info = fetch_mining_info(&state.rpc).await.ok();
    let block_height = mining_info.as_ref().map(|i| i.blocks).unwrap_or(0);
    let c = &state.metrics.counters;

    let total_stales_all = c.stales_new_block() + c.stales_expired() + c.stales_reconnect();
    let total_accepted: u64 = snapshot.miners.iter().map(|m| m.shares).sum();
    let total_stale_from_stats: u64 = snapshot.miners.iter().map(|m| m.stale).sum();
    let total_submissions = total_accepted + total_stale_from_stats;
    let stale_ratio = if total_submissions > 0 {
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
        notifyRateLimited:      c.notify_rate_limited(),
        duplicateShares:        c.duplicate_shares(),
        reconnectsTotal:        c.reconnects_total(),
        submitblockAccepted:    c.submitblock_accepted(),
        submitblockRejected:    c.submitblock_rejected(),
        submitblockRpcFail:     c.submitblock_rpc_fail(),
        versionRollingViolations: c.version_rolling_violations(),
        stalesNewBlock:         c.stales_new_block(),
        stalesExpired:          c.stales_expired(),
        stalesReconnect:        c.stales_reconnect(),
        zmqBlocksDetected:          c.zmq_blocks_detected(),
        zmqBlockNotifications:      c.zmq_block_received(),
        zmqTxTriggered:             c.zmq_tx_triggered(),
        zmqTxDebounced:             c.zmq_tx_debounced(),
        zmqTxPostBlockSuppressed:   c.zmq_tx_post_block_suppressed(),
        staleRatio:             stale_ratio,
        networkDifficulty: mining_info.as_ref().map(|i| i.difficulty).unwrap_or(0.0),
        networkHashps:     mining_info.as_ref().and_then(|i| i.networkhashps).unwrap_or(0.0),
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
#[allow(non_snake_case)]
struct BlackHoleStatus {
    name: &'static str,
    poolIdentifier: String,
    network: String,
    internalPayoutMode: bool,
    payoutAddressConfigured: bool,
    payoutAddress: Option<String>,
    apiPort: u16,
    stratumPort: u16,
    templateRefreshIntervalMs: u64,
    vardiff: BlackHoleVardiff,
    uptime: DateTime<Utc>,
    build: BuildInfo,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct BlackHoleVardiff {
    targetShareTimeSec: f64,
    checkIntervalSec: f64,
    minDifficulty: f64,
    maxDifficulty: f64,
}

async fn blackhole_status(State(state): State<ApiState>) -> impl IntoResponse {
    let payout = state.config.payout_address.trim().to_string();
    let payout_configured = !payout.is_empty();
    let payout_address = if payout_configured { Some(payout) } else { None };
    let network = match state.config.network {
        bitcoin::Network::Bitcoin  => "mainnet",
        bitcoin::Network::Testnet  => "testnet",
        bitcoin::Network::Signet   => "signet",
        bitcoin::Network::Regtest  => "regtest",
        _ => "unknown",
    }
    .to_string();

    Json(BlackHoleStatus {
        name: "BlackHole",
        poolIdentifier: state.config.pool_tag.clone(),
        network,
        internalPayoutMode: false,
        payoutAddressConfigured: payout_configured,
        payoutAddress: payout_address,
        apiPort: state.config.api_port,
        stratumPort: state.config.stratum_port,
        templateRefreshIntervalMs: state.config.template_poll_ms,
        vardiff: BlackHoleVardiff {
            targetShareTimeSec:  state.config.target_share_time_secs,
            checkIntervalSec:    state.config.vardiff_retarget_time_secs,
            minDifficulty:       state.config.min_difficulty,
            maxDifficulty:       state.config.max_difficulty,
        },
        uptime: state.started_at,
        build: build_info::current(),
    })
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct BlackHoleMinerList {
    address: String,
    bestDifficulty: f64,
    workers: Vec<BlackHoleWorker>,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct BlackHoleWorker {
    sessionId: Option<String>,
    name: String,
    userAgent: Option<String>,
    bestDifficulty: f64,
    hashRate: f64,
    startTime: Option<String>,
    lastSeen: String,
}

async fn blackhole_miners(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    let best = snapshot
        .miners
        .iter()
        .map(|m| m.best_submitted_difficulty)
        .fold(0.0, f64::max);

    let mut workers = snapshot
        .miners
        .into_iter()
        .map(|miner| BlackHoleWorker {
            sessionId:      miner.session_id,
            name:           miner.worker,
            userAgent:      miner.user_agent,
            bestDifficulty: miner.best_submitted_difficulty,
            hashRate:       miner.hashrate_gh * 1_000_000_000.0,
            startTime:      miner.session_start.map(|t| t.to_rfc3339()),
            lastSeen:       miner.last_seen.to_rfc3339(),
        })
        .collect::<Vec<_>>();

    workers.sort_by(|a, b| b.hashRate.partial_cmp(&a.hashRate).unwrap_or(std::cmp::Ordering::Equal));

    Json(BlackHoleMinerList {
        address:        state.config.payout_address.clone(),
        bestDifficulty: best,
        workers,
    })
}



async fn blackhole_mempool(State(state): State<ApiState>) -> impl IntoResponse {
    let result: serde_json::Value = match state
        .rpc
        .call("getmempoolinfo", serde_json::json!([]))
        .await
    {
        Ok(val) => val,
        Err(_)  => serde_json::json!({}),
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
#[allow(non_snake_case)]
struct ConnectionStatus {
    bitcoinRpc: RpcStatus,
    zmq: ZmqStatus,
    stratum: StratumStatus,
}

async fn blackhole_connection_status(State(state): State<ApiState>) -> impl IntoResponse {
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
            port:      state.config.stratum_port,
            connected: state.metrics.counters.is_stratum_ready(),
        },
    })
}

#[derive(Serialize)]
#[allow(non_snake_case)]
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

async fn blackhole_template_info(State(state): State<ApiState>) -> impl IntoResponse {
    let job: Arc<JobTemplate> = state.template_engine.subscribe().borrow().clone();
    let info = if job.ready {
        let ntime_u32 = u32::from_str_radix(job.ntime.trim_start_matches("0x"), 16).unwrap_or(0);
        Some(TemplateInfo {
            height:            job.height,
            version:           job.version_u32,
            bits:              job.nbits.clone(),
            previousblockhash: job.prevhash_le.clone(),
            coinbasevalue:     job.coinbase_value,
            mintime:           job.mintime_u32 as u64,
            curtime:           ntime_u32 as u64,
            transactions:      job.transactions.len(),
            target:            job.target.clone(),
            job_id:            job.job_id.clone(),
            created_at:        job.created_at.to_rfc3339(),
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
