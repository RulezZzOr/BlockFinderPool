use std::sync::Arc;
use std::collections::HashSet;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::Deserialize;
use serde::Serialize;
use tracing::info;
use tower_http::cors::{Any, CorsLayer};

use crate::build_info::{self, BuildInfo};
use crate::config::Config;
use crate::metrics::{MetricsStore, ShareEvent};
use crate::rpc::RpcClient;
use crate::storage::{BlockCandidateRow as SqlBlockCandidateRow, BlockWindowRow as SqlBlockWindowRow, SqliteStore};
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
            .route("/block-windows", get(block_windows))
            .route("/block-candidates", get(block_candidates))
            .route("/block-candidates/:id", get(block_candidate_detail))
            .route("/public-blocks", get(public_blocks))
            .route("/pool", get(pool))
            .route("/network", get(network))
            .route("/blockfinder/status",            get(blackhole_status))
            .route("/blockfinder/miners",            get(blackhole_miners))
            .route("/blockfinder/mempool",           get(blackhole_mempool))
            .route("/blockfinder/connection-status", get(blackhole_connection_status))
            .route("/blockfinder/template-info",     get(blackhole_template_info))
            .route("/blackhole/status",              get(blackhole_status))
            .route("/blackhole/miners",              get(blackhole_miners))
            .route("/blackhole/mempool",             get(blackhole_mempool))
            .route("/blackhole/connection-status",   get(blackhole_connection_status))
            .route("/blackhole/template-info",       get(blackhole_template_info))
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

#[derive(Serialize)]
struct PublicBlockRow {
    height: i64,
    hash: String,
    timestamp: String,
    pool: Option<String>,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct BlockCandidateRow {
    timestamp: String,
    worker: String,
    height: i64,
    block_hash: String,
    submitted_difficulty: f64,
    network_difficulty: f64,
    submitblock_result: String,
    submitblock_rpc_latency_ms: i64,
    rpc_error: Option<String>,
}

#[derive(Deserialize)]
struct MempoolBlock {
    id: Option<String>,
    hash: Option<String>,
    height: Option<i64>,
    timestamp: Option<i64>,
    extras: Option<MempoolExtras>,
}

#[derive(Deserialize)]
struct MempoolExtras {
    pool: Option<MempoolPool>,
    pool_name: Option<String>,
    #[serde(rename = "poolName")]
    pool_name_alt: Option<String>,
    miner: Option<String>,
}

#[derive(Deserialize)]
struct MempoolPool {
    name: Option<String>,
    slug: Option<String>,
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

#[derive(Deserialize)]
struct WindowQuery {
    limit: Option<usize>,
}

async fn block_windows(
    Query(query): Query<WindowQuery>,
    State(state): State<ApiState>,
) -> impl IntoResponse {
    let limit = query.limit.unwrap_or(10).clamp(1, 50);
    let current_template_age_secs = state.template_engine.template_age_secs();
    let current = state
        .metrics
        .current_block_window_snapshot(current_template_age_secs)
        .await;
    let current = current.into();
    let finalized_pending = state
        .metrics
        .finalized_block_windows_snapshot()
        .await
        .into_iter()
        .map(SqlBlockWindowRow::from)
        .collect::<Vec<_>>();
    let persisted_limit = limit.saturating_sub(1) as i64;
    let persisted = match state.sqlite.fetch_block_windows(persisted_limit).await {
        Ok(rows) => rows,
        Err(_) => vec![],
    };
    let rows = merge_block_windows(current, finalized_pending, persisted, limit);
    Json(rows)
}

fn merge_block_windows(
    current: SqlBlockWindowRow,
    finalized_pending: Vec<SqlBlockWindowRow>,
    persisted: Vec<SqlBlockWindowRow>,
    limit: usize,
) -> Vec<SqlBlockWindowRow> {
    let mut rows = Vec::<SqlBlockWindowRow>::new();
    rows.push(current);
    rows.extend(finalized_pending.into_iter().rev());
    let seen: HashSet<String> = rows.iter().map(|row| row.id.clone()).collect();
    rows.extend(persisted.into_iter().filter(|row| !seen.contains(&row.id)));
    rows.truncate(limit);
    rows
}

async fn block_candidates(State(state): State<ApiState>) -> impl IntoResponse {
    let rows = match state.sqlite.fetch_block_candidates(30).await {
        Ok(rows) => rows,
        Err(_) => vec![],
    };

    let candidates = rows
        .into_iter()
        .map(|row: SqlBlockCandidateRow| BlockCandidateRow {
            timestamp: row.timestamp,
            worker: row.worker,
            height: row.height,
            block_hash: row.block_hash,
            submitted_difficulty: row.submitted_difficulty,
            network_difficulty: row.network_difficulty,
            submitblock_result: row.submitblock_result,
            submitblock_rpc_latency_ms: row.submitblock_latency_ms,
            rpc_error: row.rpc_error,
        })
        .collect::<Vec<_>>();

    Json(candidates)
}

async fn block_candidate_detail(
    Path(id): Path<String>,
    State(state): State<ApiState>,
) -> impl IntoResponse {
    let candidate_id = match uuid::Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid id" })),
            )
                .into_response()
        }
    };

    match state.sqlite.fetch_block_candidate(candidate_id).await {
        Ok(Some(row)) => Json(row).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        )
            .into_response(),
        Err(err) => {
            tracing::warn!("block candidate detail lookup failed: {err:?}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "lookup failed" })),
            )
                .into_response()
        }
    }
}

async fn public_blocks() -> impl IntoResponse {
    let client = match Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Json(Vec::<PublicBlockRow>::new()),
    };

    let tip_height = match client
        .get("https://mempool.space/api/blocks/tip/height")
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(response) => match response.text().await.ok().and_then(|s| s.trim().parse::<i64>().ok()) {
            Some(height) => height,
            None => return Json(Vec::<PublicBlockRow>::new()),
        },
        Err(_) => return Json(Vec::<PublicBlockRow>::new()),
    };

    let blocks = match client
        .get(format!("https://mempool.space/api/v1/blocks/{tip_height}"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(response) => match response.json::<Vec<MempoolBlock>>().await {
            Ok(blocks) => blocks,
            Err(_) => return Json(Vec::<PublicBlockRow>::new()),
        },
        Err(_) => return Json(Vec::<PublicBlockRow>::new()),
    };

    let rows = blocks
        .into_iter()
        .take(10)
        .filter_map(|block| {
            let height = block.height?;
            let hash = block.id.or(block.hash)?;
            let timestamp = DateTime::<Utc>::from_timestamp(block.timestamp?, 0)?.to_rfc3339();
            let pool = block.extras.and_then(|extras| {
                extras.pool.and_then(|pool| pool.name.or(pool.slug))
                    .or(extras.pool_name)
                    .or(extras.pool_name_alt)
                    .or(extras.miner)
            });

            Some(PublicBlockRow {
                height,
                hash,
                timestamp,
                pool,
            })
        })
        .collect::<Vec<_>>();

    Json(rows)
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
    submitRttP50Ms: f64,
    submitRttP95Ms: f64,
    submitRttP99Ms: f64,
    submitRttMaxMs: f64,
    submitRttOver50MsCount: u64,
    submitRttOver100MsCount: u64,
    globalBestSubmittedDifficulty: f64,
    globalBestAcceptedDifficulty: f64,
    globalBestBlockCandidateDifficulty: f64,
    publicPoolStyleBest: f64,
    cgminerStyleBest: f64,
    rawBest: f64,
    acceptedBest: f64,
    currentBlockBestSubmittedDifficulty: f64,
    currentBlockBestAcceptedDifficulty: f64,
    currentBlockBestCandidateDifficulty: f64,
    previousBlockBestSubmittedDifficulty: f64,
    previousBlockBestAcceptedDifficulty: f64,
    previousBlockBestCandidateDifficulty: f64,
    templateMaxAgeSecs: u64,
    lastTemplateRefreshAt: String,
    lastZmqBlockAt: Option<String>,
    lastZmqTxAt: Option<String>,
    currentTemplateAgeSecs: Option<u64>,
    templateAgeSecs: Option<u64>,
    templateStale: bool,
    zmqConnected: bool,
    lastCleanJobsNotifyAt: Option<String>,
    templateRefreshFailures: u64,
    rpcHealthy: bool,
    currentBlockHeight: u64,
    currentBlockPrevhash: String,
    currentBlockTemplateKey: String,
    currentBlockJobId: String,
    currentBlockCreatedAt: String,
    previousBlockHeight: u64,
    previousBlockPrevhash: String,
    previousBlockTemplateKey: String,
    previousBlockJobId: String,
    previousBlockCreatedAt: Option<String>,
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
        submitRttP50Ms:         snapshot.submit_rtt_p50_ms,
        submitRttP95Ms:         snapshot.submit_rtt_p95_ms,
        submitRttP99Ms:         snapshot.submit_rtt_p99_ms,
        submitRttMaxMs:         snapshot.submit_rtt_max_ms,
        submitRttOver50MsCount:  snapshot.submit_rtt_over_50ms_count,
        submitRttOver100MsCount: snapshot.submit_rtt_over_100ms_count,
        globalBestSubmittedDifficulty: snapshot.global_best_submitted_difficulty,
        globalBestAcceptedDifficulty: snapshot.global_best_accepted_difficulty,
        globalBestBlockCandidateDifficulty: snapshot.global_best_block_candidate_difficulty,
        publicPoolStyleBest: snapshot.global_best_submitted_difficulty,
        cgminerStyleBest: snapshot.global_best_accepted_difficulty,
        rawBest: snapshot.global_best_submitted_difficulty,
        acceptedBest: snapshot.global_best_accepted_difficulty,
        currentBlockBestSubmittedDifficulty: snapshot.current_block_best_submitted_difficulty,
        currentBlockBestAcceptedDifficulty: snapshot.current_block_best_accepted_difficulty,
        currentBlockBestCandidateDifficulty: snapshot.current_block_best_candidate_difficulty,
        previousBlockBestSubmittedDifficulty: snapshot.previous_block_best_submitted_difficulty,
        previousBlockBestAcceptedDifficulty: snapshot.previous_block_best_accepted_difficulty,
        previousBlockBestCandidateDifficulty: snapshot.previous_block_best_candidate_difficulty,
        templateMaxAgeSecs: state.config.template_max_age_secs,
        lastTemplateRefreshAt: state
            .template_engine
            .last_template_refresh_at()
            .unwrap_or(snapshot.current_scope.created_at)
            .to_rfc3339(),
        lastZmqBlockAt: state.template_engine.last_zmq_block_trigger_at().map(|dt| dt.to_rfc3339()),
        lastZmqTxAt: state.template_engine.last_zmq_tx_trigger_at().map(|dt| dt.to_rfc3339()),
        currentTemplateAgeSecs: state.template_engine.template_age_secs(),
        templateAgeSecs: state.template_engine.template_age_secs(),
        templateStale: state
            .template_engine
            .template_age_secs()
            .map(|age| age > state.config.template_max_age_secs)
            .unwrap_or(true),
        zmqConnected: state.template_engine.zmq_connected(),
        lastCleanJobsNotifyAt: state.metrics.counters.last_clean_jobs_notify_at().map(|dt| dt.to_rfc3339()),
        templateRefreshFailures: state.template_engine.template_refresh_failures(),
        rpcHealthy: state.template_engine.rpc_healthy(),
        currentBlockHeight: snapshot.current_scope.height,
        currentBlockPrevhash: snapshot.current_scope.prevhash.clone(),
        currentBlockTemplateKey: snapshot.current_scope.template_key.clone(),
        currentBlockJobId: snapshot.current_scope.job_id.clone(),
        currentBlockCreatedAt: snapshot.current_scope.created_at.to_rfc3339(),
        previousBlockHeight: snapshot.previous_scope.as_ref().map(|s| s.height).unwrap_or(0),
        previousBlockPrevhash: snapshot.previous_scope.as_ref().map(|s| s.prevhash.clone()).unwrap_or_default(),
        previousBlockTemplateKey: snapshot.previous_scope.as_ref().map(|s| s.template_key.clone()).unwrap_or_default(),
        previousBlockJobId: snapshot.previous_scope.as_ref().map(|s| s.job_id.clone()).unwrap_or_default(),
        previousBlockCreatedAt: snapshot.previous_scope.as_ref().map(|s| s.created_at.to_rfc3339()),
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
struct BlockFinderStatus {
    name: &'static str,
    poolIdentifier: String,
    network: String,
    internalPayoutMode: bool,
    payoutAddressConfigured: bool,
    payoutAddress: Option<String>,
    apiPort: u16,
    stratumPort: u16,
    templateRefreshIntervalMs: u64,
    vardiff: BlockFinderVardiff,
    uptime: DateTime<Utc>,
    build: BuildInfo,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct BlockFinderVardiff {
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

    Json(BlockFinderStatus {
        name: "BlockFinder",
        poolIdentifier: state.config.pool_tag.clone(),
        network,
        internalPayoutMode: false,
        payoutAddressConfigured: payout_configured,
        payoutAddress: payout_address,
        apiPort: state.config.api_port,
        stratumPort: state.config.stratum_port,
        templateRefreshIntervalMs: state.config.template_poll_ms,
        vardiff: BlockFinderVardiff {
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
struct BlockFinderMinerList {
    address: String,
    bestDifficulty: f64,
    bestSubmittedDifficulty: f64,
    bestAcceptedDifficulty: f64,
    bestBlockCandidateDifficulty: f64,
    publicPoolStyleBest: f64,
    cgminerStyleBest: f64,
    rawBest: f64,
    acceptedBest: f64,
    workers: Vec<BlockFinderWorker>,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct BlockFinderWorker {
    sessionId: Option<String>,
    name: String,
    userAgent: Option<String>,
    bestDifficulty: f64,
    bestSubmittedDifficulty: f64,
    bestAcceptedDifficulty: f64,
    bestBlockCandidateDifficulty: f64,
    publicPoolStyleBest: f64,
    cgminerStyleBest: f64,
    rawBest: f64,
    acceptedBest: f64,
    currentBlockBestSubmittedDifficulty: f64,
    currentBlockBestAcceptedDifficulty: f64,
    currentBlockBestCandidateDifficulty: f64,
    hashRate: f64,
    startTime: Option<String>,
    lastSeen: String,
    lastShareStatus: Option<String>,
    lastShareDifficulty: f64,
    lastShareAt: Option<String>,
}

async fn blackhole_miners(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    let best = snapshot
        .miners
        .iter()
        .map(|m| m.best_submitted_difficulty)
        .fold(0.0, f64::max);
    let best_accepted = snapshot
        .miners
        .iter()
        .map(|m| m.best_accepted_difficulty)
        .fold(0.0, f64::max);
    let best_candidate = snapshot
        .miners
        .iter()
        .map(|m| m.best_block_candidate_difficulty)
        .fold(0.0, f64::max);

    let mut workers = snapshot
        .miners
        .into_iter()
        .map(|miner| BlockFinderWorker {
            sessionId:      miner.session_id,
            name:           miner.worker,
            userAgent:      miner.user_agent,
            bestDifficulty: miner.best_submitted_difficulty,
            bestSubmittedDifficulty: miner.best_submitted_difficulty,
            bestAcceptedDifficulty: miner.best_accepted_difficulty,
            bestBlockCandidateDifficulty: miner.best_block_candidate_difficulty,
            publicPoolStyleBest: miner.best_submitted_difficulty,
            cgminerStyleBest: miner.best_accepted_difficulty,
            rawBest: miner.best_submitted_difficulty,
            acceptedBest: miner.best_accepted_difficulty,
            currentBlockBestSubmittedDifficulty: miner.current_block_best_submitted_difficulty,
            currentBlockBestAcceptedDifficulty: miner.current_block_best_accepted_difficulty,
            currentBlockBestCandidateDifficulty: miner.current_block_best_candidate_difficulty,
            hashRate:       miner.hashrate_gh * 1_000_000_000.0,
            startTime:      miner.session_start.map(|t| t.to_rfc3339()),
            lastSeen:       miner.last_seen.to_rfc3339(),
            lastShareStatus: miner.last_share_status,
            lastShareDifficulty: miner.last_share_difficulty,
            lastShareAt: miner.last_share_at.map(|t| t.to_rfc3339()),
        })
        .collect::<Vec<_>>();

    workers.sort_by(|a, b| b.hashRate.partial_cmp(&a.hashRate).unwrap_or(std::cmp::Ordering::Equal));

    Json(BlockFinderMinerList {
        address:        state.config.payout_address.clone(),
        bestDifficulty: best,
        bestSubmittedDifficulty: best,
        bestAcceptedDifficulty: best_accepted,
        bestBlockCandidateDifficulty: best_candidate,
        publicPoolStyleBest: best,
        cgminerStyleBest: best_accepted,
        rawBest: best,
        acceptedBest: best_accepted,
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
    blockConnected: bool,
    txConnected: bool,
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
            enabled: state.template_engine.zmq_connected(),
            blockConnected: state.template_engine.zmq_block_connected(),
            txConnected: state.template_engine.zmq_tx_connected(),
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
        if !event.accepted {
            continue;
        }
        let key = event.created_at.format("%Y-%m-%dT%H:%M:00Z").to_string();
        *buckets.entry(key).or_insert(0usize) += 1;
    }
    buckets
        .into_iter()
        .map(|(timestamp, shares)| HashratePoint { timestamp, shares })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn build_share_series_counts_accepted_only() {
        let now = Utc::now();
        let events = vec![
            ShareEvent {
                worker: "miner".to_string(),
                difficulty: 1.0,
                accepted: true,
                is_block: false,
                submit_rtt_ms: 1.0,
                created_at: now,
                job_age_secs: 1,
                notify_delay_ms: 1,
                reconnect_recent: false,
            },
            ShareEvent {
                worker: "miner".to_string(),
                difficulty: 2.0,
                accepted: false,
                is_block: false,
                submit_rtt_ms: 1.0,
                created_at: now,
                job_age_secs: 1,
                notify_delay_ms: 1,
                reconnect_recent: false,
            },
        ];

        let series = build_share_series(events);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].shares, 1);
    }

    #[test]
    fn pool_info_serializes_expected_best_and_health_fields() {
        let pool = PoolInfo {
            totalHashRate: 0.0,
            totalMiners: 0,
            blockHeight: 0,
            blocksFound: 0,
            fee: 0,
            poolStartedAt: "2026-04-22T00:00:00Z".to_string(),
            uptimeSecs: 0,
            jobsSent: 0,
            cleanJobsSent: 0,
            jobsSentPerMiner: 0.0,
            jobsSentPerMinerPerMin: 0.0,
            notifyDeduped: 0,
            notifyRateLimited: 0,
            duplicateShares: 0,
            reconnectsTotal: 0,
            submitblockAccepted: 0,
            submitblockRejected: 0,
            submitblockRpcFail: 0,
            versionRollingViolations: 0,
            stalesNewBlock: 0,
            stalesExpired: 0,
            stalesReconnect: 0,
            zmqBlocksDetected: 0,
            zmqBlockNotifications: 0,
            zmqTxTriggered: 0,
            zmqTxDebounced: 0,
            zmqTxPostBlockSuppressed: 0,
            staleRatio: 0.0,
            submitRttP50Ms: 0.0,
            submitRttP95Ms: 0.0,
            submitRttP99Ms: 0.0,
            submitRttMaxMs: 0.0,
            submitRttOver50MsCount: 0,
            submitRttOver100MsCount: 0,
            globalBestSubmittedDifficulty: 12.0,
            globalBestAcceptedDifficulty: 11.0,
            globalBestBlockCandidateDifficulty: 10.0,
            publicPoolStyleBest: 12.0,
            cgminerStyleBest: 11.0,
            rawBest: 12.0,
            acceptedBest: 11.0,
            currentBlockBestSubmittedDifficulty: 9.0,
            currentBlockBestAcceptedDifficulty: 8.0,
            currentBlockBestCandidateDifficulty: 7.0,
            previousBlockBestSubmittedDifficulty: 6.0,
            previousBlockBestAcceptedDifficulty: 5.0,
            previousBlockBestCandidateDifficulty: 4.0,
            templateMaxAgeSecs: 30,
            lastTemplateRefreshAt: "2026-04-22T00:00:00Z".to_string(),
            lastZmqBlockAt: None,
            lastZmqTxAt: None,
            currentTemplateAgeSecs: Some(1),
            templateAgeSecs: Some(1),
            templateStale: false,
            zmqConnected: true,
            lastCleanJobsNotifyAt: None,
            templateRefreshFailures: 0,
            rpcHealthy: true,
            currentBlockHeight: 123,
            currentBlockPrevhash: "prev".to_string(),
            currentBlockTemplateKey: "tmpl".to_string(),
            currentBlockJobId: "job".to_string(),
            currentBlockCreatedAt: "2026-04-22T00:00:00Z".to_string(),
            previousBlockHeight: 122,
            previousBlockPrevhash: "prev-2".to_string(),
            previousBlockTemplateKey: "tmpl-2".to_string(),
            previousBlockJobId: "job-2".to_string(),
            previousBlockCreatedAt: None,
            networkDifficulty: 135.0,
            networkHashps: 42.0,
        };

        let value = serde_json::to_value(pool).expect("pool serializes");
        assert_eq!(value["rawBest"], Value::from(12.0));
        assert_eq!(value["acceptedBest"], Value::from(11.0));
        assert_eq!(value["currentBlockBestSubmittedDifficulty"], Value::from(9.0));
        assert_eq!(value["zmqConnected"], Value::from(true));
        assert_eq!(value["rpcHealthy"], Value::from(true));
        assert_eq!(value["templateRefreshFailures"], Value::from(0));
    }

    #[test]
    fn worker_serializes_expected_best_and_last_share_fields() {
        let worker = BlockFinderWorker {
            sessionId: Some("session-1".to_string()),
            name: "miner-1".to_string(),
            userAgent: Some("bmminer".to_string()),
            bestDifficulty: 15.0,
            bestSubmittedDifficulty: 15.0,
            bestAcceptedDifficulty: 14.0,
            bestBlockCandidateDifficulty: 13.0,
            publicPoolStyleBest: 15.0,
            cgminerStyleBest: 14.0,
            rawBest: 15.0,
            acceptedBest: 14.0,
            currentBlockBestSubmittedDifficulty: 12.0,
            currentBlockBestAcceptedDifficulty: 11.0,
            currentBlockBestCandidateDifficulty: 10.0,
            hashRate: 1_000.0,
            startTime: Some("2026-04-22T00:00:00Z".to_string()),
            lastSeen: "2026-04-22T00:00:00Z".to_string(),
            lastShareStatus: Some("accepted".to_string()),
            lastShareDifficulty: 9.0,
            lastShareAt: Some("2026-04-22T00:00:00Z".to_string()),
        };

        let value = serde_json::to_value(worker).expect("worker serializes");
        assert_eq!(value["rawBest"], Value::from(15.0));
        assert_eq!(value["acceptedBest"], Value::from(14.0));
        assert_eq!(value["lastShareStatus"], Value::from("accepted"));
        assert_eq!(value["lastShareDifficulty"], Value::from(9.0));
        assert_eq!(value["lastShareAt"], Value::from("2026-04-22T00:00:00Z"));
    }

    #[test]
    fn connection_status_serializes_separate_zmq_flags() {
        let status = ConnectionStatus {
            bitcoinRpc: RpcStatus {
                connected: true,
                latency: Some(17),
                url: "http://127.0.0.1:8332".to_string(),
                port: Some(8332),
            },
            zmq: ZmqStatus {
                enabled: true,
                blockConnected: true,
                txConnected: false,
                host: "tcp://127.0.0.1:28334,tcp://127.0.0.1:28335".to_string(),
            },
            stratum: StratumStatus {
                port: 3333,
                connected: true,
            },
        };

        let value = serde_json::to_value(status).expect("status serializes");
        assert_eq!(value["zmq"]["enabled"], Value::from(true));
        assert_eq!(value["zmq"]["blockConnected"], Value::from(true));
        assert_eq!(value["zmq"]["txConnected"], Value::from(false));
    }

    #[test]
    fn block_candidate_row_serializes_public_fields() {
        let candidate = BlockCandidateRow {
            timestamp: "2026-04-22T00:00:00Z".to_string(),
            worker: "miner-1".to_string(),
            height: 123,
            block_hash: "hash".to_string(),
            submitted_difficulty: 42.0,
            network_difficulty: 100.0,
            submitblock_result: "submitted".to_string(),
            submitblock_rpc_latency_ms: 17,
            rpc_error: Some("rpc failed".to_string()),
        };

        let value = serde_json::to_value(candidate).expect("candidate serializes");
        assert_eq!(value["timestamp"], Value::from("2026-04-22T00:00:00Z"));
        assert_eq!(value["worker"], Value::from("miner-1"));
        assert_eq!(value["height"], Value::from(123));
        assert_eq!(value["block_hash"], Value::from("hash"));
        assert_eq!(value["submitblock_result"], Value::from("submitted"));
        assert_eq!(value["submitblock_rpc_latency_ms"], Value::from(17));
        assert_eq!(value["rpc_error"], Value::from("rpc failed"));
    }

    #[test]
    fn block_window_row_serializes_current_and_history_fields() {
        let window = SqlBlockWindowRow {
            id: "window-1".to_string(),
            height: 123,
            prevhash: "prev".to_string(),
            block_hash: None,
            started_at: "2026-04-22T00:00:00Z".to_string(),
            ended_at: None,
            duration_secs: None,
            external_pool: Some("F2Pool".to_string()),
            tx_count: 1234,
            fee_rate_sat_vb: Some(4.2),
            best_submitted_difficulty: 12.0,
            best_accepted_difficulty: 11.0,
            best_block_candidate_difficulty: 0.0,
            best_worker: Some("worker-a".to_string()),
            best_payout_address: Some("bc1qbest".to_string()),
            best_submitted_worker: Some("worker-a".to_string()),
            best_submitted_payout_address: Some("bc1qbest".to_string()),
            best_accepted_worker: Some("worker-a".to_string()),
            best_candidate_worker: None,
            share_count: 3,
            accepted_count: 2,
            stale_count: 1,
            duplicate_count: 0,
            avg_pool_hashrate: Some(111.0),
            template_key: "tmpl".to_string(),
            job_id: "job".to_string(),
            network_difficulty: 99.0,
            in_progress: true,
            current_template_age_secs: Some(7),
            created_at: "2026-04-22T00:00:00Z".to_string(),
            updated_at: "2026-04-22T00:00:00Z".to_string(),
        };

        let value = serde_json::to_value(window).expect("window serializes");
        assert_eq!(value["inProgress"], Value::from(true));
        assert_eq!(value["bestSubmittedDifficulty"], Value::from(12.0));
        assert_eq!(value["bestWorker"], Value::from("worker-a"));
    }

    #[test]
    fn merge_block_windows_returns_current_first_then_newest_history() {
        let current = SqlBlockWindowRow {
            id: "current".to_string(),
            height: 200,
            prevhash: "prev-current".to_string(),
            block_hash: None,
            started_at: "2026-04-22T00:00:00Z".to_string(),
            ended_at: None,
            duration_secs: None,
            external_pool: None,
            tx_count: 1,
            fee_rate_sat_vb: None,
            best_submitted_difficulty: 20.0,
            best_accepted_difficulty: 20.0,
            best_block_candidate_difficulty: 0.0,
            best_worker: Some("worker-current".to_string()),
            best_payout_address: Some("bc1qcurrent".to_string()),
            best_submitted_worker: Some("worker-current".to_string()),
            best_submitted_payout_address: Some("bc1qcurrent".to_string()),
            best_accepted_worker: Some("worker-current".to_string()),
            best_candidate_worker: None,
            share_count: 1,
            accepted_count: 1,
            stale_count: 0,
            duplicate_count: 0,
            avg_pool_hashrate: Some(100.0),
            template_key: "tmpl-current".to_string(),
            job_id: "job-current".to_string(),
            network_difficulty: 1.0,
            in_progress: true,
            current_template_age_secs: Some(1),
            created_at: "2026-04-22T00:00:00Z".to_string(),
            updated_at: "2026-04-22T00:00:00Z".to_string(),
        };
        let finalized_old = SqlBlockWindowRow { id: "old".to_string(), height: 198, prevhash: "p1".to_string(), block_hash: Some("h1".to_string()), started_at: "2026-04-21T00:00:00Z".to_string(), ended_at: Some("2026-04-21T00:10:00Z".to_string()), duration_secs: Some(600), external_pool: None, tx_count: 2, fee_rate_sat_vb: None, best_submitted_difficulty: 1.0, best_accepted_difficulty: 1.0, best_block_candidate_difficulty: 0.0, best_worker: Some("w1".to_string()), best_payout_address: None, best_submitted_worker: Some("w1".to_string()), best_submitted_payout_address: None, best_accepted_worker: Some("w1".to_string()), best_candidate_worker: None, share_count: 1, accepted_count: 1, stale_count: 0, duplicate_count: 0, avg_pool_hashrate: None, template_key: "t1".to_string(), job_id: "j1".to_string(), network_difficulty: 1.0, in_progress: false, current_template_age_secs: None, created_at: "2026-04-21T00:00:00Z".to_string(), updated_at: "2026-04-21T00:10:00Z".to_string() };
        let finalized_new = SqlBlockWindowRow { id: "new".to_string(), height: 199, prevhash: "p2".to_string(), block_hash: Some("h2".to_string()), started_at: "2026-04-21T01:00:00Z".to_string(), ended_at: Some("2026-04-21T01:10:00Z".to_string()), duration_secs: Some(600), external_pool: None, tx_count: 2, fee_rate_sat_vb: None, best_submitted_difficulty: 2.0, best_accepted_difficulty: 2.0, best_block_candidate_difficulty: 0.0, best_worker: Some("w2".to_string()), best_payout_address: None, best_submitted_worker: Some("w2".to_string()), best_submitted_payout_address: None, best_accepted_worker: Some("w2".to_string()), best_candidate_worker: None, share_count: 2, accepted_count: 2, stale_count: 0, duplicate_count: 0, avg_pool_hashrate: None, template_key: "t2".to_string(), job_id: "j2".to_string(), network_difficulty: 2.0, in_progress: false, current_template_age_secs: None, created_at: "2026-04-21T01:00:00Z".to_string(), updated_at: "2026-04-21T01:10:00Z".to_string() };

        let rows = merge_block_windows(current, vec![finalized_old, finalized_new], vec![], 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, "current");
        assert_eq!(rows[1].id, "new");
        assert_eq!(rows[2].id, "old");
    }
}

async fn fetch_mining_info(rpc: &RpcClient) -> anyhow::Result<MiningInfo> {
    rpc.call("getmininginfo", serde_json::json!([])).await
}

fn parse_port(url: &str) -> Option<u16> {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    host_port.split(':').nth(1)?.parse().ok()
}
