mod api;
mod build_info;
mod config;
mod hash;
mod metrics;
mod rpc;
mod share;
mod storage;
mod stratum;
mod template;
mod vardiff;

use std::sync::Arc;

use anyhow::Context;
use tracing::{error, info};

use crate::api::ApiServer;
use crate::config::Config;
use crate::metrics::MetricsStore;
use crate::rpc::RpcClient;
use crate::storage::{RedisStore, SqliteStore};
use crate::stratum::StratumServer;
use crate::template::TemplateEngine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    eprintln!("blackhole-pool booting");
    eprintln!("build info: {}", crate::build_info::one_line());
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".parse().expect("valid default log filter"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .init();

    if let Err(err) = run().await {
        eprintln!("blackhole-pool fatal: {err:?}");
        return Err(err);
    }

    eprintln!("blackhole-pool exited cleanly");
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    eprintln!("blackhole-pool init: loading config");
    let config = Config::from_env().context("load config")?;

    if config.solo_mode {
        eprintln!(
            "INFO: SOLO_MODE=true — solo hunter profile enabled (raw best-share tracking, low-lock hot path, best-summary persistence)."
        );
    }

    // ── Payout address info ───────────────────────────────────────────────────
    if config.payout_address.is_empty() && config.payout_script_hex.is_none() {
        eprintln!(
            "INFO: PAYOUT_ADDRESS is not set — per-miner mode active. \
             Every miner MUST use a Bitcoin address as their Stratum username. \
             Miners without a valid address will be rejected at authorization."
        );
    } else if !config.payout_address.is_empty() {
        eprintln!(
            "INFO: PAYOUT_ADDRESS={} — used as fallback for miners without \
             a Bitcoin address in their username.",
            config.payout_address
        );
    }

    // ── Security guard: AUTH_TOKEN ────────────────────────────────────────────
    // Emit a prominent warning (or hard-fail) when AUTH_TOKEN is empty and the
    // stratum is reachable from the network.  Mining correctness is unaffected.
    if config.auth_token.is_none() {
        let open_bind = config.stratum_bind == "0.0.0.0" || config.stratum_bind == "::";
        if config.require_auth_token {
            anyhow::bail!(
                "REQUIRE_AUTH_TOKEN=true but AUTH_TOKEN is not set — refusing to start. \
                 Set AUTH_TOKEN in env/.env or set REQUIRE_AUTH_TOKEN=false to allow \
                 unauthenticated miners."
            );
        } else if open_bind {
            eprintln!(
                "WARNING: AUTH_TOKEN is not set and stratum is bound to {} — \
                 any device on your network can connect and mine. \
                 Set AUTH_TOKEN in env/.env to restrict access, or set \
                 REQUIRE_AUTH_TOKEN=true to enforce it at startup.",
                config.stratum_bind
            );
        }
    }

    let metrics = MetricsStore::new();
    eprintln!("blackhole-pool init: connecting storage backends");

    // Only connect storage backends when they are actually needed.
    let redis = if config.persist_shares {
        RedisStore::connect(config.redis_url.as_deref()).await?
    } else {
        RedisStore::connect(None).await?
    };

    let sqlite_needed = config.persist_shares || config.persist_blocks || config.persist_best;
    let sqlite = if sqlite_needed {
        SqliteStore::connect(config.database_url.as_deref()).await?
    } else {
        SqliteStore::connect(None).await?
    };

    eprintln!("blackhole-pool init: storage ready");

    // Restore persisted best-share records for all known workers so that
    // the all-time best counters survive pool restarts.
    if sqlite.is_enabled() {
        match sqlite.load_worker_bests().await {
            Ok(bests) => {
                for (worker, (best_submitted, best_accepted, best_block_candidate)) in bests {
                    metrics
                        .set_worker_best(&worker, best_submitted, best_accepted, best_block_candidate)
                        .await;
                }
                info!("loaded persisted best-share records from SQLite");
            }
            Err(err) => {
                tracing::warn!("could not load worker bests from SQLite: {err:?}");
            }
        }
    }

    let rpc = RpcClient::new(
        config.rpc_url.clone(),
        config.rpc_user.clone(),
        config.rpc_pass.clone(),
    );

    let template_engine = Arc::new(TemplateEngine::new(
        config.clone(),
        rpc.clone(),
        metrics.clone(),
        metrics.counters.clone(),
    ));

    eprintln!("blackhole-pool init: warming template engine");
    template_engine.start().await?;
    eprintln!("blackhole-pool init: template engine ready");

    let stratum = StratumServer::new(
        config.clone(),
        template_engine.clone(),
        metrics.clone(),
        redis.clone(),
        sqlite.clone(),
    );

    eprintln!("blackhole-pool init: starting stratum/api servers");
    let stratum_handle = tokio::spawn(async move { stratum.run().await });

    // API server is optional (disable for max-perf endpoints).
    let sqlite_for_api = sqlite.clone();
    let rpc_for_api = rpc.clone();
    let api_handle = if config.api_enabled {
        let api = ApiServer::new(
            config.clone(),
            metrics.clone(),
            sqlite_for_api,
            rpc_for_api,
            template_engine.clone(),
        );
        Some(tokio::spawn(async move { api.run().await }))
    } else {
        None
    };

    if config.persist_best && sqlite.is_enabled() {
        let metrics_s = metrics.clone();
        let sqlite_s = sqlite.clone();
        let persist_every = std::time::Duration::from_secs(config.best_persist_interval_secs.max(1));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(persist_every);
            loop {
                ticker.tick().await;
                let snapshot = metrics_s.snapshot().await;
                if let Err(err) = sqlite_s.persist_best_snapshot(&snapshot).await {
                    tracing::warn!("best summary persistence failed: {err:?}");
                }
                let current_template_age_secs = template_engine
                    .template_age_secs();
                let current_window = metrics_s
                    .current_block_window_snapshot(current_template_age_secs)
                    .await;
                if let Err(err) = sqlite_s.upsert_block_window(current_window.into()).await {
                    tracing::warn!("current block window persistence failed: {err:?}");
                }
                let finalized_windows = metrics_s.finalized_block_windows_snapshot().await;
                for window in finalized_windows {
                    if let Err(err) = sqlite_s.upsert_block_window(window.into()).await {
                        tracing::warn!("finalized block window persistence failed: {err:?}");
                    }
                }
            }
        });
    }

    if let Some(api_handle) = api_handle {
        tokio::select! {
            res = stratum_handle => {
                match res {
                    Ok(Ok(())) => info!("stratum stopped"),
                    Ok(Err(err)) => {
                        error!("stratum exited with error: {err:?}");
                        return Err(err).context("stratum exited");
                    }
                    Err(err) => {
                        error!("stratum task failed: {err:?}");
                        return Err(err).context("stratum task join failed");
                    }
                }
            }
            res = api_handle => {
                match res {
                    Ok(Ok(())) => info!("api stopped"),
                    Ok(Err(err)) => {
                        error!("api exited with error: {err:?}");
                        return Err(err).context("api exited");
                    }
                    Err(err) => {
                        error!("api task failed: {err:?}");
                        return Err(err).context("api task join failed");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown signal received");
            }
        }
    } else {
        // API disabled: wait on stratum only (or ctrl-c)
        tokio::select! {
            res = stratum_handle => {
                match res {
                    Ok(Ok(())) => info!("stratum stopped"),
                    Ok(Err(err)) => {
                        error!("stratum exited with error: {err:?}");
                        return Err(err).context("stratum exited");
                    }
                    Err(err) => {
                        error!("stratum task failed: {err:?}");
                        return Err(err).context("stratum task join failed");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown signal received");
            }
        }
    }

    eprintln!("blackhole-pool init: entering steady state");
Ok(())
}
