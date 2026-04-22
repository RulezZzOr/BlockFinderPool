use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use serde::Serialize;
use tracing::warn;
use uuid::Uuid;

use crate::metrics::MetricsSnapshot;

#[derive(Clone)]
pub struct SqliteStore {
    pool: Option<Arc<SqlitePool>>,
}

#[derive(Debug, Clone)]
pub struct ShareRecord {
    pub id: Uuid,
    pub worker: String,
    pub difficulty: f64,
    pub is_block: bool,
    pub is_accepted: bool,
    pub latency_ms: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct BlockCandidateRecord {
    pub id: Uuid,
    pub worker: String,
    pub payout_address: Option<String>,
    pub session_id: Option<String>,
    pub job_id: String,
    pub height: i64,
    pub prevhash: String,
    pub ntime: String,
    pub nonce: String,
    pub version: String,
    pub version_mask: String,
    pub extranonce1: Option<String>,
    pub extranonce2: String,
    pub merkle_root: String,
    pub coinbase_hex: Option<String>,
    pub block_header_hex: String,
    pub block_hex: Option<String>,
    pub full_block_hex: Option<String>,
    pub block_hash: String,
    pub submitted_difficulty: f64,
    pub network_difficulty: f64,
    pub current_share_difficulty: f64,
    pub submitblock_requested_at: DateTime<Utc>,
    pub submitblock_result: String,
    pub submitblock_latency_ms: i64,
    pub rpc_error: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockCandidateRow {
    pub timestamp: String,
    pub worker: String,
    pub payout_address: Option<String>,
    pub session_id: Option<String>,
    pub job_id: String,
    pub height: i64,
    pub prevhash: String,
    pub ntime: String,
    pub nonce: String,
    pub version: String,
    pub version_mask: String,
    pub extranonce1: Option<String>,
    pub extranonce2: String,
    pub merkle_root: String,
    pub coinbase_hex: Option<String>,
    pub block_header_hex: String,
    pub block_hex: Option<String>,
    pub full_block_hex: Option<String>,
    pub block_hash: String,
    pub submitted_difficulty: f64,
    pub network_difficulty: f64,
    pub current_share_difficulty: f64,
    pub submitblock_requested_at: String,
    pub submitblock_result: String,
    pub submitblock_latency_ms: i64,
    pub rpc_error: Option<String>,
}

impl SqliteStore {
    pub async fn connect(database_url: Option<&str>) -> anyhow::Result<Self> {
        if let Some(url) = database_url {
            if let Some(path) = sqlite_path(url) {
                if let Some(parent) = path.parent() {
                    if let Err(err) = std::fs::create_dir_all(parent) {
                        warn!("failed to create sqlite dir {parent:?}: {err}");
                    }
                }
            }

            match SqlitePool::connect(url).await {
                Ok(pool) => {
                    let store = Self {
                        pool: Some(Arc::new(pool)),
                    };
                    if let Err(err) = store.init().await {
                        warn!("sqlite init failed, disabling db: {err:?}");
                        Ok(Self { pool: None })
                    } else {
                        Ok(store)
                    }
                }
                Err(err) => {
                    warn!("sqlite connect failed, disabling db: {err:?}");
                    Ok(Self { pool: None })
                }
            }
        } else {
            Ok(Self { pool: None })
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.pool.is_some()
    }

    pub fn pool(&self) -> Option<Arc<SqlitePool>> {
        self.pool.clone()
    }

    async fn init(&self) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS shares (
                id TEXT PRIMARY KEY,
                worker TEXT NOT NULL,
                difficulty REAL NOT NULL,
                is_block INTEGER NOT NULL DEFAULT 0,
                is_accepted INTEGER NOT NULL DEFAULT 1,
                latency_ms INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS blocks (
                id TEXT PRIMARY KEY,
                height INTEGER NOT NULL,
                block_hash TEXT NOT NULL,
                found_by TEXT,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS block_candidates (
                id TEXT PRIMARY KEY,
                worker TEXT NOT NULL,
                payout_address TEXT,
                session_id TEXT,
                job_id TEXT NOT NULL,
                height INTEGER NOT NULL,
                prevhash TEXT NOT NULL,
                ntime TEXT NOT NULL,
                nonce TEXT NOT NULL,
                version TEXT NOT NULL,
                version_mask TEXT NOT NULL,
                extranonce1 TEXT,
                extranonce2 TEXT NOT NULL,
                merkle_root TEXT NOT NULL,
                coinbase_hex TEXT,
                block_header_hex TEXT NOT NULL,
                block_hex TEXT,
                full_block_hex TEXT,
                block_hash TEXT NOT NULL,
                submitted_difficulty REAL NOT NULL,
                network_difficulty REAL NOT NULL,
                current_share_difficulty REAL NOT NULL,
                submitblock_requested_at TEXT NOT NULL,
                submitblock_result TEXT NOT NULL,
                submitblock_latency_ms INTEGER NOT NULL,
                rpc_error TEXT,
                created_at TEXT NOT NULL
            );
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        self.ensure_blocks_found_by_column().await?;
        self.ensure_block_candidate_columns().await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_shares_worker_created
            ON shares(worker, created_at DESC);
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_blocks_created
            ON blocks(created_at DESC);
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        // Persist best_difficulty per worker across restarts.
        // This is the all-time best hash quality score — resetting it on every
        // pool restart would destroy days of accumulated lucky-hash history.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS worker_best (
                worker       TEXT PRIMARY KEY,
                best_diff    REAL NOT NULL DEFAULT 0,
                best_submitted_diff REAL NOT NULL DEFAULT 0,
                best_accepted_diff REAL NOT NULL DEFAULT 0,
                best_block_candidate_diff REAL NOT NULL DEFAULT 0,
                updated_at   TEXT NOT NULL
            );
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS best_summaries (
                scope_kind   TEXT NOT NULL,
                scope_key    TEXT PRIMARY KEY,
                worker       TEXT,
                session_id   TEXT,
                height       INTEGER,
                prevhash     TEXT,
                template_key TEXT,
                job_id       TEXT,
                created_at   TEXT,
                best_submitted_diff REAL NOT NULL DEFAULT 0,
                best_accepted_diff  REAL NOT NULL DEFAULT 0,
                best_block_candidate_diff REAL NOT NULL DEFAULT 0,
                updated_at   TEXT NOT NULL
            );
            "#,
        )
        .execute(pool.as_ref())
        .await?;

        self.ensure_worker_best_columns().await?;
        self.ensure_best_summaries_columns().await?;

        Ok(())
    }

    async fn ensure_worker_best_columns(&self) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };

        let rows = sqlx::query("PRAGMA table_info(worker_best)")
            .fetch_all(pool.as_ref())
            .await?;
        let has_best_submitted = rows.iter().any(|row| {
            let name: String = row.get("name");
            name == "best_submitted_diff"
        });
        let has_best_accepted = rows.iter().any(|row| {
            let name: String = row.get("name");
            name == "best_accepted_diff"
        });
        let has_best_candidate = rows.iter().any(|row| {
            let name: String = row.get("name");
            name == "best_block_candidate_diff"
        });

        if !has_best_submitted {
            sqlx::query("ALTER TABLE worker_best ADD COLUMN best_submitted_diff REAL NOT NULL DEFAULT 0")
                .execute(pool.as_ref())
                .await?;
        }
        if !has_best_accepted {
            sqlx::query("ALTER TABLE worker_best ADD COLUMN best_accepted_diff REAL NOT NULL DEFAULT 0")
                .execute(pool.as_ref())
                .await?;
        }
        if !has_best_candidate {
            sqlx::query("ALTER TABLE worker_best ADD COLUMN best_block_candidate_diff REAL NOT NULL DEFAULT 0")
                .execute(pool.as_ref())
                .await?;
        }

        Ok(())
    }

    async fn ensure_blocks_found_by_column(&self) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };

        let rows = sqlx::query("PRAGMA table_info(blocks)")
            .fetch_all(pool.as_ref())
            .await?;
        let has_found_by = rows.iter().any(|row| {
            let name: String = row.get("name");
            name == "found_by"
        });

        if !has_found_by {
            sqlx::query("ALTER TABLE blocks ADD COLUMN found_by TEXT")
                .execute(pool.as_ref())
                .await?;
        }

        Ok(())
    }

    async fn ensure_best_summaries_columns(&self) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };

        let rows = sqlx::query("PRAGMA table_info(best_summaries)")
            .fetch_all(pool.as_ref())
            .await?;

        let has_best_submitted = rows.iter().any(|row| {
            row.try_get::<String, _>("name")
                .map(|name| name == "best_submitted_diff")
                .unwrap_or(false)
        });
        if !has_best_submitted {
            sqlx::query(
                "ALTER TABLE best_summaries ADD COLUMN best_submitted_diff REAL NOT NULL DEFAULT 0",
            )
            .execute(pool.as_ref())
            .await?;
        }

        let has_best_accepted = rows.iter().any(|row| {
            row.try_get::<String, _>("name")
                .map(|name| name == "best_accepted_diff")
                .unwrap_or(false)
        });
        if !has_best_accepted {
            sqlx::query(
                "ALTER TABLE best_summaries ADD COLUMN best_accepted_diff REAL NOT NULL DEFAULT 0",
            )
            .execute(pool.as_ref())
            .await?;
        }

        let has_best_block_candidate = rows.iter().any(|row| {
            row.try_get::<String, _>("name")
                .map(|name| name == "best_block_candidate_diff")
                .unwrap_or(false)
        });
        if !has_best_block_candidate {
            sqlx::query(
                "ALTER TABLE best_summaries ADD COLUMN best_block_candidate_diff REAL NOT NULL DEFAULT 0",
            )
            .execute(pool.as_ref())
            .await?;
        }

        Ok(())
    }

    async fn ensure_block_candidate_columns(&self) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };

        let rows = sqlx::query("PRAGMA table_info(block_candidates)")
            .fetch_all(pool.as_ref())
            .await?;
        let has_full_block_hex = rows.iter().any(|row| {
            let name: String = row.get("name");
            name == "full_block_hex"
        });

        if !has_full_block_hex {
            sqlx::query("ALTER TABLE block_candidates ADD COLUMN full_block_hex TEXT")
                .execute(pool.as_ref())
                .await?;
        }

        Ok(())
    }

    /// Update (or insert) the all-time best difficulty for a worker.
    /// Only called when a new best is reached, so it is very infrequent.
    pub async fn upsert_worker_best(
        &self,
        worker: &str,
        best_submitted: f64,
        best_accepted: f64,
        best_block_candidate: f64,
    ) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };
        sqlx::query(
            r#"
            INSERT INTO worker_best (
                worker, best_diff, best_submitted_diff, best_accepted_diff,
                best_block_candidate_diff, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(worker) DO UPDATE SET
                best_diff  = MAX(worker_best.best_diff, excluded.best_submitted_diff),
                best_submitted_diff = MAX(worker_best.best_submitted_diff, excluded.best_submitted_diff),
                best_accepted_diff = MAX(worker_best.best_accepted_diff, excluded.best_accepted_diff),
                best_block_candidate_diff = MAX(worker_best.best_block_candidate_diff, excluded.best_block_candidate_diff),
                updated_at = excluded.updated_at
            "#,
        )
        .bind(worker)
        .bind(best_submitted)
        .bind(best_accepted)
        .bind(best_block_candidate)
        .bind(Utc::now().to_rfc3339())
        .execute(pool.as_ref())
        .await?;
        Ok(())
    }

    /// Load all persisted best difficulties at pool startup.
    pub async fn load_worker_bests(&self) -> anyhow::Result<HashMap<String, (f64, f64, f64)>> {
        let Some(pool) = &self.pool else {
            return Ok(HashMap::new());
        };
        let rows = sqlx::query(
            r#"
            SELECT worker,
                   COALESCE(best_submitted_diff, best_diff, 0) AS best_submitted_diff,
                   COALESCE(best_accepted_diff, best_diff, 0) AS best_accepted_diff,
                   COALESCE(best_block_candidate_diff, 0) AS best_block_candidate_diff
            FROM worker_best
            "#,
        )
            .fetch_all(pool.as_ref())
            .await?;
        let mut out = HashMap::with_capacity(rows.len());
        for row in rows {
            let worker: String = row.get("worker");
            let best_submitted: f64 = row.get("best_submitted_diff");
            let best_accepted: f64 = row.get("best_accepted_diff");
            let best_block_candidate: f64 = row.get("best_block_candidate_diff");
            out.insert(worker, (best_submitted, best_accepted, best_block_candidate));
        }
        Ok(out)
    }

    pub async fn persist_best_snapshot(
        &self,
        snapshot: &MetricsSnapshot,
    ) -> anyhow::Result<()> {
        let Some(_pool) = &self.pool else {
            return Ok(());
        };

        let global_created_at = snapshot.current_scope.created_at.to_rfc3339();
        let current_created_at = snapshot.current_scope.created_at.to_rfc3339();
        let previous_created_at = snapshot.previous_scope.as_ref().map(|s| s.created_at.to_rfc3339());

        for miner in &snapshot.miners {
            let miner_last_seen = miner.last_seen.to_rfc3339();
            let miner_session_created_at = miner
                .session_start
                .as_ref()
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| miner_last_seen.clone());
            self.upsert_worker_best(
                &miner.worker,
                miner.best_submitted_difficulty,
                miner.best_accepted_difficulty,
                miner.best_block_candidate_difficulty,
            ).await?;

            if let Some(session_id) = miner.session_id.as_deref() {
                let scope_key = format!("session:{worker}:{session}", worker = miner.worker.as_str(), session = session_id);
                self.upsert_best_summary(
                    "session",
                    &scope_key,
                    Some(&miner.worker),
                    Some(session_id),
                    None,
                    None,
                    None,
                    None,
                    Some(miner_session_created_at.as_str()),
                    miner.session_best_submitted_difficulty,
                    miner.session_best_accepted_difficulty,
                    miner.best_block_candidate_difficulty,
                ).await?;
            }
        }

        let global_key = "global";
        self.upsert_best_summary(
            "global",
            global_key,
            None,
            None,
            Some(snapshot.current_scope.height as i64),
            Some(snapshot.current_scope.prevhash.as_str()),
            Some(snapshot.current_scope.template_key.as_str()),
            Some(snapshot.current_scope.job_id.as_str()),
            Some(global_created_at.as_str()),
            snapshot.global_best_submitted_difficulty,
            snapshot.global_best_accepted_difficulty,
            snapshot.global_best_block_candidate_difficulty,
        ).await?;

        let current_key = format!(
            "{}:{}:{}:{}",
            snapshot.current_scope.height,
            snapshot.current_scope.prevhash,
            snapshot.current_scope.template_key,
            snapshot.current_scope.job_id,
        );
        self.upsert_best_summary(
            "current_block",
            &current_key,
            None,
            None,
            Some(snapshot.current_scope.height as i64),
            Some(snapshot.current_scope.prevhash.as_str()),
            Some(snapshot.current_scope.template_key.as_str()),
            Some(snapshot.current_scope.job_id.as_str()),
            Some(current_created_at.as_str()),
            snapshot.current_block_best_submitted_difficulty,
            snapshot.current_block_best_accepted_difficulty,
            snapshot.current_block_best_candidate_difficulty,
        ).await?;

        if let Some(previous) = &snapshot.previous_scope {
            let previous_key = format!(
                "{}:{}:{}:{}",
                previous.height,
                previous.prevhash,
                previous.template_key,
                previous.job_id,
            );
            self.upsert_best_summary(
                "previous_block",
                &previous_key,
                None,
                None,
                Some(previous.height as i64),
                Some(previous.prevhash.as_str()),
                Some(previous.template_key.as_str()),
                Some(previous.job_id.as_str()),
                Some(previous_created_at.as_deref().unwrap_or("")),
                snapshot.previous_block_best_submitted_difficulty,
                snapshot.previous_block_best_accepted_difficulty,
                snapshot.previous_block_best_candidate_difficulty,
            ).await?;
        }

        Ok(())
    }

    pub async fn upsert_best_summary(
        &self,
        scope_kind: &str,
        scope_key: &str,
        worker: Option<&str>,
        session_id: Option<&str>,
        height: Option<i64>,
        prevhash: Option<&str>,
        template_key: Option<&str>,
        job_id: Option<&str>,
        created_at: Option<&str>,
        best_submitted: f64,
        best_accepted: f64,
        best_block_candidate: f64,
    ) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };

        sqlx::query(
            r#"
            INSERT INTO best_summaries (
                scope_kind, scope_key, worker, session_id, height, prevhash,
                template_key, job_id, created_at,
                best_submitted_diff, best_accepted_diff, best_block_candidate_diff,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(scope_key) DO UPDATE SET
                scope_kind = excluded.scope_kind,
                worker = excluded.worker,
                session_id = excluded.session_id,
                height = excluded.height,
                prevhash = excluded.prevhash,
                template_key = excluded.template_key,
                job_id = excluded.job_id,
                created_at = excluded.created_at,
                best_submitted_diff = MAX(best_summaries.best_submitted_diff, excluded.best_submitted_diff),
                best_accepted_diff = MAX(best_summaries.best_accepted_diff, excluded.best_accepted_diff),
                best_block_candidate_diff = MAX(best_summaries.best_block_candidate_diff, excluded.best_block_candidate_diff),
                updated_at = excluded.updated_at
            "#,
        )
        .bind(scope_kind)
        .bind(scope_key)
        .bind(worker)
        .bind(session_id)
        .bind(height)
        .bind(prevhash)
        .bind(template_key)
        .bind(job_id)
        .bind(created_at)
        .bind(best_submitted)
        .bind(best_accepted)
        .bind(best_block_candidate)
        .bind(Utc::now().to_rfc3339())
        .execute(pool.as_ref())
        .await?;

        Ok(())
    }

    pub async fn insert_share(&self, record: ShareRecord) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };

        sqlx::query(
            r#"
            INSERT INTO shares
                (id, worker, difficulty, is_block, is_accepted, latency_ms, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
        )
        .bind(record.id.to_string())
        .bind(record.worker)
        .bind(record.difficulty)
        .bind(record.is_block)
        .bind(record.is_accepted)
        .bind(record.latency_ms)
        .bind(record.created_at)
        .execute(pool.as_ref())
        .await?;

        Ok(())
    }

    pub async fn insert_block(
        &self,
        height: i64,
        block_hash: &str,
        found_by: Option<&str>,
        status: &str,
    ) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };

        sqlx::query(
            r#"
            INSERT INTO blocks (id, height, block_hash, found_by, status, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(height)
        .bind(block_hash)
        .bind(found_by)
        .bind(status)
        .bind(Utc::now().to_rfc3339())
        .execute(pool.as_ref())
        .await?;

        Ok(())
    }

    pub async fn insert_block_candidate(
        &self,
        record: BlockCandidateRecord,
    ) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };

        sqlx::query(
            r#"
            INSERT INTO block_candidates (
                id, worker, payout_address, session_id, job_id, height, prevhash,
                ntime, nonce, version, version_mask, extranonce1, extranonce2,
                merkle_root, coinbase_hex, block_header_hex, block_hex, block_hash,
                full_block_hex,
                submitted_difficulty, network_difficulty, current_share_difficulty,
                submitblock_requested_at, submitblock_result, submitblock_latency_ms,
                rpc_error, created_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)
            "#,
        )
        .bind(record.id.to_string())
        .bind(record.worker)
        .bind(record.payout_address)
        .bind(record.session_id)
        .bind(record.job_id)
        .bind(record.height)
        .bind(record.prevhash)
        .bind(record.ntime)
        .bind(record.nonce)
        .bind(record.version)
        .bind(record.version_mask)
        .bind(record.extranonce1)
        .bind(record.extranonce2)
        .bind(record.merkle_root)
        .bind(record.coinbase_hex)
        .bind(record.block_header_hex)
        .bind(record.block_hex)
        .bind(record.block_hash)
        .bind(record.full_block_hex)
        .bind(record.submitted_difficulty)
        .bind(record.network_difficulty)
        .bind(record.current_share_difficulty)
        .bind(record.submitblock_requested_at)
        .bind(record.submitblock_result)
        .bind(record.submitblock_latency_ms)
        .bind(record.rpc_error)
        .bind(record.created_at)
        .execute(pool.as_ref())
        .await?;

        Ok(())
    }

    pub async fn update_block_candidate_result(
        &self,
        id: Uuid,
        submitblock_result: &str,
        submitblock_latency_ms: i64,
        rpc_error: Option<&str>,
    ) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };

        sqlx::query(
            r#"
            UPDATE block_candidates
               SET submitblock_result = ?2,
                   submitblock_latency_ms = ?3,
                   rpc_error = ?4
             WHERE id = ?1
            "#,
        )
        .bind(id.to_string())
        .bind(submitblock_result)
        .bind(submitblock_latency_ms)
        .bind(rpc_error)
        .execute(pool.as_ref())
        .await?;

        Ok(())
    }

    pub async fn fetch_block_candidates(&self, limit: i64) -> anyhow::Result<Vec<BlockCandidateRow>> {
        let Some(pool) = &self.pool else {
            return Ok(vec![]);
        };

        let rows = sqlx::query(
            r#"
            SELECT
                created_at, worker, payout_address, session_id, job_id, height, prevhash,
                ntime, nonce, version, version_mask, extranonce1, extranonce2,
                merkle_root, coinbase_hex, block_header_hex, block_hex, full_block_hex,
                block_hash, submitted_difficulty, network_difficulty, current_share_difficulty,
                submitblock_requested_at, submitblock_result, submitblock_latency_ms, rpc_error
            FROM block_candidates
            ORDER BY created_at DESC
            LIMIT ?1
            "#,
        )
        .bind(limit)
        .fetch_all(pool.as_ref())
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(BlockCandidateRow {
                timestamp: row.get("created_at"),
                worker: row.get("worker"),
                payout_address: row.try_get("payout_address")?,
                session_id: row.try_get("session_id")?,
                job_id: row.get("job_id"),
                height: row.get("height"),
                prevhash: row.get("prevhash"),
                ntime: row.get("ntime"),
                nonce: row.get("nonce"),
                version: row.get("version"),
                version_mask: row.get("version_mask"),
                extranonce1: row.try_get("extranonce1")?,
                extranonce2: row.get("extranonce2"),
                merkle_root: row.get("merkle_root"),
                coinbase_hex: row.try_get("coinbase_hex")?,
                block_header_hex: row.get("block_header_hex"),
                block_hex: row.try_get("block_hex")?,
                full_block_hex: row.try_get("full_block_hex")?,
                block_hash: row.get("block_hash"),
                submitted_difficulty: row.get("submitted_difficulty"),
                network_difficulty: row.get("network_difficulty"),
                current_share_difficulty: row.get("current_share_difficulty"),
                submitblock_requested_at: row.get("submitblock_requested_at"),
                submitblock_result: row.get("submitblock_result"),
                submitblock_latency_ms: row.get("submitblock_latency_ms"),
                rpc_error: row.try_get("rpc_error")?,
            });
        }
        Ok(out)
    }

    pub async fn fetch_block_candidate(
        &self,
        id: Uuid,
    ) -> anyhow::Result<Option<BlockCandidateRow>> {
        let Some(pool) = &self.pool else {
            return Ok(None);
        };

        let row = sqlx::query(
            r#"
            SELECT
                created_at, worker, payout_address, session_id, job_id, height, prevhash,
                ntime, nonce, version, version_mask, extranonce1, extranonce2,
                merkle_root, coinbase_hex, block_header_hex, block_hex, full_block_hex,
                block_hash, submitted_difficulty, network_difficulty, current_share_difficulty,
                submitblock_requested_at, submitblock_result, submitblock_latency_ms, rpc_error
            FROM block_candidates
            WHERE id = ?1
            "#,
        )
        .bind(id.to_string())
        .fetch_optional(pool.as_ref())
        .await?;

        Ok(row.map(|row| BlockCandidateRow {
            timestamp: row.get("created_at"),
            worker: row.get("worker"),
            payout_address: row.try_get("payout_address").ok(),
            session_id: row.try_get("session_id").ok(),
            job_id: row.get("job_id"),
            height: row.get("height"),
            prevhash: row.get("prevhash"),
            ntime: row.get("ntime"),
            nonce: row.get("nonce"),
            version: row.get("version"),
            version_mask: row.get("version_mask"),
            extranonce1: row.try_get("extranonce1").ok(),
            extranonce2: row.get("extranonce2"),
            merkle_root: row.get("merkle_root"),
            coinbase_hex: row.try_get("coinbase_hex").ok(),
            block_header_hex: row.get("block_header_hex"),
            block_hex: row.try_get("block_hex").ok(),
            full_block_hex: row.try_get("full_block_hex").ok(),
            block_hash: row.get("block_hash"),
            submitted_difficulty: row.get("submitted_difficulty"),
            network_difficulty: row.get("network_difficulty"),
            current_share_difficulty: row.get("current_share_difficulty"),
            submitblock_requested_at: row.get("submitblock_requested_at"),
            submitblock_result: row.get("submitblock_result"),
            submitblock_latency_ms: row.get("submitblock_latency_ms"),
            rpc_error: row.try_get("rpc_error").ok(),
        }))
    }

    pub async fn fetch_blocks(&self, limit: i64) -> anyhow::Result<Vec<(i64, String, Option<String>, String, String)>> {
        let Some(pool) = &self.pool else {
            return Ok(vec![]);
        };

        let rows = sqlx::query(
            r#"
            SELECT height, block_hash, found_by, status, created_at
            FROM blocks
            ORDER BY created_at DESC
            LIMIT ?1
            "#,
        )
        .bind(limit)
        .fetch_all(pool.as_ref())
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let height: i64 = row.get("height");
            let hash: String = row.get("block_hash");
            let found_by: Option<String> = row.try_get("found_by")?;
            let status: String = row.get("status");
            let created_at: String = row.get("created_at");
            out.push((height, hash, found_by, status, created_at));
        }
        Ok(out)
    }
}

fn sqlite_path(url: &str) -> Option<PathBuf> {
    const PREFIX: &str = "sqlite://";
    if !url.starts_with(PREFIX) {
        return None;
    }
    let remainder = &url[PREFIX.len()..];
    if remainder.starts_with(":memory:") || remainder.is_empty() {
        return None;
    }
    let path = remainder.split('?').next().unwrap_or(remainder);
    Some(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn block_candidate_round_trip_persists_forensic_fields() {
        let store = SqliteStore::connect(Some("sqlite::memory:"))
            .await
            .expect("memory sqlite store");

        let requested_at = Utc::now();
        let record = BlockCandidateRecord {
            id: Uuid::new_v4(),
            worker: "miner-1".to_string(),
            payout_address: Some("bc1qtest".to_string()),
            session_id: Some("session-1".to_string()),
            job_id: "job-1".to_string(),
            height: 123,
            prevhash: "00ff".to_string(),
            ntime: "5f5e100".to_string(),
            nonce: "00000001".to_string(),
            version: "20000000".to_string(),
            version_mask: "1fffe000".to_string(),
            extranonce1: Some("abcd".to_string()),
            extranonce2: "ef01".to_string(),
            merkle_root: "deadbeef".to_string(),
            coinbase_hex: Some("0100".to_string()),
            block_header_hex: "0200".to_string(),
            block_hex: Some("0300".to_string()),
            full_block_hex: Some("0300".to_string()),
            block_hash: "hash".to_string(),
            submitted_difficulty: 42.0,
            network_difficulty: 100.0,
            current_share_difficulty: 21.0,
            submitblock_requested_at: requested_at,
            submitblock_result: "pending".to_string(),
            submitblock_latency_ms: 0,
            rpc_error: None,
            created_at: requested_at,
        };

        store.insert_block_candidate(record.clone()).await.expect("insert candidate");
        store
            .update_block_candidate_result(record.id, "submitted", 17, None)
            .await
            .expect("update candidate");

        let rows = store.fetch_block_candidates(10).await.expect("fetch candidates");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].worker, "miner-1");
        assert_eq!(rows[0].block_hash, "hash");
        assert_eq!(rows[0].submitblock_result, "submitted");
        assert_eq!(rows[0].submitblock_latency_ms, 17);
        assert_eq!(rows[0].full_block_hex.as_deref(), Some("0300"));

        let fetched = store
            .fetch_block_candidate(record.id)
            .await
            .expect("fetch candidate by id");
        assert_eq!(fetched.as_ref().map(|row| row.block_hash.as_str()), Some("hash"));
        assert_eq!(
            fetched.as_ref().and_then(|row| row.full_block_hex.as_deref()),
            Some("0300")
        );
    }
}
