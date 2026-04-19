use std::env;

use anyhow::{bail, Context};
use bitcoin::Network;
fn opt_trimmed(var: &str) -> Option<String> {
    std::env::var(var).ok().and_then(|v| {
        let t = v.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    })
}

fn parse_bool(var: &str, default: bool) -> bool {
    match std::env::var(var).ok().map(|v| v.trim().to_ascii_lowercase()) {
        None => default,
        Some(v) if v.is_empty() => default,
        Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "y" | "on") => true,
        Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "n" | "off") => false,
        Some(_) => default,
    }
}


#[derive(Clone, Debug)]
pub struct Config {
    pub network: Network,
    pub stratum_bind: String,
    pub stratum_port: u16,
    pub api_bind: String,
    pub api_port: u16,
    pub api_enabled: bool,
    pub rpc_url: String,
    pub rpc_user: String,
    pub rpc_pass: String,
    /// One or more ZMQ endpoints for block notifications.
    /// Prefer setting `ZMQ_BLOCKS` (comma-separated). `ZMQ_BLOCK` is still supported.
    pub zmq_block_urls: Vec<String>,
    /// One or more ZMQ endpoints for tx notifications.
    /// Prefer setting `ZMQ_TXS` (comma-separated). `ZMQ_TX` is still supported.
    pub zmq_tx_urls: Vec<String>,
    pub payout_address: String,
    pub payout_script_hex: Option<String>,
    pub pool_tag: String,
    pub coinbase_message: String,
    pub extranonce1_size: usize,
    pub extranonce2_size: usize,
    pub min_difficulty: f64,
    pub max_difficulty: f64,
    pub start_difficulty: f64,
    pub target_share_time_secs: f64,
    pub vardiff_retarget_time_secs: f64,
    pub vardiff_enabled: bool,
    pub job_refresh_ms: u64,
    pub template_poll_ms: u64,
    // notify_throttle_ms removed — replaced by token bucket (NOTIFY_BUCKET_CAPACITY /
    // NOTIFY_BUCKET_REFILL_MS). clean_jobs=true always bypasses the bucket entirely.

    /// Maximum number of tokens the per-session notify bucket can hold (burst capacity).
    /// Default 2: miner can receive 2 back-to-back mempool-update notifies at connect time.
    pub notify_bucket_capacity: f64,
    /// Milliseconds between token refills in the per-session notify bucket.
    /// Default 500: at most 1 new mempool-update notify per 500ms per miner.
    /// clean_jobs=true (new block) always bypasses the bucket entirely.
    pub notify_bucket_refill_ms: f64,
    /// Minimum ms between ZMQ-triggered template refreshes (debounce duplicate topics).
    pub zmq_debounce_ms: u64,
    /// After a new block (hashblock ZMQ), suppress TX-triggered GBT refreshes for this many ms.
    /// Prevents hammering bitcoind during the mempool refill burst after each block.
    /// After the window expires, ONE refresh captures all accumulated high-fee txs.
    pub post_block_suppress_ms: u64,
    pub auth_token: Option<String>,
    /// If true, the pool refuses to start when AUTH_TOKEN is not set.
    /// Useful when the stratum port is bound to 0.0.0.0 and the operator
    /// wants to guarantee that no unauthenticated miner can connect.
    /// Default: false (allows empty token for local-only setups).
    pub require_auth_token: bool,
    pub redis_url: Option<String>,
    pub database_url: Option<String>,

    // Performance / persistence toggles (solo max-perf defaults).
    pub persist_shares: bool,
    pub persist_blocks: bool,

    /// Number of accepted shares per session for which SHARE_PROOF is logged.
    /// Default 0 = disabled.  Set SHARE_PROOF_SHARES=200 to enable for debugging.
    /// When 0, the logging block is skipped entirely with no lock acquisition.
    pub share_proof_limit: u16,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let network = match env::var("BITCOIN_NETWORK")
            .unwrap_or_else(|_| "mainnet".to_string())
            .as_str()
        {
            "mainnet" => Network::Bitcoin,
            "testnet" => Network::Testnet,
            "signet" => Network::Signet,
            "regtest" => Network::Regtest,
            other => bail!("unsupported BITCOIN_NETWORK: {other}"),
        };

        let stratum_bind = env::var("STRATUM_BIND").unwrap_or_else(|_| "0.0.0.0".to_string());
        let stratum_port = env::var("STRATUM_PORT")
            .unwrap_or_else(|_| "3333".to_string())
            .parse()
            .context("STRATUM_PORT must be a number")?;

        let api_bind = env::var("API_BIND").unwrap_or_else(|_| "0.0.0.0".to_string());
        let api_port = env::var("API_PORT")
            .unwrap_or_else(|_| "8080".to_string())
            .parse()
            .context("API_PORT must be a number")?;

        let api_enabled = parse_bool("API_ENABLED", true);

        let rpc_url = env::var("RPC_URL").context("RPC_URL is required")?;
        let rpc_user = env::var("RPC_USER").context("RPC_USER is required")?;
        let rpc_pass = env::var("RPC_PASS").context("RPC_PASS is required")?;

        let zmq_block_urls = opt_trimmed("ZMQ_BLOCKS")
            .or_else(|| opt_trimmed("ZMQ_BLOCK"))
            .map(|s| {
                s.split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let zmq_tx_urls = opt_trimmed("ZMQ_TXS")
            .or_else(|| opt_trimmed("ZMQ_TX"))
            .map(|s| {
                s.split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Payout settings:
        // - If PAYOUT_SCRIPT_HEX is provided it overrides PAYOUT_ADDRESS entirely.
        // - PAYOUT_ADDRESS is the FALLBACK used only when a miner's Stratum username
        //   is NOT a valid Bitcoin address.
        // - It is OPTIONAL: if empty, the pool requires every miner to set their
        //   Bitcoin address as the Stratum username.  Any miner connecting without
        //   a valid address will be rejected with a clear error.
        let payout_script_hex = opt_trimmed("PAYOUT_SCRIPT_HEX");
        let payout_address = env::var("PAYOUT_ADDRESS").unwrap_or_default();

        let pool_tag = env::var("POOL_TAG").unwrap_or_else(|_| "BlackHole".to_string());
        let coinbase_message = env::var("COINBASE_MESSAGE").unwrap_or_else(|_| "BlackHole".to_string());

        let extranonce1_size = env::var("EXTRANONCE1_SIZE")
            .unwrap_or_else(|_| "4".to_string())
            .parse()
            .context("EXTRANONCE1_SIZE must be a number")?;
        let extranonce2_size = env::var("EXTRANONCE2_SIZE")
            .unwrap_or_else(|_| "4".to_string())
            .parse()
            .context("EXTRANONCE2_SIZE must be a number")?;

        let min_difficulty: f64 = env::var("MIN_DIFFICULTY")
            .unwrap_or_else(|_| "16384".to_string())
            .parse()
            .context("MIN_DIFFICULTY must be a number")?;
        let max_difficulty: f64 = env::var("MAX_DIFFICULTY")
            .unwrap_or_else(|_| "4194304".to_string())
            .parse()
            .context("MAX_DIFFICULTY must be a number")?;
        let start_difficulty: f64 = env::var("STRATUM_START_DIFFICULTY")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(min_difficulty)
            .clamp(min_difficulty, max_difficulty);
        let target_share_time_secs = env::var("TARGET_SHARE_TIME_SECS")
            .unwrap_or_else(|_| "60".to_string())
            .parse()
            .context("TARGET_SHARE_TIME_SECS must be a number")?;
        let vardiff_retarget_time_secs = env::var("VARDIFF_RETARGET_SECS")
            .unwrap_or_else(|_| "90".to_string())
            .parse()
            .context("VARDIFF_RETARGET_SECS must be a number")?;
                let vardiff_enabled = parse_bool("VARDIFF_ENABLED", true);

        let job_refresh_ms = env::var("JOB_REFRESH_MS")
            .unwrap_or_else(|_| "60000".to_string())
            .parse()
            .context("JOB_REFRESH_MS must be a number")?;
        let template_poll_ms = env::var("TEMPLATE_POLL_MS")
            .unwrap_or_else(|_| "10000".to_string())
            .parse()
            .context("TEMPLATE_POLL_MS must be a number")?;
        let notify_bucket_capacity: f64 = env::var("NOTIFY_BUCKET_CAPACITY")
            .unwrap_or_else(|_| "2".to_string())
            .parse()
            .context("NOTIFY_BUCKET_CAPACITY must be a number")?;
        let notify_bucket_refill_ms: f64 = env::var("NOTIFY_BUCKET_REFILL_MS")
            .unwrap_or_else(|_| "500".to_string())
            .parse()
            .context("NOTIFY_BUCKET_REFILL_MS must be a number")?;
        let zmq_debounce_ms = env::var("ZMQ_DEBOUNCE_MS")
            .unwrap_or_else(|_| "250".to_string())
            .parse()
            .context("ZMQ_DEBOUNCE_MS must be a number")?;
        let post_block_suppress_ms = env::var("POST_BLOCK_SUPPRESS_MS")
            .unwrap_or_else(|_| "15000".to_string())
            .parse()
            .context("POST_BLOCK_SUPPRESS_MS must be a number")?;

        let auth_token = env::var("AUTH_TOKEN")
            .ok()
            .and_then(|token| {
                let trimmed = token.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            });
        let require_auth_token = parse_bool("REQUIRE_AUTH_TOKEN", false);
                let redis_url = opt_trimmed("REDIS_URL");
        let database_url = opt_trimmed("DATABASE_URL");

        // In solo mode, persisting every share can become the bottleneck at high hashrate.
        // Default to false unless explicitly enabled.
                let persist_shares = parse_bool("PERSIST_SHARES", false);
        // Persisting blocks is cheap and usually desirable.
                let persist_blocks = parse_bool("PERSIST_BLOCKS", true);

        let share_proof_limit: u16 = env::var("SHARE_PROOF_SHARES")
            .unwrap_or_else(|_| "0".to_string())
            .parse()
            .context("SHARE_PROOF_SHARES must be a number 0-65535")?;

        Ok(Self {
            network,
            stratum_bind,
            stratum_port,
            api_bind,
            api_port,
            api_enabled,
            rpc_url,
            rpc_user,
            rpc_pass,
            zmq_block_urls,
            zmq_tx_urls,
            payout_address,
            payout_script_hex,
            pool_tag,
            coinbase_message,
            extranonce1_size,
            extranonce2_size,
            min_difficulty,
            max_difficulty,
            start_difficulty,
            target_share_time_secs,
            vardiff_retarget_time_secs,
            vardiff_enabled,
            job_refresh_ms,
            template_poll_ms,
            notify_bucket_capacity,
            notify_bucket_refill_ms,
            zmq_debounce_ms,
            post_block_suppress_ms,
            auth_token,
            require_auth_token,
            redis_url,
            database_url,

            persist_shares,
            persist_blocks,
            share_proof_limit,
        })
    }
}
