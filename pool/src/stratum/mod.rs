use std::collections::VecDeque;
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};
use uuid::Uuid;

use std::str::FromStr;

use bitcoin::Address;

use crate::config::Config;
use crate::metrics::{MetricsStore, StaleReason};
use crate::share::{share_target_le, validate_share, ShareSubmit};
use std::collections::HashSet;
use crate::storage::{RedisStore, ShareRecord, SqliteStore};
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
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if writer.write_all(msg.as_bytes()).await.is_err() {
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
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

                // ── Token-bucket notify gate ────────────────────────────────
                // Check auth and determine clean_jobs with a short lock.
                // For clean_jobs=true (new block) we bypass the bucket entirely —
                // latency on a block change is critical.
                // For mempool-only updates (clean_jobs=false) we consume a token;
                // if the bucket is empty, skip this notify (the miner is already
                // working on the latest prevhash and its nonce space is still valid).
                let (authorized, clean_jobs_peek) = {
                    let guard = notify_state.lock().await;
                    let clean = guard
                        .last_prevhash
                        .as_deref()
                        .map_or(true, |prev| prev != job.prevhash_le);
                    (guard.authorized, clean)
                };

                if !authorized {
                    continue;
                }

                // For non-block updates, check (and consume) a token.
                // This is done outside the main lock to avoid holding it across time.
                if !clean_jobs_peek {
                    let token_ok = {
                        let mut guard = notify_state.lock().await;
                        guard.notify_bucket.try_consume()
                    };
                    if !token_ok {
                        // Token bucket empty: rate-limit this mempool-only update.
                        // The miner is already working on the latest prevhash.
                        notify_counters.inc_notify_rate_limited();
                        continue;
                    }
                }

                let notify = {
                    let mut guard = notify_state.lock().await;
                    if !guard.authorized {
                        continue;
                    }

                    // Re-check clean_jobs inside the lock (state may have changed during await).
                    let clean_jobs = guard
                        .last_prevhash
                        .as_deref()
                        .map_or(true, |prev| prev != job.prevhash_le);

                    if clean_jobs {
                        guard.jobs.clear();
                        // Record time of last clean_jobs for stale classification.
                        guard.last_clean_jobs_time = Some(Utc::now());
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

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let request: StratumRequest = match serde_json::from_str(&line) {
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
                        let (difficulty, user_agent, extranonce1, diff_msg, notify_opt) = {
                            let mut guard = state.lock().await;
                            guard.authorized = true;
                            guard.worker = worker.clone();
                            guard.session_payout_script = payout_script;
                            guard.session_payout_address = payout_address_str.clone();

                            let difficulty = guard.difficulty;
                            let user_agent = guard.user_agent.clone();
                            let extranonce1 = guard.extranonce1.clone();

                            let diff_msg = build_set_difficulty(difficulty);

                            let job = job_rx.borrow().clone();
                            let notify_opt = if job.ready {
                                guard.jobs.clear();
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

                            (difficulty, user_agent, extranonce1, diff_msg, notify_opt)
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
                            .record_miner_seen(&worker, difficulty, user_agent, Some(extranonce1))
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
                        "result": "Solo",
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
                        // Round to nearest power-of-2 for clean, predictable steps.
                        let target = guard.vardiff.nearest_p2(raw);
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
                None => (None, None),
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
        let (version_mask, session_job, last_notify, session_start, vardiff_enabled, session_worker, early_reply, stale_reason) = {
            let mut guard = state.lock().await;

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
                        None,
                        None::<StaleReason>,
                    ),
                    None => {
                        let now = Utc::now();
                        let stale_worker = guard.worker.clone();

                        // ── Stale classification ─────────────────────────────
                        // Priority: Reconnect > NewBlock > Expired
                        //
                        // Reconnect: session < 30 s old → miner reconnected and
                        //   submitted a share from its previous session's job.
                        //   These stales are expected on reconnect storms and should
                        //   not be confused with ZMQ latency stales.
                        //
                        // NewBlock: clean_jobs=true was sent ≤ 60 s ago → miner
                        //   was still working on the old block when a new one arrived.
                        //   Indicates ZMQ propagation delay or miner latency.
                        //
                        // Expired: all other cases — job simply aged out or unknown
                        //   job ID (e.g. miner resumed from a paused state).
                        let session_age_secs = (now - guard.session_start).num_seconds();
                        let notify_delay_ms  = (now - guard.last_notify).num_milliseconds();
                        let reason = if session_age_secs < 30 {
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
            self.metrics.record_share(
                &worker, session_job.difficulty, 0.0, false, false,
                0, 0.0, 0, 0, false,
            ).await;
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
        let version_key = version.as_deref().unwrap_or(&job.version);
        let dup_key = format!("{}:{}:{}:{}:{}", job_id, nonce_clean, ntime_clean, extranonce2_clean, version_key);
        {
            let mut guard = state.lock().await;
            if guard.submitted_hashes.contains(&dup_key) {
                self.metrics.counters.inc_duplicate_share();
                tracing::debug!(
                    "duplicate share from {worker}: job={job_id} nonce={nonce_clean} ntime={ntime_clean} en2={extranonce2_clean} ver={version_key}"
                );
                let resp = json!({"id": request.id, "result": false,
                                  "error": [22, "Duplicate share", null]});
                let _ = tx.send(resp.to_string()).await;
                self.metrics.record_share(&worker, session_job.difficulty, 0.0, false, false, 0, 0.0, 0, 0, false).await;
                return Ok(());
            }
            // Bound the set to prevent unbounded memory growth.
            if guard.submitted_hashes.len() >= 2048 {
                guard.submitted_hashes.clear();
            }
            guard.submitted_hashes.insert(dup_key);
        }

        // ── Heavy work: hash/merkle/header validation ─────────────────────────
        // Uses 256-bit integer comparison (exact) for both acceptance and block.
        // f64 difficulty is computed only for metrics/UI (true_share_diff).
        let result = validate_share(
            session_job.job.as_ref(),
            session_job.coinbase_prefix.as_slice(),
            &submit,
            &session_job.share_target_le,   // ← 256-bit LE target
            session_job.custom_coinbase2_bytes.as_deref().map(|v| v.as_slice()),
        ).context("share validation")?;

        let now = Utc::now();
        // notify_to_submit_ms: time from last mining.notify → this mining.submit.
        // Dominated by hashing time at the current difficulty, NOT network RTT.
        // At TARGET_SHARE_TIME_SECS=10 this will naturally be ~10,000 ms.
        let notify_to_submit_ms = (now - last_notify).num_milliseconds().max(0);
        let notify_delay_ms = notify_to_submit_ms as u64;
        // job_age_secs: age of the GBT template at share submission time.
        // > 30 s suggests ZMQ latency or miner replaying old work.
        let job_age_secs = (now - session_job.job.created_at).num_seconds().max(0) as u64;
        // reconnect_recent: session started < 30 s ago.
        let reconnect_recent = (now - session_start).num_seconds() < 30;
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

        // Update vardiff/session state with a short lock, and collect any outgoing messages.
        let mut outbound_msgs: Vec<String> = Vec::new();
        let mut session_diff_after = 0.0f64;
        {
            let mut guard = state.lock().await;

            if result.accepted {
                // Record accepted shares for vardiff timing.
                guard.vardiff.record_share(now, job_diff);
            }

            // Retarget can be triggered even without acceptance to keep behavior consistent.
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
        }

        // ── Round-trip proof logging (first 3 accepted shares per session) ─────
        // Logs every field needed to verify SHA256d(header) == hash externally.
        // Verification: python3 verify_share.py <job fields> → must match hash_hex.
        if result.accepted {
            let should_log_proof = {
                let mut guard = state.lock().await;
                if guard.proof_shares_logged < 200 {
                    guard.proof_shares_logged += 1;
                    true
                } else {
                    false
                }
            };
            if should_log_proof {
                // extranonce1 is stored in SessionState; read it with a brief lock.
                let extranonce1_for_proof = {
                    let guard = state.lock().await;
                    guard.extranonce1.clone()
                };
                // version_final: the actually-used nVersion (after rolling or job default)
                let version_final = version.clone()
                    .unwrap_or_else(|| job.version.clone());
                info!(
                    "SHARE_PROOF worker={} job_id={} \
                     extranonce1={} extranonce2={} ntime={} nonce={} version_final={} \
                     coinbase1={} coinbase2={} \
                     prevhash_header_le={} nbits={} \
                     merkle_branches=[{}] \
                     header_hex={} hash_hex={} diff={:.2}",
                    worker,
                    session_job.session_job_id,
                    extranonce1_for_proof,  // 4-byte random per-session (unique)
                    extranonce2_clean,      // miner-chosen (extends nonce space)
                    ntime_clean,
                    nonce_clean,
                    version_final,
                    job.coinbase1,          // prefix: version+vin up to extranonce placeholder
                    job.coinbase2,          // suffix: vout + locktime
                    // prevhash_le = full byte-reversal of GBT hash = bytes 4..36 of header
                    // (NOT word-swapped Stratum format — pool uses this directly in build_header)
                    job.prevhash_le,
                    job.nbits,
                    job.merkle_branches.join(","),
                    result.header_hex,      // 80B header — SHA256d(this) must == hash_hex
                    result.hash_hex,
                    result.difficulty,
                );
            }
        }

        // Block submit / optional persistence / metrics are all outside the lock.
        if result.accepted {
            if result.is_block {
                if let Some(block_hex) = result.block_hex.as_deref() {
                    let coinbase_hex_ref = result.coinbase_hex.as_deref().unwrap_or("");
                    tracing::info!(
                        "*** BLOCK FOUND by {worker} height={} hash={} diff={:.2} template_key=\"{}\" ***",
                        job.height,
                        result.hash_hex,
                        result.difficulty,
                        job.template_key,
                    );
                    let status = match self.template_engine.submit_block(
                        block_hex,
                        &result.hash_hex,
                        &job.template_key,
                        coinbase_hex_ref,
                        &job.txid_partial_root,
                        job.witness_commitment_script.as_deref().unwrap_or(""),
                    ).await {
                        Ok(_) => {
                            tracing::info!(
                                "BLOCK SUBMITTED OK height={} hash={}",
                                job.height, result.hash_hex
                            );
                            "submitted"
                        }
                        Err(err) => {
                            tracing::error!(
                                "BLOCK SUBMIT FAILED height={} hash={} — {err:?}",
                                job.height, result.hash_hex
                            );
                            "submit_failed"
                        }
                    };
                    if self.config.persist_blocks {
                        let _ = self
                            .sqlite
                            .insert_block(job.height as i64, &result.hash_hex, status)
                            .await;
                    }
                }
            }

            if self.config.persist_shares {
                let _ = self.redis.incr_share(&worker).await;
                let _ = self.redis.set_difficulty(&worker, session_diff_after).await;

                let share = ShareRecord {
                    id: Uuid::new_v4(),
                    worker: worker.clone(),
                    difficulty: job_diff,
                    is_block: result.is_block,
                    is_accepted: true,
                    latency_ms: notify_to_submit_ms,
                    created_at: now,
                };
                let _ = self.sqlite.insert_share(share).await;
            }

            // Persist new best_difficulty to SQLite so it survives pool restarts.
            // This is called very rarely (only when all-time best improves), so
            // the DB write overhead is negligible.
            if let Some(new_best) = self.metrics
                .record_share(
                    &worker, job_diff, result.difficulty, true, result.is_block,
                    notify_to_submit_ms, submit_rtt_ms, job_age_secs, notify_delay_ms, reconnect_recent,
                ).await
            {
                let _ = self.sqlite.upsert_worker_best(&worker, new_best).await;
            }
        } else {
            self.metrics
                .record_share(
                    &worker, job_diff, result.difficulty, false, false,
                    notify_to_submit_ms, submit_rtt_ms, job_age_secs, notify_delay_ms, reconnect_recent,
                ).await;
        }

        // Send any queued difficulty/notify messages.
        for msg in outbound_msgs {
            let _ = tx.send(msg).await;
        }

        let error = if result.accepted {
            serde_json::Value::Null
        } else {
            json!([23, "Low difficulty share", null])
        };

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
            tracing::debug!(
                "share rejected (worker={worker}) diff={:.2}",
                result.difficulty
            );
        }

        let response = json!({
            "id": request.id,
            "result": result.accepted,
            "error": error
        });
        let _ = tx.send(response.to_string()).await;

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
    /// tokens added per millisecond (= 1 / 500)
    fill_per_ms: f64,
    last_fill_ms: u64,
}

impl NotifyBucket {
    /// Construct from pool config values.
    ///
    /// `capacity`    = NOTIFY_BUCKET_CAPACITY  (default 2 tokens)
    /// `refill_ms`   = NOTIFY_BUCKET_REFILL_MS (default 500 ms per token)
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
    /// Used to detect reconnect stales: if a share arrives within 30 s of
    /// session start, the miner likely reconnected and replayed an old share.
    session_start: chrono::DateTime<chrono::Utc>,
    /// Timestamp of last clean_jobs=true notify. Used to classify stales as
    /// "new_block" (if within 60s) vs "expired" (job just timed out naturally).
    last_clean_jobs_time: Option<chrono::DateTime<chrono::Utc>>,
    /// Token bucket: rate-limits mining.notify to ≤ 1/500ms burst=2.
    /// Bypassed when clean_jobs=true so new-block notifies are never throttled.
    notify_bucket: NotifyBucket,
            /// How many round-trip proof entries have been logged for this session.
            /// We log the first 200 accepted shares per session for version-rolling stats,
            /// then stop to avoid flooding logs at scale.
            proof_shares_logged: u16,
    version_mask: u32,
    /// Duplicate share detection: (job_id, nonce, ntime, extranonce2, version) per session.
    /// job_id is session-scoped so the key is unique per (session, job, hash-attempt).
    /// Bounded to last 2048 entries to cap memory.
    submitted_hashes: HashSet<String>,
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
            proof_shares_logged: 0,
            version_mask: 0x1fffe000,
            submitted_hashes: HashSet::with_capacity(256),
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
                        tracing::warn!(
                            "failed to build custom coinbase2 for {}: {err:?} — falling back to pool address",
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
        });

        self.jobs.push_back(entry.clone());
        while self.jobs.len() > 16 {
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
fn build_notify_with_ntime(
    job_id: &str,
    job: &JobTemplate,
    ntime: &str,
    clean_jobs: bool,
    custom_coinbase2: Option<&str>,
) -> String {
    let coinbase2 = custom_coinbase2.unwrap_or(&job.coinbase2);
    json!({
        "id": null,
        "method": "mining.notify",
        "params": [
            job_id,
            job.prevhash,
            job.coinbase1,
            coinbase2,
            job.merkle_branches,
            job.version,
            job.nbits,
            ntime,
            clean_jobs
        ]
    })
    .to_string()
}

fn build_set_difficulty(diff: f64) -> String {
    json!({
        "id": null,
        "method": "mining.set_difficulty",
        "params": [diff]
    })
    .to_string()
}
