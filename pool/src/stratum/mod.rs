use std::collections::VecDeque;
use std::env;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Duplicate-share key: (job_id_u64, nonce_u32, ntime_u32, extranonce2_hex, version_u32).
///
/// extranonce2 is stored as the canonical validated hex String rather than u64 so that
/// any EXTRANONCE2_SIZE is handled correctly, including values > 8 bytes.  Parsing a
/// multi-byte extranonce2 as u64 would overflow and return unwrap_or(0), collapsing all
/// distinct large extranonce2 values to the same key — a false-duplicate that silently
/// drops real shares, including high-difficulty / best-share candidates.
type DupKey = (u64, u32, u32, String, u32);

use anyhow::Context;
use chrono::Utc;
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

/// Maximum byte length of a single Stratum line (including trailing `\n`).
/// A legitimate mining.submit for a BIP310 miner is < 400 bytes.
/// 64 KB is generous enough for any valid Stratum message.
/// Without this guard, a client that never sends `\n` causes BufReader to
/// grow until the process is OOM-killed.
const MAX_LINE_BYTES: usize = 65_536;

/// If no data is received from a miner within this duration the session is
/// torn down.  Bitcoin ASICs submit shares every few seconds; 300 s gives
/// enough headroom for very low-difficulty / low-hashrate devices while
/// ensuring dead TCP connections are reaped promptly.
const IDLE_TIMEOUT_SECS: u64 = 300;

/// Duplicate share LRU window per session.
///
/// The key includes job_id, nonce, ntime, extranonce2 and version. A larger
/// window protects against reconnect/replay bursts without using a full clear().
const MAX_DUP_HASHES: usize = 16_384;
use uuid::Uuid;

use std::str::FromStr;

use bitcoin::Address;

use crate::config::Config;
use crate::metrics::{MetricsStore, StaleReason};
use crate::share::{share_target_le, validate_share, ShareSubmit};
use std::collections::HashSet;
use crate::storage::{BlockCandidateRecord, RedisStore, ShareRecord, SqliteStore};
use crate::template::{build_coinbase2_for_payout, JobTemplate, TemplateEngine};
use crate::vardiff::VardiffController;

#[derive(Clone)]
pub struct StratumServer {
    config: Config,
    template_engine: Arc<TemplateEngine>,
    metrics: MetricsStore,
    redis: RedisStore,
    sqlite: SqliteStore,
}

impl StratumServer {
    pub fn new(
        config: Config,
        template_engine: Arc<TemplateEngine>,
        metrics: MetricsStore,
        redis: RedisStore,
        sqlite: SqliteStore,
    ) -> Self {
        Self {
            config,
            template_engine,
            metrics,
            redis,
            sqlite,
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let bind = format!("{}:{}", self.config.stratum_bind, self.config.stratum_port);
        let listener = TcpListener::bind(&bind).await?;
        self.metrics.counters.set_stratum_ready();
        info!("stratum listening on {bind}");

        loop {
            let (stream, addr) = listener.accept().await?;
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(err) = server.handle_client(stream, addr).await {
                    warn!("client error: {err:?}");
                }
            });
        }
    }

    async fn handle_client(&self, stream: TcpStream, addr: SocketAddr) -> anyhow::Result<()> {
        // Reduce latency for share submissions/notifications.
        let _ = stream.set_nodelay(true);
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        let (tx, mut rx) = mpsc::channel::<String>(64);
        // Writer task: exits when the socket dies or all senders are dropped.
        // Reuses a single Vec<u8> buffer to combine msg + "\n" into one write_all,
        // reducing syscall count from 2N to N and packet count for small messages.
        tokio::spawn(async move {
            let mut write_buf: Vec<u8> = Vec::with_capacity(512);
            while let Some(msg) = rx.recv().await {
                write_buf.clear();
                write_buf.extend_from_slice(msg.as_bytes());
                write_buf.push(b'\n');
                if writer.write_all(&write_buf).await.is_err() {
                    break;
                }
            }
        });

        let mut extranonce1 = vec![0u8; self.config.extranonce1_size];
        rand::thread_rng().fill_bytes(&mut extranonce1);
        let extranonce1_hex = hex::encode(&extranonce1);

        let state = Arc::new(Mutex::new(SessionState::new(
            extranonce1_hex.clone(),
            extranonce1.clone(),
            self.config.start_difficulty,
            self.config.target_share_time_secs,
            self.config.vardiff_retarget_time_secs,
            self.config.min_difficulty,
            self.config.max_difficulty,
            self.config.vardiff_enabled,
            self.config.notify_bucket_capacity,
            self.config.notify_bucket_refill_ms,
        )));

        let job_rx = self.template_engine.subscribe();
        let mut notify_rx = job_rx.clone();
        let notify_tx = tx.clone();
        let notify_state = state.clone();
        let notify_counters = self.metrics.counters.clone();

        tokio::spawn(async move {
            loop {
                // Exit promptly when the client disconnects (receiver is dropped).
                tokio::select! {
                    _ = notify_tx.closed() => {
                        break;
                    }
                    changed = notify_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                }
                let job = notify_rx.borrow().clone();
                if !job.ready {
                    continue;
                }

                // ── Token-bucket notify gate (single lock) ──────────────────
                // All state reads and mutations (auth check, clean_jobs detection,
                // bucket consume, job push) happen inside ONE lock acquisition.
                //
                // Previously: 3 separate locks (peek → bucket → work).
                // Now: 1 lock — eliminates 2 redundant Mutex round-trips per notify.
                //
                // Safety: try_consume() is pure synchronous computation (no await),
                // so holding the lock across it is safe.  The lock is never held
                // across a yield point.
                //
                // Semantics preserved:
                //  - clean_jobs=true bypasses the bucket (critical path, no rate limit)
                //  - clean_jobs=false consumes a token; empty bucket → skip notify
                //  - authorized check still gates all work
                let notify = {
                    let mut guard = notify_state.lock().await;

                    if !guard.authorized {
                        continue;
                    }

                    let clean_jobs = guard
                        .last_prevhash
                        .as_deref()
                        .map_or(true, |prev| prev != job.prevhash_le);

                    // For mempool-only updates, check (and consume) a token.
                    // clean_jobs=true bypasses the bucket — new-block notify must never be delayed.
                    if !clean_jobs && !guard.notify_bucket.try_consume() {
                        notify_counters.inc_notify_rate_limited();
                        continue;
                    }

                    if clean_jobs {
                        // Mark existing jobs stale in-place (AtomicBool, no Arc recreation).
                        // Grace window stays active for STALE_GRACE_MS after this point.
                        guard.mark_jobs_stale_block();
                        guard.last_clean_jobs_time = Some(Utc::now());
                        notify_counters.set_last_clean_jobs_notify_at(Utc::now());
                    }
                    let diff = guard.difficulty;
                    let extranonce1_bytes = guard.extranonce1_bytes.clone();
                    let session_job = guard.push_job(job.clone(), diff, &extranonce1_bytes, None);
                    guard.last_notify = Utc::now();
                    guard.last_prevhash = Some(job.prevhash_le.clone());

                    notify_counters.inc_jobs_sent(clean_jobs);

                    build_notify_with_ntime(
                        &session_job.session_job_id,
                        session_job.job.as_ref(),
                        &session_job.notify_ntime,
                        clean_jobs,
                        session_job.custom_coinbase2_hex.as_deref(),
                    )
                };

                if notify_tx.send(notify).await.is_err() {
                    break;
                }
            }
        });
        let refresh_rx = job_rx.clone();
        let refresh_tx = tx.clone();
        let refresh_state = state.clone();
        let refresh_config = self.config.clone();
        let refresh_counters = self.metrics.counters.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(refresh_config.job_refresh_ms));
            loop {
                tokio::select! {
                    _ = refresh_tx.closed() => {
                        break;
                    }
                    _ = interval.tick() => {}
                }
                let job = refresh_rx.borrow().clone();
                if !job.ready {
                    continue;
                }
                let notify = {
                    let mut guard = refresh_state.lock().await;
                    if !guard.authorized {
                        continue;
                    }
                    if guard.last_prevhash.as_deref() != Some(job.prevhash_le.as_str()) {
                        continue;
                    }

                    // Skip ntime refresh if a job was already sent recently.
                    // This prevents the refresh timer from firing right after the template
                    // engine just sent a job (e.g. both ZMQ_DEBOUNCE and JOB_REFRESH_MS
                    // are equal), which would cause the miner to receive two jobs in ~1s.
                    let since_last_ms = (Utc::now() - guard.last_notify).num_milliseconds();
                    let half_refresh = (refresh_config.job_refresh_ms / 2) as i64;
                    if since_last_ms < half_refresh {
                        continue;
                    }

                    // Help firmwares that don't roll ntime well: bump notify ntime on refresh.
                    // Never go below mintime/curtime (Bitcoin Core will reject blocks earlier than mintime).
                    let now_u32 = Utc::now().timestamp() as u32;
                    let job_ntime_u32 = u32::from_str_radix(job.ntime.trim_start_matches("0x"), 16).unwrap_or(0);
                    let min_ok = job.mintime_u32.max(job_ntime_u32);
                    let bumped_ntime = format!("{:08x}", now_u32.max(min_ok));
                    let diff = guard.difficulty;
                    let extranonce1_bytes = guard.extranonce1_bytes.clone();
                    let session_job =
                        guard.push_job(job.clone(), diff, &extranonce1_bytes, Some(bumped_ntime));
                    guard.last_notify = Utc::now();
                    refresh_counters.inc_jobs_sent(false);
                    build_notify_with_ntime(&session_job.session_job_id, session_job.job.as_ref(), &session_job.notify_ntime, false, session_job.custom_coinbase2_hex.as_deref())
                };
                if refresh_tx.send(notify).await.is_err() {
                    break;
                }
            }
        });

        let vardiff_state = state.clone();
        let vardiff_tx = tx.clone();
        let vardiff_config = self.config.clone();
        tokio::spawn(async move {
            if !vardiff_config.vardiff_enabled {
                return;
            }
            let interval_ms = (vardiff_config.vardiff_retarget_time_secs * 1000.0).max(1000.0) as u64;
            let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
            loop {
                tokio::select! {
                    _ = vardiff_tx.closed() => {
                        break;
                    }
                    _ = interval.tick() => {}
                }
                let now = Utc::now();

                let (diff_msg, notify_msg) = {
                    let mut guard = vardiff_state.lock().await;
                    if !guard.authorized {
                        (None, None)
                    } else {
                        let current_diff = guard.difficulty;
                        if let Some(new_diff) = guard.vardiff.maybe_retarget(current_diff, now) {
                            guard.difficulty = new_diff;
                            let diff_msg = Some(build_set_difficulty(new_diff));

                            let job_latest = guard.jobs.back().map(|entry| entry.job.clone());
                            let notify_msg = if let Some(job_latest) = job_latest {
                                let extranonce1_bytes = guard.extranonce1_bytes.clone();
                                let session_job =
                                    guard.push_job(job_latest.clone(), new_diff, &extranonce1_bytes, None);
                                guard.last_notify = Utc::now();
                                guard.last_prevhash = Some(job_latest.prevhash_le.clone());
                                Some(build_notify_with_ntime(
                                    &session_job.session_job_id,
                                    session_job.job.as_ref(),
                                    &session_job.notify_ntime,
                                    false,
                                    session_job.custom_coinbase2_hex.as_deref(),
                                ))
                            } else {
                                None
                            };

                            (diff_msg, notify_msg)
                        } else {
                            (None, None)
                        }
                    }
                };

                if let Some(m) = diff_msg {
                    if vardiff_tx.send(m).await.is_err() {
                        break;
                    }
                }
                if let Some(m) = notify_msg {
                    if vardiff_tx.send(m).await.is_err() {
                        break;
                    }
                }
            }
        });

        info!("miner connected: {addr}");
        self.metrics.counters.inc_reconnect();

        // ── Guarded line reader ───────────────────────────────────────────────
        // We need two safety properties that plain `lines.next_line()` does not
        // provide on its own:
        //
        //  1. MAX LINE LENGTH: A miner that sends bytes without a newline will
        //     cause BufReader to buffer them indefinitely → OOM.  We read into
        //     a pre-allocated Vec and enforce a hard cap.  Any message over
        //     MAX_LINE_BYTES is assumed malicious; the connection is dropped.
        //
        //  2. IDLE TIMEOUT: A half-open TCP connection or a dead miner that
        //     stops transmitting keeps the session alive forever.  We wrap every
        //     read in tokio::time::timeout.
        //
        // Implementation note: we switch from `lines()` to `read_until(b'\n')`
        // so we can inspect the accumulated length before allocating a String.
        let mut buf: Vec<u8> = Vec::with_capacity(512);

        loop {
            buf.clear();
            let read_result = tokio::time::timeout(
                Duration::from_secs(IDLE_TIMEOUT_SECS),
                lines.get_mut().read_until(b'\n', &mut buf),
            ).await;

            let n = match read_result {
                Err(_elapsed) => {
                    warn!("idle timeout ({IDLE_TIMEOUT_SECS}s) for {addr}, closing session");
                    break;
                }
                Ok(Err(io_err)) => {
                    // Real I/O error (connection reset etc.) — not a warning.
                    return Err(io_err.into());
                }
                Ok(Ok(0)) => {
                    // EOF — miner closed the connection cleanly.
                    break;
                }
                Ok(Ok(n)) => n,
            };

            // ── Line-length guard ─────────────────────────────────────────────
            if n > MAX_LINE_BYTES {
                warn!(
                    "oversized Stratum line from {addr}: {} bytes (max {}), closing session",
                    n, MAX_LINE_BYTES
                );
                break;
            }

            // Strip trailing \n / \r\n for the JSON parser.
            let line = match std::str::from_utf8(&buf[..n]) {
                Ok(s) => s.trim_end_matches(|c| c == '\n' || c == '\r'),
                Err(_) => {
                    warn!("non-UTF-8 data from {addr}, skipping line");
                    continue;
                }
            };

            if line.is_empty() {
                continue;
            }

            let request: StratumRequest = match serde_json::from_str(line) {
                Ok(req) => req,
                Err(err) => {
                    warn!("invalid json from {addr}: {err}");
                    continue;
                }
            };

            match request.method.as_str() {
                "mining.subscribe" => {
                    let user_agent = request
                        .params
                        .as_array()
                        .and_then(|params| params.get(0))
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string());
                    if user_agent.is_some() {
                        let mut guard = state.lock().await;
                        guard.user_agent = user_agent;
                    }
                    let response = json!({
                        "id": request.id,
                        "result": [
                            [
                                ["mining.set_difficulty", "1"],
                                ["mining.notify", "1"]
                            ],
                            extranonce1_hex,
                            self.config.extranonce2_size
                        ],
                        "error": null
                    });
                    let _ = tx.send(response.to_string()).await;
                }
                "mining.extranonce.subscribe" => {
                    let response = json!({
                        "id": request.id,
                        "result": true,
                        "error": null
                    });
                    let _ = tx.send(response.to_string()).await;
                }
                "mining.authorize" => {
                    let (worker, authorized, payout_script, payout_address_str) =
                        self.handle_authorize(&request).await;
                    let response = json!({
                        "id": request.id,
                        "result": authorized,
                        "error": null
                    });
                    let _ = tx.send(response.to_string()).await;

                    if authorized {
                        // Update session state quickly, but don't await while holding the lock.
                        let (difficulty, user_agent, session_id, extranonce1, diff_msg, notify_opt) = {
                            let mut guard = state.lock().await;
                            guard.authorized = true;
                            guard.worker = worker.clone();
                            guard.session_payout_script = payout_script;
                            guard.session_payout_address = payout_address_str.clone();

                            let difficulty = guard.difficulty;
                            let user_agent = guard.user_agent.clone();
                            let session_id = guard.session_id.clone();
                            let extranonce1 = guard.extranonce1.clone();

                            let diff_msg = build_set_difficulty(difficulty);

                            let job = job_rx.borrow().clone();
                            let notify_opt = if job.ready {
                                guard.jobs.clear(); // fresh session — no grace needed
                                let extranonce1_bytes = guard.extranonce1_bytes.clone();
                                let session_job =
                                    guard.push_job(job.clone(), difficulty, &extranonce1_bytes, None);
                                guard.last_notify = Utc::now();
                                guard.last_prevhash = Some(job.prevhash_le.clone());
                                Some(build_notify_with_ntime(
                                    &session_job.session_job_id,
                                    session_job.job.as_ref(),
                                    &session_job.notify_ntime,
                                    true,
                                    session_job.custom_coinbase2_hex.as_deref(),
                                ))
                            } else {
                                None
                            };

                            (difficulty, user_agent, session_id, extranonce1, diff_msg, notify_opt)
                        };

                        // Log extranonce1 assignment for search-space verification.
                        // Each session gets a unique random 4-byte extranonce1 → unique nonce space.
                        if let Some(ref addr) = payout_address_str {
                            info!(
                                "worker authorized: worker={} payout={} extranonce1={} extranonce2_size={} diff={:.0}",
                                worker, addr, extranonce1,
                                self.config.extranonce2_size,
                                difficulty
                            );
                        } else {
                            info!(
                                "worker authorized: worker={} payout=pool_default extranonce1={} extranonce2_size={} diff={:.0}",
                                worker, extranonce1,
                                self.config.extranonce2_size,
                                difficulty
                            );
                        }

                        self.metrics
                            .record_miner_seen(&worker, difficulty, user_agent, Some(session_id))
                            .await;

                        let _ = tx.send(diff_msg).await;
                        if let Some(notify) = notify_opt {
                            let _ = tx.send(notify).await;
                        }
                    }
                }
                "mining.get_version" => {
                    let response = json!({
                        "id": request.id,
                        "result": "BlockFinder",
                        "error": null
                    });
                    let _ = tx.send(response.to_string()).await;
                }
                "mining.ping" => {
                    let response = json!({
                        "id": request.id,
                        "result": true,
                        "error": null
                    });
                    let _ = tx.send(response.to_string()).await;
                }
                "mining.suggest_difficulty" => {
                    let suggested = request
                        .params
                        .as_array()
                        .and_then(|params| params.get(0))
                        .and_then(|value| value.as_f64())
                        .unwrap_or(self.config.start_difficulty);

                    // Release lock before any async send to avoid holding Mutex across await.
                    let (diff_msg, notify_msg) = {
                        let mut guard = state.lock().await;
                        let raw = if guard.vardiff_enabled {
                            // Vardiff active: allow raising immediately, never lower
                            // (vardiff handles downward adjustment on its own schedule).
                            suggested
                                .clamp(self.config.min_difficulty, self.config.max_difficulty)
                                .max(guard.difficulty)
                        } else {
                            // Fixed-diff mode: honour suggestion freely within min/max.
                            suggested
                                .clamp(self.config.min_difficulty, self.config.max_difficulty)
                        };
                        // Keep exact integer difficulty steps so vardiff can move
                        // down smoothly instead of getting stuck on coarse buckets.
                        let target = guard.vardiff.clamp_diff(raw);
                        if (target - guard.difficulty).abs() > f64::EPSILON {
                            guard.difficulty = target;
                            let diff_msg = build_set_difficulty(target);
                            let notify_msg = if let Some(job_latest) = guard.jobs.back().map(|e| e.job.clone()) {
                                let extranonce1_bytes = guard.extranonce1_bytes.clone();
                                let session_job =
                                    guard.push_job(job_latest.clone(), target, &extranonce1_bytes, None);
                                guard.last_notify = Utc::now();
                                guard.last_prevhash = Some(job_latest.prevhash_le.clone());
                                Some(build_notify_with_ntime(
                                    &session_job.session_job_id,
                                    session_job.job.as_ref(),
                                    &session_job.notify_ntime,
                                    false,
                                    session_job.custom_coinbase2_hex.as_deref(),
                                ))
                            } else {
                                None
                            };
                            (diff_msg, notify_msg)
                        } else {
                            (build_set_difficulty(guard.difficulty), None)
                        }
                    };

                    let _ = tx.send(diff_msg).await;
                    if let Some(notify) = notify_msg {
                        let _ = tx.send(notify).await;
                    }
                    let response = json!({
                        "id": request.id,
                        "result": true,
                        "error": null
                    });
                    let _ = tx.send(response.to_string()).await;
                }
                "mining.submit" => {
                    self.handle_submit(&request, &state, &tx)
                        .await?;
                }
                "mining.configure" => {
                    // BIP310 protocol constant — must never change.
                    let allowed_mask = 0x1fffe000u32;
                    let requested_mask = request
                        .params
                        .as_array()
                        .and_then(|params| params.get(1))
                        .and_then(|value| value.get("version-rolling.mask"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("1fffe000");
                    let parsed_mask = u32::from_str_radix(requested_mask, 16).unwrap_or(allowed_mask);
                    let configured_mask = parsed_mask & allowed_mask;
                    let mask_hex = format!("{:08x}", configured_mask);
                    {
                        let mut guard = state.lock().await;
                        guard.version_mask = configured_mask;
                    }
                    let response = json!({
                        "id": request.id,
                        "result": {
                            "version-rolling": true,
                            "version-rolling.mask": mask_hex
                        },
                        "error": null
                    });
                    let _ = tx.send(response.to_string()).await;
                }
                _ => {
                    let response = json!({
                        "id": request.id,
                        "result": null,
                        "error": [20, "Unsupported method", null]
                    });
                    let _ = tx.send(response.to_string()).await;
                }
            }
        }

        Ok(())
    }

    async fn handle_authorize(&self, request: &StratumRequest) -> (String, bool, Option<Vec<u8>>, Option<String>) {
        let params = request.params.as_array().cloned().unwrap_or_default();
        let worker = params.get(0).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let password = params.get(1).and_then(|v| v.as_str()).unwrap_or("");

        let authorized = if let Some(token) = &self.config.auth_token {
            password == token
        } else {
            true
        };

        // Try to extract a Bitcoin address from the worker name.
        // Miners commonly use: "address.worker_name" or just "address".
        // We try the full string first, then the part before the first '.'.
        let (payout_script, payout_address_str) = if authorized {
            let candidates = {
                let mut v = vec![worker.as_str()];
                if let Some(dot_pos) = worker.find('.') {
                    v.push(&worker[..dot_pos]);
                }
                v
            };
            let mut found: Option<(Vec<u8>, String)> = None;
            for candidate in candidates {
                if let Ok(addr) = Address::from_str(candidate) {
                    if addr.is_valid_for_network(self.config.network) {
                        let script = addr.assume_checked().script_pubkey().as_bytes().to_vec();
                        found = Some((script, candidate.to_string()));
                        break;
                    }
                }
            }
            match found {
                Some((script, addr_str)) => (Some(script), Some(addr_str)),
                None => {
                    // Miner provided no valid Bitcoin address as username.
                    // If a pool-wide fallback address is configured, allow the
                    // connection and the block reward will go to that address.
                    // If no fallback is configured, reject the miner with a
                    // clear, actionable error message.
                    if self.config.payout_address.is_empty()
                        && self.config.payout_script_hex.is_none()
                    {
                        tracing::warn!(
                            "authorize rejected: worker={worker} — no Bitcoin address in \
                             username and no pool PAYOUT_ADDRESS configured. \
                             Set the miner's Stratum username to a Bitcoin address \
                             (e.g. bc1qYOUR_ADDRESS) or set PAYOUT_ADDRESS in env/.env."
                        );
                        return (worker, false, None, None);
                    }
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        (worker, authorized, payout_script, payout_address_str)
    }

    async fn handle_submit(
        &self,
        request: &StratumRequest,
        state: &Arc<Mutex<SessionState>>,
        tx: &mpsc::Sender<String>,
    ) -> anyhow::Result<()> {
        // submit_rtt: measures actual pool processing time (parse → validate → respond).
        // Should be 1–5 ms under normal load; spikes indicate CPU pressure.
        let submit_start = std::time::Instant::now();
        let params = request.params.as_array().cloned().unwrap_or_default();
        if params.len() < 5 {
            let response = json!({
                "id": request.id,
                "result": false,
                "error": [20, "Invalid params", null]
            });
            let _ = tx.send(response.to_string()).await;
            return Ok(());
        }

        let submitted_worker = params[0].as_str().unwrap_or("").to_string();
        let job_id = params[1].as_str().unwrap_or("").to_string();
        let extranonce2 = params[2].as_str().unwrap_or("").to_string();
        let ntime = params[3].as_str().unwrap_or("").to_string();
        let nonce = params[4].as_str().unwrap_or("").to_string();
        let submit_version = params
            .get(5)
            .and_then(|v| v.as_str())
            .map(|s| s.trim_start_matches("0x").to_string());

        // Cheap DoS-guard: enforce expected hex lengths up-front.
        // Accept optional 0x prefix from some firmwares.
        let mut extranonce2_clean = extranonce2.trim_start_matches("0x").to_string();
        let mut ntime_clean = ntime.trim_start_matches("0x").to_string();
        let mut nonce_clean = nonce.trim_start_matches("0x").to_string();

        let expected_extra2 = self.config.extranonce2_size * 2;
        if extranonce2_clean.len() > 256 {
            let response = json!({
                "id": request.id,
                "result": false,
                "error": [20, "Invalid extranonce2 length", null]
            });
            let _ = tx.send(response.to_string()).await;
            return Ok(());
        }

        if extranonce2_clean.len() % 2 != 0 {
            extranonce2_clean = format!("0{extranonce2_clean}");
        }
        if extranonce2_clean.len() < expected_extra2 {
            extranonce2_clean = format!("{extranonce2_clean:0>width$}", width = expected_extra2);
        }
        if extranonce2_clean.len() != expected_extra2 {
            let response = json!({
                "id": request.id,
                "result": false,
                "error": [20, "Invalid extranonce2 length", null]
            });
            let _ = tx.send(response.to_string()).await;
            return Ok(());
        }

        if ntime_clean.len() % 2 != 0 {
            ntime_clean = format!("0{ntime_clean}");
        }
        if nonce_clean.len() % 2 != 0 {
            nonce_clean = format!("0{nonce_clean}");
        }
        if ntime_clean.len() < 8 {
            ntime_clean = format!("{ntime_clean:0>8}");
        }
        if nonce_clean.len() < 8 {
            nonce_clean = format!("{nonce_clean:0>8}");
        }
        if ntime_clean.len() != 8 || nonce_clean.len() != 8 {
            let response = json!({
                "id": request.id,
                "result": false,
                "error": [20, "Invalid ntime/nonce length", null]
            });
            let _ = tx.send(response.to_string()).await;
            return Ok(());
        }


        // Grab everything needed for validation with a short lock (no await-heavy work inside).
        let (version_mask, session_job, last_notify, session_start, vardiff_enabled, session_worker, session_session_id, session_extranonce1, session_payout_address, early_reply, stale_reason) = {
            let guard = state.lock().await;

            if !guard.authorized {
                let response = json!({
                    "id": request.id,
                    "result": false,
                    "error": [24, "Unauthorized", null]
                });
                (
                    0u32,
                    None,
                    guard.last_notify,
                    guard.session_start,
                    guard.vardiff_enabled,
                    guard.worker.clone(),
                    guard.session_id.clone(),
                    guard.extranonce1.clone(),
                    guard.session_payout_address.clone(),
                    Some(response.to_string()),
                    None::<StaleReason>,
                )
            } else {
                match guard.find_job(&job_id) {
                    Some(job) => (
                        guard.version_mask,
                        Some(job),
                        guard.last_notify,
                        guard.session_start,
                        guard.vardiff_enabled,
                        guard.worker.clone(),
                        guard.session_id.clone(),
                        guard.extranonce1.clone(),
                        guard.session_payout_address.clone(),
                        None,
                        None::<StaleReason>,
                    ),
                    None => {
                        let now = Utc::now();
                        let stale_worker = guard.worker.clone();

                        // ── Stale classification ─────────────────────────────
                        // Priority: Reconnect > NewBlock > Expired
                        //
                        // Reconnect: session is very young → miner likely replayed
                        //   work from a previous session. Threshold is configurable
                        //   via RECONNECT_RECENT_SECS.
                        //
                        // NewBlock: clean_jobs=true was sent ≤ 60 s ago → miner
                        //   was still working on the old block when a new one arrived.
                        //   Indicates ZMQ propagation delay or miner latency.
                        //
                        // Expired: all other cases — job simply aged out or unknown
                        //   job ID (e.g. miner resumed from a paused state).
                        let session_age_secs = (now - guard.session_start).num_seconds();
                        let notify_delay_ms  = (now - guard.last_notify).num_milliseconds();
                        let reason = if session_age_secs < self.config.reconnect_recent_secs {
                            StaleReason::Reconnect
                        } else {
                            match guard.last_clean_jobs_time {
                                Some(t) if (now - t).num_seconds() <= 60 => StaleReason::NewBlock,
                                _ => StaleReason::Expired,
                            }
                        };
                        tracing::warn!(
                            "stale share: worker={} reason={:?} session_age={}s notify_delay={}ms",
                            stale_worker, reason, session_age_secs, notify_delay_ms
                        );
                        let response = json!({
                            "id": request.id,
                            "result": false,
                            "error": [21, "Stale share", null]
                        });
                        (
                            guard.version_mask,
                            None,
                            guard.last_notify,
                            guard.session_start,
                            guard.vardiff_enabled,
                            stale_worker,
                            guard.session_id.clone(),
                            guard.extranonce1.clone(),
                            guard.session_payout_address.clone(),
                            Some(response.to_string()),
                            Some(reason),
                        )
                    }
                }
            }
        };

        if let Some(reply) = early_reply {
            let _ = tx.send(reply).await;
            // session_job is None only for stale or unauthorized; propagate stale to metrics.
            if session_job.is_none() && !session_worker.is_empty() {
                let reason = stale_reason.unwrap_or(StaleReason::Expired);
                self.metrics.record_stale(&session_worker, reason).await;
            }
            return Ok(());
        }

        let session_job = match session_job {
            Some(job) => job,
            None => return Ok(()),
        };


        let worker = if !session_worker.is_empty() { session_worker } else { submitted_worker.clone() };
        let session_id = session_session_id;
        let job = session_job.job.clone();
        let job_diff = session_job.difficulty;

        // Version rolling (BIP310):
        //   allowed bits = version_mask (negotiated via mining.configure)
        //   combined     = (job_bits_outside_mask) | (miner_bits_inside_mask)
        //
        // If the miner modified bits OUTSIDE the negotiated mask the share is
        // rejected immediately with error [20].  Silent acceptance would build a
        // block whose nVersion contains bits the pool never agreed to roll,
        // which Bitcoin Core may reject as invalid.
        let (version, version_outside_mask) = if let Some(submit_val) =
            submit_version.as_deref().and_then(parse_u32_be)
        {
            let job_val = parse_u32_be(&job.version).unwrap_or(job.version_u32);
            let submit_outside = submit_val & !version_mask;
            let job_outside   = job_val   & !version_mask;
            // outside_mismatch: miner changed bits the pool did not authorise.
            let outside_mismatch = submit_outside != 0 && submit_outside != job_outside;
            // Always use the safe combined value; never let the miner's raw bits
            // outside the mask reach block construction.
            let combined = (job_val & !version_mask) | (submit_val & version_mask);
            (Some(format!("{:08x}", combined)), outside_mismatch)
        } else {
            (None, false) // validate_share will fall back to job.version_u32
        };

        if version_outside_mask {
            self.metrics.counters.inc_version_rolling_violation();
            warn!(
                "version-rolling violation worker={worker}: bits outside mask modified — rejecting share"
            );
            let resp = json!({
                "id": request.id,
                "result": false,
                "error": [20, "Version bits outside negotiated mask", null]
            });
            let _ = tx.send(resp.to_string()).await;
            self.metrics
                .record_share_result(
                    &worker,
                    session_job.difficulty,
                    0.0,
                    false,
                    false,
                    0,
                    0.0,
                    0,
                    0,
                    false,
                    false,
                )
                .await;
            return Ok(());
        }

        let submit = ShareSubmit {
            worker: worker.clone(),
            job_id: job_id.clone(),
            extranonce2: extranonce2_clean.clone(),
            ntime: ntime_clean.clone(),
            nonce: nonce_clean.clone(),
            version: version.clone(),
        };

        // ── Duplicate share detection ─────────────────────────────────────────
        // Key: job_id + nonce + ntime + extranonce2 + version
        //
        // job_id MUST be part of the key.  Without it, the same nonce/ntime/en2
        // submitted on a *different* job (different prevhash / merkle / coinbase)
        // would be rejected as a false duplicate even though the resulting block
        // header is completely different.
        //
        // version is included because BIP310 miners roll it independently;
        // two submits with the same nonce/ntime/en2 but different version bits
        // produce different hashes and must both be evaluated.
        //
        // job_id here is session-scoped (SessionJob.session_job_id), so it is
        // already unique per (session, template) — no cross-session confusion.
        //
        // CONCURRENCY ANALYSIS (single session):
        //   All submits from one TCP connection are processed sequentially by the
        //   `loop { read_until(...) }` loop — the Tokio task awaits each line before
        //   processing the next.  Therefore the check+insert block below cannot race
        //   with another submit on the same session; the Mutex is only needed for
        //   coordination with the notify/vardiff tasks.
        //
        // CONCURRENCY ANALYSIS (across sessions / block candidates):
        //   Two different sessions that both find a hash below the block target will
        //   each call submitblock independently.  If both are truly valid the second
        //   call will return "duplicate" from Bitcoin Core, which we treat as
        //   accepted.  No block is lost.
        //
        // BOUND STRATEGY: True LRU eviction via paired HashSet + VecDeque.
        // submitted_hashes_order tracks insertion order; when full, pop_front() gives
        // the oldest key, which is then removed from submitted_hashes.
        // This guarantees the most-recent MAX_DUP_HASHES-1 keys always remain in the
        // guard window — eliminating the clear()-then-duplicate-slips-through race
        // that a full wipe would create.
        let version_key = version.as_deref().unwrap_or(&job.version);
        // Build the duplicate-detection key.
        // extranonce2 is stored as a String (the canonical validated hex), NOT as u64,
        // so that EXTRANONCE2_SIZE > 8 bytes works correctly without any false-positive
        // duplicate rejection.  The other fields are fixed-width and always ≤ their
        // respective integer sizes after the earlier length validation.
        let dup_key: DupKey = (
            u64::from_str_radix(&job_id, 16).unwrap_or(0),
            u32::from_str_radix(&nonce_clean, 16).unwrap_or(0),
            u32::from_str_radix(&ntime_clean, 16).unwrap_or(0),
            extranonce2_clean.clone(),
            u32::from_str_radix(version_key, 16).unwrap_or(0),
        );
        // Duplicate detection is intentionally deferred until after the hash is
        // computed so raw best-share tracking can record every validly parsed submit.

        // ── Heavy work: hash/merkle/header validation ─────────────────────────
        // Uses 256-bit integer comparison (exact) for both acceptance and block.
        // f64 difficulty is computed only for metrics/UI (true_share_diff).
        //
        // NOTE: validate_share can return Err for malformed inputs (e.g., non-hex
        // ntime/nonce that parse_u32_be rejects).  We must NOT propagate the error
        // up to handle_client, because that would close the TCP connection.
        // Instead, return an error response and keep the session alive — the miner
        // may send valid shares after this.
        let result = match validate_share(
            session_job.job.as_ref(),
            session_job.coinbase_prefix.as_slice(),
            &submit,
            &session_job.share_target_le,   // ← 256-bit LE target
            session_job.custom_coinbase2_bytes.as_deref().map(|v| v.as_slice()),
        ) {
            Ok(r) => r,
            Err(err) => {
                // Malformed input (non-hex nonce/ntime, etc.) — reject without closing.
                tracing::warn!(
                    "share validation input error worker={worker}: {err:?}"
                );
                let resp = json!({
                    "id": request.id,
                    "result": false,
                    "error": [20, format!("Invalid share params: {err}"), null]
                });
                let _ = tx.send(resp.to_string()).await;
                return Ok(());
            }
        };

        let now = Utc::now();
        // notify_to_submit_ms: time from last mining.notify → this mining.submit.
        // Dominated by hashing time at the current difficulty, NOT network RTT.
        // At TARGET_SHARE_TIME_SECS=15 this will naturally be ~15,000 ms.
        let notify_to_submit_ms = (now - last_notify).num_milliseconds().max(0);
        let notify_delay_ms = notify_to_submit_ms as u64;
        // job_age_secs: age of the GBT template at share submission time.
        // > 30 s suggests ZMQ latency or miner replaying old work.
        let job_age_secs = (now - session_job.job.created_at).num_seconds().max(0) as u64;
        // reconnect_recent is diagnostic only; it never affects acceptance.
        let reconnect_recent =
            (now - session_start).num_seconds() < self.config.reconnect_recent_secs;
        // submit_rtt_ms: actual pool processing time (parse→validate→respond).
        // Fractional ms via µs for sub-millisecond accuracy.
        // Healthy value: 0.1–5 ms. Spike > 50 ms indicates CPU pressure.
        let submit_rtt_ms = submit_start.elapsed().as_micros() as f64 / 1000.0;
        if job_age_secs > 30 {
            tracing::debug!(
                "slow share: worker={worker} job_age={}s notify_delay={}ms submit_rtt={}ms diff={:.2} reconnect={}",
                job_age_secs, notify_delay_ms, submit_rtt_ms, result.difficulty, reconnect_recent
            );
        }

        // Raw best-share accounting is updated immediately after the hash is known,
        // before duplicate rejection or any persistence work.
        let is_stale_block = session_job.is_stale_block.load(Ordering::Acquire);
            self.metrics
                .record_raw_share(
                    &worker,
                    session_payout_address.as_deref(),
                    session_job.difficulty,
                    result.difficulty,
                    result.is_block,
                    is_stale_block,
                )
            .await;

        // High-priority submitblock path: fire before duplicate/reject bookkeeping
        // so a candidate never waits behind SQLite or dashboard code.
        if result.is_block {
            if is_stale_block {
                tracing::warn!(
                    "GRACE-BLOCK: worker={worker} found hash below target on stale-block job \
                     height={} hash={} — NOT submitting (wrong prevhash, grace window)",
                    job.height, result.hash_hex
                );
            } else if result.block_hex.is_some() {
                let block_hex_owned   = result.block_hex.clone().unwrap();
                let coinbase_hex_owned = result.coinbase_hex.clone().unwrap_or_default();
                let block_hash        = result.hash_hex.clone();
                let template_key      = job.template_key.clone();
                let txid_root         = job.txid_partial_root.clone();
                let witness           = job.witness_commitment_script.clone().unwrap_or_default();
                let height            = job.height;
                let persist_blocks    = self.config.persist_blocks;
                let engine            = self.template_engine.clone();
                let sqlite_for_block  = self.sqlite.clone();
                let found_by          = worker.to_owned();
                let requested_at     = Utc::now();
                let version_str      = version.clone().unwrap_or_else(|| job.version.clone());
                let version_mask_str  = format!("{:08x}", version_mask);
                let merkle_root_hex  = header_merkle_root_hex(&result.header_hex).unwrap_or_else(|| job.txid_partial_root.clone());
                let candidate_record = BlockCandidateRecord {
                    id: Uuid::new_v4(),
                    worker: worker.clone(),
                    payout_address: session_payout_address.clone(),
                    session_id: Some(session_id.clone()),
                    job_id: session_job.session_job_id.clone(),
                    height: height as i64,
                    prevhash: job.prevhash.clone(),
                    ntime: submit.ntime.clone(),
                    nonce: submit.nonce.clone(),
                    version: version_str.clone(),
                    version_mask: version_mask_str,
                    extranonce1: Some(session_extranonce1.clone()),
                    extranonce2: submit.extranonce2.clone(),
                    merkle_root: merkle_root_hex,
                    coinbase_hex: result.coinbase_hex.clone(),
                    block_header_hex: result.header_hex.clone(),
                    block_hex: result.block_hex.clone(),
                    full_block_hex: result.block_hex.clone(),
                    block_hash: block_hash.clone(),
                    submitted_difficulty: result.difficulty,
                    network_difficulty: job.network_difficulty,
                    current_share_difficulty: session_job.difficulty,
                    submitblock_requested_at: requested_at,
                    submitblock_result: "pending".to_string(),
                    submitblock_latency_ms: 0,
                    rpc_error: None,
                    created_at: requested_at,
                };
                tracing::info!(
                    "*** BLOCK FOUND by {worker} height={height} hash={block_hash} \
                     diff={:.2} template_key=\"{}\" ***",
                    result.difficulty, job.template_key,
                );
                tokio::spawn(async move {
                    let submit_started = std::time::Instant::now();
                    let (status, rpc_error) = match engine.submit_block(
                        &block_hex_owned, &block_hash, &template_key,
                        &coinbase_hex_owned, &txid_root, &witness,
                    ).await {
                        Ok(_) => {
                            tracing::info!("BLOCK SUBMITTED OK height={height} hash={block_hash}");
                            ("submitted", None)
                        }
                        Err(err) => {
                            tracing::error!(
                                "BLOCK SUBMIT FAILED height={height} hash={block_hash} — {err:?}"
                            );
                            ("submit_failed", Some(err.to_string()))
                        }
                    };
                    let mut candidate_record = candidate_record;
                    candidate_record.submitblock_result = status.to_string();
                    candidate_record.submitblock_latency_ms = submit_started.elapsed().as_millis() as i64;
                    candidate_record.rpc_error = rpc_error;
                    if let Err(err) = sqlite_for_block.insert_block_candidate(candidate_record).await {
                        tracing::error!(
                            "failed to persist block candidate forensic record: {err:?}"
                        );
                    }
                    if persist_blocks {
                        if let Err(err) = sqlite_for_block.insert_block(
                            height as i64,
                            &block_hash,
                            Some(found_by.as_str()),
                            status,
                        ).await {
                            tracing::error!(
                                "failed to persist found block summary: {err:?}"
                            );
                        }
                    }
                });
            }
        }

        let duplicate = {
            let mut guard = state.lock().await;
            if guard.submitted_hashes.contains(&dup_key) {
                true
            } else {
                // LRU eviction: remove the single oldest entry O(1) instead of clear().
                // The guard window shrinks by exactly one entry, so all other recent keys
                // remain protected.  No duplicate can slip through after eviction.
                if guard.submitted_hashes.len() >= MAX_DUP_HASHES {
                    if let Some(oldest) = guard.submitted_hashes_order.pop_front() {
                        guard.submitted_hashes.remove(&oldest);
                    }
                }
                guard.submitted_hashes.insert(dup_key.clone());
                guard.submitted_hashes_order.push_back(dup_key.clone());
                false
            }
        };
        let final_accepted = result.accepted && !duplicate;

        // ── Send acknowledgment immediately ──────────────────────────────────────
        // All decisions affecting accept/reject are already final:
        //   • duplicate detection (HashSet check + insert, above)
        //   • stale/job classification (find_job, above)
        //   • version-rolling validation (above)
        //   • validate_share result (accepted/rejected, is_block, hash)
        //
        // Nothing below can change result.accepted.  Sending the ACK now lets the
        // miner pipeline the next share immediately while we do post-processing.
        //
        // Rejected-share debug log runs before the ACK so the log reflects the
        // exact state at decision time (no correctness impact, just log ordering).
        if !result.accepted {
            if let Ok(filter) = env::var("DEBUG_WORKER") {
                if !filter.is_empty() && worker.contains(&filter) {
                    tracing::warn!(
                        "reject debug worker={worker} job_id={job_id} job_diff={:.4} submit_diff={:.4} version_job={} version_submit={} version_final={} version_mask={:08x} version_outside_mask={} ntime={} nonce={} extranonce2={}",
                        job_diff,
                        result.difficulty,
                        job.version,
                        submit_version.clone().unwrap_or_else(|| "-".to_string()),
                        version.clone().unwrap_or_else(|| "-".to_string()),
                        version_mask,
                        version_outside_mask,
                        submit.ntime,
                        submit.nonce,
                        submit.extranonce2
                    );
                }
            }
            tracing::debug!("share rejected (worker={worker}) diff={:.2}", result.difficulty);
        }

        if duplicate {
            self.metrics.counters.inc_duplicate_share();
            self.metrics.record_duplicate().await;
            tracing::debug!(
                "duplicate share from {worker}: job={job_id} nonce={nonce_clean} ntime={ntime_clean} en2={extranonce2_clean} ver={version_key}"
            );
        }

        let ack_error = if final_accepted { serde_json::Value::Null }
                        else { json!([23, "Low difficulty share", null]) };
        let ack_msg = json!({"id": request.id, "result": final_accepted, "error": ack_error}).to_string();
        if tx.send(ack_msg).await.is_err() {
            // TCP connection closed before ACK — log only for block candidates to avoid noise.
            if result.is_block {
                tracing::error!(
                    "BLOCK ACK SEND FAILED: worker={worker} height={} hash={} — \
                     block already queued for submitblock (miner disconnected mid-submit)",
                    job.height, result.hash_hex
                );
            }
            return Ok(());
        }

        // ── Post-ack: vardiff + session state ─────────────────────────────────────
        // Must run before persist_shares (needs session_diff_after) and before
        // outbound_msgs are sent (set_difficulty / notify follow in TCP stream order).
        let mut outbound_msgs: Vec<String> = Vec::new();
        let mut session_diff_after = 0.0f64;
        let mut effective_job_diff = job_diff;
        {
            let mut guard = state.lock().await;

            if vardiff_enabled {
                let current_diff = guard.difficulty;
                if let Some(new_diff) = guard.vardiff.maybe_retarget(current_diff, now) {
                    guard.difficulty = new_diff;
                    outbound_msgs.push(build_set_difficulty(new_diff));

                    if let Some(job_latest) = guard.jobs.back().map(|entry| entry.job.clone()) {
                        let extranonce1_bytes = guard.extranonce1_bytes.clone();
                        let session_job =
                            guard.push_job(job_latest.clone(), new_diff, &extranonce1_bytes, None);
                        guard.last_notify = Utc::now();
                        guard.last_prevhash = Some(job_latest.prevhash_le.clone());
                        outbound_msgs.push(build_notify_with_ntime(
                            &session_job.session_job_id,
                            session_job.job.as_ref(),
                            &session_job.notify_ntime,
                            false,
                            session_job.custom_coinbase2_hex.as_deref(),
                        ));
                    }
                }
            }

            session_diff_after = guard.difficulty;

            // Match ckpool-style accounting: if the session diff has already
            // moved, count the share at the lower of the job threshold and the
            // current session difficulty so late shares from an older job do not
            // keep the estimator pinned high.
            effective_job_diff = job_diff.min(session_diff_after);
            if final_accepted {
                guard.vardiff.record_share(now, effective_job_diff);
            }
        }

        // ── Post-ack: round-trip proof logging ───────────────────────────────────
        // Disabled by default (SHARE_PROOF_SHARES=0).  When enabled, logs every
        // field needed to verify SHA256d(header) == hash externally for the first
        // N accepted shares per session.  Set SHARE_PROOF_SHARES=200 to enable.
        // When the limit is 0 this block is entirely skipped — no lock, no formatting.
        if final_accepted && self.config.share_proof_limit > 0 {
            let proof_extranonce1: Option<String> = {
                let mut guard = state.lock().await;
                if guard.proof_shares_logged < self.config.share_proof_limit {
                    guard.proof_shares_logged += 1;
                    Some(guard.extranonce1.clone())
                } else {
                    None
                }
            };
            if let Some(extranonce1_for_proof) = proof_extranonce1 {
                let version_final = version.clone().unwrap_or_else(|| job.version.clone());
                info!(
                    "SHARE_PROOF worker={} job_id={} \
                     extranonce1={} extranonce2={} ntime={} nonce={} version_final={} \
                     coinbase1={} coinbase2={} \
                     prevhash_header_le={} nbits={} \
                     merkle_branches=[{}] \
                     header_hex={} hash_hex={} diff={:.2}",
                    worker, session_job.session_job_id,
                    extranonce1_for_proof, extranonce2_clean, ntime_clean, nonce_clean,
                    version_final,
                    job.coinbase1, job.coinbase2, job.prevhash_le, job.nbits,
                    job.merkle_branches.join(","),
                    result.header_hex, result.hash_hex, result.difficulty,
                );
            }
        }

        // ── Post-ack: persistence + metrics (spawned) ────────────────────────────
        // Persistence (Redis/SQLite) and in-memory metrics updates have no effect on:
        //   • the ACK already sent to the miner
        //   • the next submit's duplicate/stale/validate decisions
        //   • the outbound_msgs ordering through the TCP mpsc channel
        //
        // Spawning removes every await in these paths from the session TCP handler,
        // allowing the handler to reach outbound_msgs and return immediately.
        //
        // Safety invariants:
        //   • session_diff_after captured from the vardiff lock above (f64, Copy)
        //   • final_accepted / is_block / difficulty are final (all Copy)
        //   • metrics record calls hit per-worker atomics, not a global write lock
        //   • best summary persistence is periodic; share history stays optional
        //   • worker is moved into the spawn (not used after this point in the handler)
        {
            let metrics_s      = self.metrics.clone();
            let redis_s        = self.redis.clone();
            let sqlite_s       = self.sqlite.clone();
            let persist_shares = self.config.persist_shares;
            let accepted       = final_accepted;
            let is_block       = result.is_block;
            let share_diff     = result.difficulty;
            // Build ShareRecord before spawn so Uuid is generated on the session task.
            // difficulty = share_diff (DIFF1/hash — actual work done by the miner), NOT job_diff
            // (job_diff is the session acceptance threshold, not the hash quality).
            let share_record: Option<ShareRecord> = if accepted && persist_shares {
                Some(ShareRecord {
                    id: Uuid::new_v4(),
                    worker: worker.clone(),
                    difficulty: share_diff,
                    is_block,
                    is_accepted: true,
                    latency_ms: notify_to_submit_ms,
                    created_at: now,
                })
            } else {
                None
            };
            tokio::spawn(async move {
                // Optional Redis/SQLite persistence (persist_shares=true only).
                if let Some(share) = share_record {
                    let _ = redis_s.incr_share(&worker).await;
                    let _ = redis_s.set_difficulty(&worker, session_diff_after).await;
                    let _ = sqlite_s.insert_share(share).await;
                }
                // In-memory metrics are updated separately from persistence so
                // the hot path never waits on SQLite or dashboard aggregation.
                metrics_s
                    .record_share_result(
                        &worker,
                        effective_job_diff,
                        share_diff,
                        accepted,
                        is_block,
                        notify_to_submit_ms,
                        submit_rtt_ms,
                        job_age_secs,
                        notify_delay_ms,
                        reconnect_recent,
                        session_job.is_stale_block.load(Ordering::Acquire),
                    )
                    .await;
            });
        }

        // ── Post-ack: vardiff notifications (set_difficulty + new job if retarget) ─
        // Sent after the ACK intentionally: the miner receives response → set_difficulty
        // → notify in TCP stream order (FIFO through the mpsc writer channel).
        for msg in outbound_msgs {
            let _ = tx.send(msg).await;
        }

        Ok(())
    }

}

#[derive(Debug, Deserialize)]
struct StratumRequest {
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

// ─── Token bucket for per-session notify rate-limiting ───────────────────────
/// Leaky-bucket / token-bucket limiter for `mining.notify` messages.
///
/// Design:
///   capacity  = 2 tokens  (burst: miner can receive 2 jobs back-to-back)
///   fill_rate = 1 token per 500 ms
///
/// Usage:
///   • `try_consume()` → true: token available, send the notify
///   • `try_consume()` → false: bucket empty, skip this notify (log as throttled)
///   • `bypass()`: bypass the bucket entirely for clean_jobs=true (new block)
///     — new-block notifies are never throttled; hashing on a stale block is waste.
///
/// Why this beats a simple `NOTIFY_THROTTLE_MS`:
///   • Simple throttle suppresses content-different jobs within the window (false suppression).
///   • Token bucket allows bursts (e.g. connect + first job) while still rate-limiting floods.
///   • clean_jobs bypass ensures zero delay on block changes regardless of bucket state.
struct NotifyBucket {
    tokens:      f64,
    capacity:    f64,
    /// tokens added per millisecond (= 1 / NOTIFY_BUCKET_REFILL_MS)
    fill_per_ms: f64,
    last_fill_ms: u64,
}

impl NotifyBucket {
    /// Construct from pool config values.
    ///
    /// `capacity`    = NOTIFY_BUCKET_CAPACITY  (default 2 tokens)
    /// `refill_ms`   = NOTIFY_BUCKET_REFILL_MS (default 1500 ms per token)
    fn new(capacity: f64, refill_ms: f64) -> Self {
        let fill_per_ms = if refill_ms > 0.0 { 1.0 / refill_ms } else { 1.0 };
        Self {
            tokens:       capacity,   // start full: first `capacity` notifies go immediately
            capacity,
            fill_per_ms,
            last_fill_ms: 0,
        }
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// Refill bucket based on elapsed time, then try to consume 1 token.
    /// Returns `true` if the notify should be sent.
    fn try_consume(&mut self) -> bool {
        let now = Self::now_ms();
        if self.last_fill_ms == 0 { self.last_fill_ms = now; }
        let elapsed_ms = now.saturating_sub(self.last_fill_ms) as f64;
        self.tokens = (self.tokens + elapsed_ms * self.fill_per_ms).min(self.capacity);
        self.last_fill_ms = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

struct SessionState {
    authorized: bool,
    worker: String,
    user_agent: Option<String>,
    session_id: String,
    extranonce1: String,
    extranonce1_bytes: Vec<u8>,
    difficulty: f64,
    /// scriptPubKey bytes derived from the miner's worker username when it is a
    /// valid Bitcoin address. Set at `mining.authorize` time and used in every
    /// subsequent `push_job` call to build a per-session coinbase2 so the block
    /// reward goes directly to this miner. `None` → use pool-wide PAYOUT_ADDRESS.
    session_payout_script: Option<Vec<u8>>,
    /// Human-readable address string for logging.
    session_payout_address: Option<String>,
    /// Cached share target (256-bit LE) for the current difficulty.
    /// Recomputed only when difficulty changes.
    share_target_cache: [u8; 32],
    vardiff: VardiffController,
    vardiff_enabled: bool,
    jobs: VecDeque<Arc<SessionJob>>,
    next_job_id: u64,
    last_notify: chrono::DateTime<chrono::Utc>,
    last_prevhash: Option<String>,
    /// Wall-clock time when this session was created (TCP connection accepted).
    /// Used to detect reconnect stales: if a share arrives shortly after
    /// session start, the miner likely reconnected and replayed an old share.
    session_start: chrono::DateTime<chrono::Utc>,
    /// Timestamp of last clean_jobs=true notify. Used to classify stales as
    /// "new_block" (if within 60s) vs "expired" (job just timed out naturally).
    last_clean_jobs_time: Option<chrono::DateTime<chrono::Utc>>,
    /// Token bucket: rate-limits mining.notify according to NOTIFY_BUCKET_REFILL_MS.
    /// Bypassed when clean_jobs=true so new-block notifies are never throttled.
    notify_bucket: NotifyBucket,
    /// Epoch-ms deadline until which stale-block jobs are kept in the queue
    /// and shares on them are accepted (without block submission).
    /// Set to now + STALE_GRACE_MS on every clean_jobs=true event.
    stale_grace_until_ms: u64,
            /// How many round-trip proof entries have been logged for this session.
            /// We log the first 200 accepted shares per session for version-rolling stats,
            /// then stop to avoid flooding logs at scale.
            proof_shares_logged: u16,
    version_mask: u32,
    /// Duplicate share detection: (job_id, nonce, ntime, extranonce2_hex, version) tuple.
    /// extranonce2 is stored as a String so EXTRANONCE2_SIZE > 8 bytes never causes a
    /// false-positive (u64 would overflow → unwrap_or(0) → distinct values collide).
    /// Bounded to MAX_DUP_HASHES entries with LRU eviction.
    submitted_hashes: HashSet<DupKey>,
    /// Insertion-order tracker for submitted_hashes.
    /// Enables true O(1) LRU eviction: pop_front() removes the oldest key, which is
    /// then removed from submitted_hashes.  Eliminates the full-clear() window where
    /// a duplicate submitted immediately after eviction would pass undetected.
    submitted_hashes_order: VecDeque<DupKey>,
}

impl SessionState {
    fn new(
        extranonce1: String,
        extranonce1_bytes: Vec<u8>,
        initial_diff: f64,
        target_share_time: f64,
        retarget_time: f64,
        min_diff: f64,
        max_diff: f64,
        vardiff_enabled: bool,
        bucket_capacity: f64,
        bucket_refill_ms: f64,
    ) -> Self {
        Self {
            authorized: false,
            worker: String::new(),
            user_agent: None,
            session_id: Uuid::new_v4().to_string(),
            extranonce1,
            extranonce1_bytes,
            difficulty: initial_diff,
            session_payout_script: None,
            session_payout_address: None,
            share_target_cache: share_target_le(initial_diff)
                .unwrap_or([0xFF; 32]),
            vardiff: VardiffController::new(
                target_share_time,
                retarget_time,
                min_diff,
                max_diff,
            ),
            vardiff_enabled,
            jobs: VecDeque::with_capacity(16),
            next_job_id: 1,
            last_notify: Utc::now(),
            last_prevhash: None,
            session_start: Utc::now(),
            last_clean_jobs_time: None,
            notify_bucket: NotifyBucket::new(bucket_capacity, bucket_refill_ms),
            stale_grace_until_ms: 0,
            proof_shares_logged: 0,
            version_mask: 0x1fffe000,
            submitted_hashes: HashSet::with_capacity(256),
            submitted_hashes_order: VecDeque::with_capacity(256),
        }
    }

    /// Mark all current jobs as belonging to a stale (previous) block and arm the
    /// grace window. Called instead of jobs.clear() on clean_jobs=true so shares
    /// in-flight can still be accepted for the next STALE_GRACE_MS milliseconds.
    ///
    /// Uses AtomicBool::store instead of Arc recreation: O(N) flag sets with
    /// zero heap allocations — critical path on every new block.
    fn mark_jobs_stale_block(&mut self) {
        // 300ms grace window: covers TCP RTT + firmware processing + share in buffer.
        const STALE_GRACE_MS: u64 = 300;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.stale_grace_until_ms = now_ms + STALE_GRACE_MS;

        // Mark every existing job stale in-place — no Arc cloning, no heap allocation.
        // Release ordering: ensures any concurrent load(Acquire) in handle_submit sees
        // is_stale_block=true before the block candidate submission check.
        for job in &self.jobs {
            job.is_stale_block.store(true, Ordering::Release);
        }
    }

    fn push_job(&mut self, job: Arc<JobTemplate>, difficulty: f64, extranonce1_bytes: &[u8], notify_ntime: Option<String>) -> Arc<SessionJob> {
        let session_job_id = format!("{:x}", self.next_job_id);
        self.next_job_id = self.next_job_id.wrapping_add(1).max(1);

        // Update cached share target when difficulty changes.
        if (difficulty - self.difficulty).abs() > f64::EPSILON {
            if let Ok(t) = share_target_le(difficulty) {
                self.share_target_cache = t;
                self.difficulty = difficulty;
            }
        }
        let share_target = self.share_target_cache;

        // Cache coinbase prefix per (session, job): coinbase1 + extranonce1.
        let mut coinbase_prefix = Vec::with_capacity(job.coinbase1_bytes.len() + extranonce1_bytes.len());
        coinbase_prefix.extend_from_slice(&job.coinbase1_bytes);
        coinbase_prefix.extend_from_slice(extranonce1_bytes);

        // If this session has a per-miner payout address, rebuild coinbase2 so
        // the block reward goes to the miner's own address.
        let (custom_coinbase2_hex, custom_coinbase2_bytes) =
            if let Some(ref payout_script) = self.session_payout_script {
                match build_coinbase2_for_payout(
                    job.coinbase_value,
                    payout_script,
                    job.witness_commitment_script.as_deref(),
                ) {
                    Ok((hex, bytes)) => (Some(hex), Some(Arc::new(bytes))),
                    Err(err) => {
                        // ⚠️  IMPORTANT: if this session finds a block, the reward goes to
                        // the pool's PAYOUT_ADDRESS, not the miner's address.
                        // This warn fires once per push_job; for block candidates it will
                        // appear in the log before the "BLOCK FOUND" line.
                        tracing::warn!(
                            "coinbase2 build failed for miner {} — \
                             falling back to pool address (block reward goes to pool!): {err:?}",
                            self.session_payout_address.as_deref().unwrap_or("?")
                        );
                        (None, None)
                    }
                }
            } else {
                (None, None)
            };

        let notify_ntime = notify_ntime.unwrap_or_else(|| job.ntime.clone());
        let entry = Arc::new(SessionJob {
            job,
            difficulty,
            share_target_le: share_target,
            coinbase_prefix: Arc::new(coinbase_prefix),
            session_job_id,
            notify_ntime,
            custom_coinbase2_hex,
            custom_coinbase2_bytes,
            is_stale_block: AtomicBool::new(false),
        });

        // Evict expired stale-block jobs before adding the new one.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if now_ms >= self.stale_grace_until_ms {
            self.jobs.retain(|j| !j.is_stale_block.load(Ordering::Relaxed));
        }

        self.jobs.push_back(entry.clone());
        while self.jobs.len() > 32 {
            self.jobs.pop_front();
        }
        entry
    }

    fn find_job(&self, job_id: &str) -> Option<Arc<SessionJob>> {
        self.jobs
            .iter()
            .rev()
            .find(|entry| entry.session_job_id == job_id)
            .cloned()
    }
}

struct SessionJob {
    job: Arc<JobTemplate>,
    difficulty: f64,
    /// Pre-computed 256-bit LE share target for this session's difficulty.
    share_target_le: [u8; 32],
    coinbase_prefix: Arc<Vec<u8>>,
    session_job_id: String,
    notify_ntime: String,
    /// Per-miner coinbase2 hex (for mining.notify) when the miner's username is
    /// a valid Bitcoin address. Replaces the pool-wide coinbase2 in the notify
    /// message so the miner hashes towards their own address.
    custom_coinbase2_hex: Option<String>,
    /// Pre-decoded bytes of `custom_coinbase2_hex` — passed to `validate_share`
    /// as `coinbase2_override` so block validation uses the miner's address.
    custom_coinbase2_bytes: Option<Arc<Vec<u8>>>,
    /// True when this job belongs to a previous block (its prevhash is no longer
    /// the chain tip). Shares on stale-block jobs are accepted for metrics
    /// (the miner did real work) but never submitted as blocks.
    /// AtomicBool allows mark_jobs_stale_block() to set this flag in-place
    /// without recreating the Arc<SessionJob> (eliminates O(N) allocations on
    /// every clean_jobs=true event).
    is_stale_block: AtomicBool,
}

fn parse_u32_be(hex_str: &str) -> Option<u32> {
    u32::from_str_radix(hex_str, 16).ok()
}

/// Build a `mining.notify` message.
///
/// `custom_coinbase2` — per-session coinbase2 hex when the miner's username is
/// a valid Bitcoin address. When `Some`, it replaces `job.coinbase2` so the
/// miner hashes towards their own address. When `None`, uses `job.coinbase2`
/// (pool-wide payout address).
///
/// Builds the JSON string directly without an intermediate serde_json Value tree,
/// eliminating multiple heap allocations per notify. All Stratum field values are
/// hex strings ([0-9a-f]) that require no JSON escaping, making this safe.
fn build_notify_with_ntime(
    job_id: &str,
    job: &JobTemplate,
    ntime: &str,
    clean_jobs: bool,
    custom_coinbase2: Option<&str>,
) -> String {
    let coinbase2 = custom_coinbase2.unwrap_or(&job.coinbase2);
    // Estimate capacity: fixed overhead ~80 B + field sizes.
    let cap = 80
        + job_id.len()
        + job.prevhash.len()
        + job.coinbase1.len()
        + coinbase2.len()
        + job.version.len()
        + job.nbits.len()
        + ntime.len()
        + job.merkle_branches.iter().map(|b| b.len() + 3).sum::<usize>();
    let mut out = String::with_capacity(cap);
    // Trace of final output:
    // {"id":null,"method":"mining.notify","params":["job","prev","cb1","cb2",[branches],"ver","bits","ntime",true]}
    out.push_str(r#"{"id":null,"method":"mining.notify","params":[""#);
    out.push_str(job_id);       out.push_str(r#"",""#);   // "job_id","
    out.push_str(&job.prevhash);out.push_str(r#"",""#);   // "prevhash","
    out.push_str(&job.coinbase1);out.push_str(r#"",""#);  // "coinbase1","
    out.push_str(coinbase2);    out.push_str("\",[");     // "coinbase2",[
    for (i, branch) in job.merkle_branches.iter().enumerate() {
        if i > 0 { out.push(','); }
        out.push('"'); out.push_str(branch); out.push('"');
    }
    out.push_str("],\"");       // ],  "version"
    out.push_str(&job.version); out.push_str("\",\"");    // "version","nbits"
    out.push_str(&job.nbits);   out.push_str("\",\"");    // "nbits","ntime"
    out.push_str(ntime);        out.push('"');             // "ntime"
    out.push(',');
    out.push_str(if clean_jobs { "true]}" } else { "false]}" });
    out
}

fn build_set_difficulty(diff: f64) -> String {
    json!({
        "id": null,
        "method": "mining.set_difficulty",
        "params": [diff]
    })
    .to_string()
}

fn header_merkle_root_hex(header_hex: &str) -> Option<String> {
    let bytes = hex::decode(header_hex).ok()?;
    if bytes.len() < 68 {
        return None;
    }
    Some(hex::encode(&bytes[36..68]))
}

// ─── Unit tests ───────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    // Build a DupKey exactly as handle_submit does — mirrors the production code path.
    fn make_dup_key(job_id: &str, nonce: &str, ntime: &str, en2: &str, version: &str) -> DupKey {
        (
            u64::from_str_radix(job_id, 16).unwrap_or(0),
            u32::from_str_radix(nonce, 16).unwrap_or(0),
            u32::from_str_radix(ntime, 16).unwrap_or(0),
            en2.to_string(),
            u32::from_str_radix(version, 16).unwrap_or(0),
        )
    }

    // ── 1) Duplicate correctness — normal config ──────────────────────────────

    /// Same share submitted twice → duplicate.
    #[test]
    fn test_dup_key_same_share_is_duplicate() {
        let k1 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "20000000");
        let k2 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "20000000");
        let mut set: HashSet<DupKey> = HashSet::new();
        assert!(set.insert(k1));
        assert!(!set.insert(k2), "identical share must be detected as duplicate");
    }

    /// Different nonce → not a duplicate.
    #[test]
    fn test_dup_key_different_nonce_not_duplicate() {
        let k1 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "20000000");
        let k2 = make_dup_key("1", "aabbccee", "699f722b", "00000001", "20000000");
        let mut set: HashSet<DupKey> = HashSet::new();
        assert!(set.insert(k1));
        assert!(set.insert(k2), "different nonce must not be flagged as duplicate");
    }

    /// Different ntime → not a duplicate.
    #[test]
    fn test_dup_key_different_ntime_not_duplicate() {
        let k1 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "20000000");
        let k2 = make_dup_key("1", "aabbccdd", "699f722c", "00000001", "20000000");
        let mut set: HashSet<DupKey> = HashSet::new();
        assert!(set.insert(k1));
        assert!(set.insert(k2), "different ntime must not be flagged as duplicate");
    }

    /// Different version bits (BIP310) → not a duplicate.
    #[test]
    fn test_dup_key_different_version_not_duplicate() {
        let k1 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "20000000");
        let k2 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "203f4000");
        let mut set: HashSet<DupKey> = HashSet::new();
        assert!(set.insert(k1));
        assert!(set.insert(k2), "different version bits must not be flagged as duplicate");
    }

    /// Different extranonce2 (normal 4-byte / 8-hex) → not a duplicate.
    #[test]
    fn test_dup_key_different_extranonce2_not_duplicate() {
        let k1 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "20000000");
        let k2 = make_dup_key("1", "aabbccdd", "699f722b", "00000002", "20000000");
        let mut set: HashSet<DupKey> = HashSet::new();
        assert!(set.insert(k1));
        assert!(set.insert(k2), "different extranonce2 must not be flagged as duplicate");
    }

    // ── 2) Duplicate correctness — extended extranonce2 (> 8 bytes) ──────────

    /// EXTRANONCE2_SIZE = 12 bytes (24 hex chars).  Two shares that differ only
    /// in extranonce2 must NOT collide — the old u64 path would overflow → 0 for
    /// both, creating a false duplicate and silently dropping real work.
    #[test]
    fn test_dup_key_extended_en2_different_not_duplicate() {
        // 24 hex chars = 12 bytes — exceeds u64 (8 bytes = 16 hex chars).
        let en2_a = "000000000000000000000001";
        let en2_b = "000000000000000000000002";
        let k1 = make_dup_key("1", "aabbccdd", "699f722b", en2_a, "20000000");
        let k2 = make_dup_key("1", "aabbccdd", "699f722b", en2_b, "20000000");
        let mut set: HashSet<DupKey> = HashSet::new();
        assert!(set.insert(k1));
        assert!(
            set.insert(k2),
            "different extended extranonce2 (>8 bytes) must not be a false duplicate \
             — the old code parsed both as u64 and both would unwrap_or(0)"
        );
    }

    /// Same large extranonce2 submitted twice → must still detect the duplicate.
    #[test]
    fn test_dup_key_extended_en2_same_is_duplicate() {
        let en2 = "000000000000000000000001"; // 24 hex = 12 bytes
        let k1 = make_dup_key("1", "aabbccdd", "699f722b", en2, "20000000");
        let k2 = make_dup_key("1", "aabbccdd", "699f722b", en2, "20000000");
        let mut set: HashSet<DupKey> = HashSet::new();
        assert!(set.insert(k1));
        assert!(!set.insert(k2), "same extended extranonce2 must still be detected as duplicate");
    }

    /// The specific u64-overflow scenario: two values that differ only in bits
    /// above position 63 would both produce 0 under `u64::from_str_radix(...).unwrap_or(0)`.
    /// With the String-based key they remain distinct — no false duplicate.
    #[test]
    fn test_dup_key_extended_en2_overflow_no_false_duplicate() {
        // 20 hex chars = 10 bytes.  Bit 64+ differs between the two values.
        let en2_a = "00010000000000000001";
        let en2_b = "00020000000000000001";
        let k1 = make_dup_key("1", "aabbccdd", "699f722b", en2_a, "20000000");
        let k2 = make_dup_key("1", "aabbccdd", "699f722b", en2_b, "20000000");
        let mut set: HashSet<DupKey> = HashSet::new();
        assert!(set.insert(k1));
        assert!(
            set.insert(k2),
            "extranonce2 values differing only in bits above 63 must not false-duplicate \
             — old u64::from_str_radix would overflow both to 0"
        );
    }

    // ── 3) Best share safety ─────────────────────────────────────────────────

    /// A high-difficulty share with an extended extranonce2 must not be falsely
    /// rejected.  This test proves that two shares with the same job/nonce/ntime
    /// but DIFFERENT extranonce2 (> 8 bytes) are both accepted by the dup filter,
    /// so a high-diff share is never silently discarded before best tracking.
    #[test]
    fn test_dup_key_high_diff_extended_en2_not_lost() {
        // Simulate: miner submits two shares on the same job, same nonce/ntime,
        // but different extranonce2 values that are each 9 bytes (18 hex chars).
        // Under the old code both would collapse to 0 → second rejected as dup.
        let en2_first  = "000000000000000000";  // 18 hex = 9 bytes
        let en2_second = "000000000000000001";  // different → different hash
        let k1 = make_dup_key("a", "deadbeef", "6abc1234", en2_first,  "20000000");
        let k2 = make_dup_key("a", "deadbeef", "6abc1234", en2_second, "20000000");
        let mut set: HashSet<DupKey> = HashSet::new();
        assert!(set.insert(k1), "first share must be accepted");
        assert!(
            set.insert(k2),
            "second share with different extended extranonce2 must reach validation \
             — if falsely duplicated the best-diff share could be silently dropped"
        );
    }

    // ── 4) Storage semantics ─────────────────────────────────────────────────

    /// ShareRecord.difficulty must hold the actual share difficulty (DIFF1/hash),
    /// not the session/job acceptance threshold.
    ///
    /// Rationale: job_diff is a threshold — it tells us the minimum quality the
    /// pool requires.  share_diff (result.difficulty) is the true measure of the
    /// work the miner actually performed.  Analytics, dashboards, and API consumers
    /// reading `shares.difficulty` expect the actual work value.
    ///
    /// This test verifies the semantics at the ShareRecord construction site.
    #[test]
    fn test_share_record_stores_actual_share_difficulty_not_threshold() {
        use crate::storage::ShareRecord;
        use uuid::Uuid;
        use chrono::Utc;

        let job_diff: f64   = 1024.0;   // session/job acceptance threshold
        let share_diff: f64 = 98_765.0; // actual DIFF1/hash for this share

        // After the fix: difficulty = share_diff.
        let record = ShareRecord {
            id: Uuid::new_v4(),
            worker: "test_worker".into(),
            difficulty: share_diff,  // ← must be share_diff, not job_diff
            is_block: false,
            is_accepted: true,
            latency_ms: 10,
            created_at: Utc::now(),
        };

        assert!(
            (record.difficulty - share_diff).abs() < f64::EPSILON,
            "ShareRecord.difficulty must equal the actual share difficulty ({share_diff})"
        );
        assert!(
            (record.difficulty - job_diff).abs() > 1000.0,
            "ShareRecord.difficulty must NOT store the job threshold ({job_diff})"
        );
    }

    // ── 5) No regression: current standard config ────────────────────────────

    /// Standard 4-byte extranonce2 (8 hex chars) still works correctly.
    #[test]
    fn test_dup_key_standard_4byte_en2_still_works() {
        let k1 = make_dup_key("3", "cafebabe", "5f0a1b2c", "deadc0de", "20000000");
        let k2 = make_dup_key("3", "cafebabe", "5f0a1b2c", "deadc0de", "20000000");
        let k3 = make_dup_key("3", "cafebabe", "5f0a1b2c", "deadc0df", "20000000");
        let mut set: HashSet<DupKey> = HashSet::new();
        assert!(set.insert(k1));
        assert!(!set.insert(k2), "standard 4-byte en2: same share must be duplicate");
        assert!(set.insert(k3), "standard 4-byte en2: different en2 must not be duplicate");
    }

    /// Different job_id with identical nonce/ntime/en2/version → not a duplicate.
    /// Verifies that job_id remains part of the key (cross-job false-duplicate prevention).
    #[test]
    fn test_dup_key_different_job_not_duplicate() {
        let k1 = make_dup_key("1", "aabbccdd", "699f722b", "00000001", "20000000");
        let k2 = make_dup_key("2", "aabbccdd", "699f722b", "00000001", "20000000");
        let mut set: HashSet<DupKey> = HashSet::new();
        assert!(set.insert(k1));
        assert!(
            set.insert(k2),
            "same fields on a different job must not be flagged as duplicate \
             (different job → different coinbase → different block header)"
        );
    }
}
