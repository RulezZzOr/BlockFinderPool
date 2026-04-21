use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use tracing::warn;
use uuid::Uuid;

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

        self.ensure_blocks_found_by_column().await?;

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
                updated_at   TEXT NOT NULL
            );
            "#,
        )
        .execute(pool.as_ref())
        .await?;

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

    /// Update (or insert) the all-time best difficulty for a worker.
    /// Only called when a new best is reached, so it is very infrequent.
    pub async fn upsert_worker_best(&self, worker: &str, best_diff: f64) -> anyhow::Result<()> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };
        sqlx::query(
            r#"
            INSERT INTO worker_best (worker, best_diff, updated_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(worker) DO UPDATE SET
                best_diff  = excluded.best_diff,
                updated_at = excluded.updated_at
            WHERE excluded.best_diff > worker_best.best_diff
            "#,
        )
        .bind(worker)
        .bind(best_diff)
        .bind(Utc::now().to_rfc3339())
        .execute(pool.as_ref())
        .await?;
        Ok(())
    }

    /// Load all persisted best difficulties at pool startup.
    pub async fn load_worker_bests(&self) -> anyhow::Result<HashMap<String, f64>> {
        let Some(pool) = &self.pool else {
            return Ok(HashMap::new());
        };
        let rows = sqlx::query("SELECT worker, best_diff FROM worker_best")
            .fetch_all(pool.as_ref())
            .await?;
        let mut out = HashMap::with_capacity(rows.len());
        for row in rows {
            let worker: String = row.get("worker");
            let diff: f64 = row.get("best_diff");
            out.insert(worker, diff);
        }
        Ok(out)
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
