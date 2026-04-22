import React, { useEffect, useState, useCallback, useRef } from "react";
import {
  PoolStats, Miner, NetworkInfo, BlockRow, PublicBlockRow, BlockCandidateRow, HashrateResponse,
  fetchPool, fetchMiners, fetchHashrate,
  fetchBlocks,
  fetchBlockCandidates,
  fetchPublicBlocks,
  fmtHr, fmtDiff, fmtNetHash, fmtUptime, fmtBlockInterval, timeAgo, shortWorker, shortAddress, getFirmwareLabel,
  blockSubsidy, fmtBtc,
} from "./api";
import BlocksTable from "./components/BlocksTable";
import BlockCandidatesTable from "./components/BlockCandidatesTable";
import PublicBlocksTable from "./components/PublicBlocksTable";
import "./styles.css";

const REFRESH_MS = 5000;
const PUBLIC_REFRESH_MS = 30000;

// ─── Helpers ──────────────────────────────────────────────────────────────────

function pct(v: number, of: number) {
  if (!of) return 0;
  return Math.min(100, (v / of) * 100);
}

function fmtMs(ms: number): string {
  if (ms >= 1000) return `${(ms / 1000).toFixed(1)}s`;
  return `${ms.toFixed(1)}ms`;
}

function fmtK(n: number): string {
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
  return String(n);
}

const HASHES_PER_DIFF = 4_294_967_296;

type HealthSeverity = "ok" | "warning" | "critical";

type HealthReason = {
  label: string;
  detail: string;
  severity: HealthSeverity;
};

type MiningHealthSummary = {
  severity: HealthSeverity;
  reasons: HealthReason[];
  expectedSharesPerMin: number;
  observedSharesPerMin: number;
  observedVsExpected: number;
  bestProbability: number | null;
  staleRatio: number;
  rejectRatio: number;
  duplicateRatio: number;
  runtimeSecs: number;
  templateAgeSecs: number | null;
  zmqLastBlockAgeSecs: number | null;
  rpcHealthy: boolean;
  zmqConnected: boolean;
};

function safePercent(numer: number, denom: number): number {
  if (!denom) return 0;
  return numer / denom;
}

function probabilityAtLeastOne(hashrateHs: number, difficulty: number, runtimeSecs: number): number {
  if (!Number.isFinite(hashrateHs) || !Number.isFinite(difficulty) || !Number.isFinite(runtimeSecs)) return 0;
  if (hashrateHs <= 0 || difficulty <= 0 || runtimeSecs <= 0) return 0;
  const lambda = (hashrateHs * runtimeSecs) / (difficulty * HASHES_PER_DIFF);
  if (lambda <= 0) return 0;
  return 1 - Math.exp(-lambda);
}

function deriveMiningHealth(
  pool: PoolStats | null,
  miners: Miner[],
  hashrate: HashrateResponse | null,
): MiningHealthSummary {
  const runtimeSecs = pool?.uptimeSecs ?? 0;
  const totalAccepted = miners.reduce((sum, m) => sum + (m.shares ?? 0), 0);
  const totalRejected = miners.reduce((sum, m) => sum + (m.rejected ?? 0), 0);
  const totalStale = miners.reduce((sum, m) => sum + (m.stale ?? 0), 0);
  const totalDuplicates = pool?.duplicateShares ?? 0;
  const totalSubmissions = totalAccepted + totalRejected + totalStale + totalDuplicates;

  const expectedSharesPerMin = miners.reduce((sum, miner) => {
    const hrHs = Math.max(0, miner.hashrate_gh ?? 0) * 1e9;
    const diff = miner.difficulty ?? 0;
    if (hrHs <= 0 || diff <= 0) return sum;
    return sum + (hrHs / (diff * HASHES_PER_DIFF)) * 60;
  }, 0);

  const windowStart = Date.now() - 10 * 60 * 1000;
  const observedSharesLast10Min = (hashrate?.recent ?? []).reduce((sum, point) => {
    const ts = Date.parse(point.timestamp);
    if (Number.isNaN(ts) || ts < windowStart) return sum;
    return sum + (point.shares ?? 0);
  }, 0);
  const observedSharesPerMin = observedSharesLast10Min / 10;
  const observedVsExpected = expectedSharesPerMin > 0 ? observedSharesPerMin / expectedSharesPerMin : 0;

  const staleRatio = safePercent(totalStale, totalSubmissions);
  const rejectRatio = safePercent(totalRejected, totalSubmissions);
  const duplicateRatio = safePercent(totalDuplicates, totalSubmissions);

  const bestForProbability = Math.max(
    pool?.currentBlockBestSubmittedDifficulty ?? 0,
    ...miners.map((m) => m.current_block_best_submitted_difficulty ?? 0),
  );
  const bestProbability = bestForProbability > 0 && runtimeSecs > 0
    ? probabilityAtLeastOne((pool?.totalHashRate ?? 0), bestForProbability, runtimeSecs)
    : null;

  const templateAgeSecs = pool?.templateAgeSecs ?? pool?.currentTemplateAgeSecs ?? null;
  const zmqLastBlockAgeSecs = pool?.lastZmqBlockAt
    ? Math.max(0, Math.floor((Date.now() - new Date(pool.lastZmqBlockAt).getTime()) / 1000))
    : null;
  const rpcHealthy = pool?.rpcHealthy ?? false;
  const zmqConnected = pool?.zmqConnected ?? false;

  const reasons: HealthReason[] = [];
  if (!rpcHealthy) {
    reasons.push({
      label: "RPC health",
      detail: "getblocktemplate or getmininginfo has not been confirmed healthy",
      severity: "critical",
    });
  }
  if (!zmqConnected) {
    reasons.push({
      label: "ZMQ connection",
      detail: "block/tx endpoints are not configured or not connected",
      severity: "critical",
    });
  }
  if ((pool?.templateRefreshFailures ?? 0) > 0) {
    reasons.push({
      label: "Template refresh failures",
      detail: `${pool?.templateRefreshFailures ?? 0} failures logged`,
      severity: "warning",
    });
  }
  if ((pool?.templateStale ?? false) || (templateAgeSecs != null && templateAgeSecs > (pool?.templateMaxAgeSecs ?? 30))) {
    reasons.push({
      label: "Template freshness",
      detail: templateAgeSecs != null ? `${templateAgeSecs}s old` : "template missing",
      severity: "critical",
    });
  }
  if (runtimeSecs >= 600 && expectedSharesPerMin > 0 && observedVsExpected < 0.5) {
    reasons.push({
      label: "Share rate lag",
      detail: `${(observedVsExpected * 100).toFixed(1)}% of expected over 10m`,
      severity: "warning",
    });
  }
  if (staleRatio > 0.02) {
    reasons.push({
      label: "Stale ratio",
      detail: `${(staleRatio * 100).toFixed(2)}%`,
      severity: "warning",
    });
  }
  if (rejectRatio > 0.02) {
    reasons.push({
      label: "Reject ratio",
      detail: `${(rejectRatio * 100).toFixed(2)}%`,
      severity: "warning",
    });
  }
  if (duplicateRatio > 0.005) {
    reasons.push({
      label: "Duplicate ratio",
      detail: `${(duplicateRatio * 100).toFixed(2)}%`,
      severity: "warning",
    });
  }
  if ((pool?.submitRttP99Ms ?? 0) > 50) {
    reasons.push({
      label: "Submit RTT p99",
      detail: `${(pool?.submitRttP99Ms ?? 0).toFixed(2)}ms`,
      severity: "warning",
    });
  }
  if (bestProbability !== null && bestProbability < 0.01) {
    reasons.push({
      label: "Current best probability",
      detail: `${(bestProbability * 100).toFixed(3)}% chance to see this best`,
      severity: "warning",
    });
  }
  if (zmqLastBlockAgeSecs != null && zmqLastBlockAgeSecs > 120) {
    reasons.push({
      label: "ZMQ block age",
      detail: `${zmqLastBlockAgeSecs}s since last block event`,
      severity: "warning",
    });
  }
  if ((pool?.currentBlockHeight ?? 0) === 0) {
    reasons.push({
      label: "Block height",
      detail: "unknown current block height",
      severity: "critical",
    });
  }

  const severity: HealthSeverity = reasons.some((r) => r.severity === "critical")
    ? "critical"
    : reasons.length > 0
      ? "warning"
      : "ok";

  return {
    severity,
    reasons,
    expectedSharesPerMin,
    observedSharesPerMin,
    observedVsExpected,
    bestProbability,
    staleRatio,
    rejectRatio,
    duplicateRatio,
    runtimeSecs,
    templateAgeSecs,
    zmqLastBlockAgeSecs,
    rpcHealthy,
    zmqConnected,
  };
}

// ─── BlockFinder Core Visual ──────────────────────────────────────────────────

function CoreVisual({ pool }: { pool: PoolStats | null }) {
  return (
    <div className="bh-core-wrap bh-card bh-card-orange">
      <div className="bh-card-title">
        <span className="bh-card-title-dot" style={{ background: "var(--orange)" }} />
        BlockFinder Engine
      </div>

      <div className="bh-core-container">
        <div className="bh-ring bh-ring-1" />
        <div className="bh-ring bh-ring-2" />
        <div className="bh-ring bh-ring-3" />
        <div className="bh-ring bh-ring-4" />
        <div className="bh-core-glow" />
        <div className="bh-core-accent-ring" />
        <div className="bh-core-center">
          <img src="/blockfinder-logo.svg" alt="BlockFinder" className="bh-core-logo-img" />
        </div>
        <span className="bh-core-label">SINGULARITY CORE</span>
      </div>

      <div className="bh-core-stats">
        <div className="bh-core-stat">
          <div className="bh-core-stat-val">
            {fmtHr((pool?.totalHashRate ?? 0) / 1e9)}
          </div>
          <div className="bh-core-stat-lbl">Hashrate</div>
        </div>
        <div className="bh-core-stat">
          <div className="bh-core-stat-val" style={{ color: "var(--blue)", textShadow: "0 0 12px var(--blue-glow)" }}>
            {pool?.totalMiners ?? 0}
          </div>
          <div className="bh-core-stat-lbl">Miners</div>
        </div>
        <div className="bh-core-stat">
          <div className="bh-core-stat-val" style={{ color: "var(--cyan)" }}>
            {pool ? fmtUptime(pool.uptimeSecs) : "—"}
          </div>
          <div className="bh-core-stat-lbl">Uptime</div>
        </div>
      </div>

      {/* mining flow */}
      <div className="bh-flow" style={{ marginTop: 20 }}>
        <span className="bh-flow-node">Bitcoin Core</span>
        <span className="bh-flow-arrow">→</span>
        <span className="bh-flow-node" style={{ color: "var(--orange)", borderColor: "rgba(249,115,22,.3)" }}>BlockFinder</span>
        <span className="bh-flow-arrow">→</span>
        <span className="bh-flow-node">Miners</span>
        <span className="bh-flow-arrow">→</span>
        <span className="bh-flow-node">Shares</span>
        <span className="bh-flow-arrow">→</span>
        <span className="bh-flow-node">Block?</span>
      </div>
    </div>
  );
}

// ─── Bitcoin Core Panel ───────────────────────────────────────────────────────

function BitcoinCorePanel({ pool, network }: { pool: PoolStats | null; network: NetworkInfo | null }) {
  const hasBlocks = (pool?.zmqBlocksDetected ?? 0) > 0;
  const zmqRatio = hasBlocks
    ? (pool!.zmqBlockNotifications / pool!.zmqBlocksDetected).toFixed(3)
    : null;
  const zmqRatioOk = zmqRatio !== null && parseFloat(zmqRatio) >= 1.8;
  const txTotal = (pool?.zmqTxTriggered ?? 0) + (pool?.zmqTxDebounced ?? 0) + (pool?.zmqTxPostBlockSuppressed ?? 0);
  const suppressPct = txTotal > 0 ? ((txTotal - (pool?.zmqTxTriggered ?? 0)) / txTotal * 100).toFixed(0) : "—";

  return (
    <div className="bh-card bh-card-blue bh-animate bh-animate-d1">
      <div className="bh-card-title">
        <span className="bh-card-title-dot" style={{ background: "var(--blue)" }} />
        Bitcoin Core
      </div>

      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Block Height</span>
        <span className="bh-stat-row-value blue">#{(pool?.blockHeight ?? network?.blocks ?? 0).toLocaleString()}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Network Difficulty</span>
        <span className="bh-stat-row-value dim">{fmtDiff(network?.difficulty ?? 0)}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Network Hashrate</span>
        <span className="bh-stat-row-value dim">{fmtNetHash(network?.networkhashps ?? 0)}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Pool Share</span>
        <span className="bh-stat-row-value orange">
          {network && pool
            ? `${((pool.totalHashRate / 1e9 * 1e9) / (network.networkhashps || 1) * 100).toFixed(7)}%`
            : "—"}
        </span>
      </div>

      <div style={{ marginTop: 16, marginBottom: 8, fontSize: 10, fontFamily: "var(--font-mono)", letterSpacing: ".10em", color: "var(--text3)", textTransform: "uppercase" }}>
        ZMQ Events
      </div>

      <div className="bh-zmq-ratio">
        <div className="bh-zmq-dot" style={{ background: hasBlocks ? "var(--green)" : "var(--text3)" }} />
        <span className="bh-zmq-label">Blocks detected / notified</span>
        <span className="bh-zmq-val" style={{ color: hasBlocks ? "var(--orange)" : "var(--text3)" }}>
          {hasBlocks
            ? `${pool!.zmqBlocksDetected} / ${pool!.zmqBlockNotifications}`
            : "waiting for block…"}
        </span>
      </div>
      <div className="bh-zmq-ratio" style={{ marginTop: 6 }}>
        <div className="bh-zmq-dot" style={{
          background: zmqRatio === null ? "var(--text3)" : zmqRatioOk ? "var(--green)" : "var(--yellow)"
        }} />
        <span className="bh-zmq-label">Dual ZMQ ratio</span>
        <span className="bh-zmq-val" style={{
          color: zmqRatio === null ? "var(--text3)" : zmqRatioOk ? "var(--green)" : "var(--yellow)"
        }}>
          {zmqRatio === null
            ? "waiting for block…"
            : `${zmqRatio} / 2.000 ${zmqRatioOk ? "✓" : "⚠"}`}
        </span>
      </div>
      <div className="bh-zmq-ratio" style={{ marginTop: 6 }}>
        <div className="bh-zmq-dot" style={{ background: "var(--cyan)" }} />
        <span className="bh-zmq-label">TX suppress rate</span>
        <span className="bh-zmq-val" style={{ color: "var(--cyan)" }}>{suppressPct}%</span>
      </div>

      <div style={{ marginTop: 14 }}>
        <div className="bh-stat-row">
          <span className="bh-stat-row-label">TX Triggered</span>
          <span className="bh-stat-row-value dim">{fmtK(pool?.zmqTxTriggered ?? 0)}</span>
        </div>
        <div className="bh-stat-row">
          <span className="bh-stat-row-label">TX Debounced</span>
          <span className="bh-stat-row-value dim">{fmtK(pool?.zmqTxDebounced ?? 0)}</span>
        </div>
        <div className="bh-stat-row">
          <span className="bh-stat-row-label">Post-Block Suppressed</span>
          <span className="bh-stat-row-value dim">{fmtK(pool?.zmqTxPostBlockSuppressed ?? 0)}</span>
        </div>
        <div className="bh-stat-row">
          <span className="bh-stat-row-label">Notify Deduped</span>
          <span className="bh-stat-row-value dim">{fmtK(pool?.notifyDeduped ?? 0)}</span>
        </div>
      </div>
    </div>
  );
}

// ─── Share Flow Panel ─────────────────────────────────────────────────────────

function ShareFlowPanel({ pool, miners }: { pool: PoolStats | null; miners: Miner[] }) {
  const totalShares = miners.reduce((s, m) => s + m.shares, 0);
  const totalStale = miners.reduce((s, m) => s + (m.stale ?? 0), 0);
  const totalRej = miners.reduce((s, m) => s + m.rejected, 0);
  const avgRtt = miners.length > 0
    ? miners.reduce((s, m) => s + m.submit_rtt_ms, 0) / miners.length
    : 0;
  const submitRttP50 = pool?.submitRttP50Ms ?? 0;
  const submitRttP95 = pool?.submitRttP95Ms ?? 0;
  const submitRttP99 = pool?.submitRttP99Ms ?? 0;
  const submitRttMax = pool?.submitRttMaxMs ?? 0;

  return (
    <div className="bh-card bh-animate bh-animate-d2">
      <div className="bh-card-title">
        <span className="bh-card-title-dot" style={{ background: "var(--green)" }} />
        Share Validation
      </div>

      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Total Accepted</span>
        <span className="bh-stat-row-value green">{totalShares.toLocaleString()}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Rejected</span>
        <span className="bh-stat-row-value" style={{ color: totalRej > 0 ? "var(--red)" : "var(--green)" }}>
          {totalRej}
        </span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Stale (total)</span>
        <span className="bh-stat-row-value" style={{ color: totalStale > 0 ? "var(--yellow)" : "var(--green)" }}>
          {totalStale}
        </span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Stale Ratio</span>
        <span className="bh-stat-row-value" style={{ color: (pool?.staleRatio ?? 0) > 0.01 ? "var(--yellow)" : "var(--green)" }}>
          {((pool?.staleRatio ?? 0) * 100).toFixed(4)}%
        </span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Stale / New Block</span>
        <span className="bh-stat-row-value" style={{ color: (pool?.stalesNewBlock ?? 0) > 0 ? "var(--red)" : "var(--green)" }}>
          {pool?.stalesNewBlock ?? 0}
        </span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Duplicate Shares</span>
        <span className="bh-stat-row-value" style={{ color: (pool?.duplicateShares ?? 0) > 0 ? "var(--yellow)" : "var(--green)" }}>
          {pool?.duplicateShares ?? 0}
        </span>
      </div>

      <div style={{ marginTop: 16, padding: "12px 0 4px" }}>
        <div className="bh-label" style={{ marginBottom: 8 }}>Pool Processing RTT</div>
        <div style={{ fontFamily: "var(--font-mono)", fontSize: 24, fontWeight: 500, color: "var(--blue)", textShadow: "0 0 12px var(--blue-glow)", lineHeight: 1 }}>
          {avgRtt > 0 ? `${(avgRtt * 1000).toFixed(0)}µs` : "—"}
        </div>
        <div style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--text3)", marginTop: 4 }}>
          avg submit-to-ACK (CPU floor)
        </div>
        <div className="bh-bar" style={{ marginTop: 8 }}>
          <div className="bh-bar-fill bh-bar-blue" style={{ width: `${Math.min(100, avgRtt * 2000)}%` }} />
        </div>
      </div>

      <div style={{ marginTop: 16 }}>
        <div className="bh-label" style={{ marginBottom: 8 }}>Submit RTT Percentiles</div>
        <div style={{ display: "grid", gridTemplateColumns: "repeat(2, minmax(0, 1fr))", gap: 8 }}>
          {[
            { label: "p50", value: submitRttP50 },
            { label: "p95", value: submitRttP95 },
            { label: "p99", value: submitRttP99 },
            { label: "max", value: submitRttMax },
          ].map(({ label, value }) => (
            <div key={label} style={{
              padding: "8px 10px",
              background: "var(--card2)",
              border: "1px solid var(--border)",
              borderRadius: "var(--radius)",
            }}>
              <div style={{ fontFamily: "var(--font-mono)", fontSize: 10, color: "var(--text3)", letterSpacing: ".08em" }}>{label.toUpperCase()}</div>
              <div style={{ fontFamily: "var(--font-mono)", fontSize: 18, color: "var(--orange)", lineHeight: 1.2 }}>
                {value > 0 ? `${value.toFixed(2)}ms` : "—"}
              </div>
            </div>
          ))}
        </div>
        <div style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--text3)", marginTop: 8, lineHeight: 1.5 }}>
          {pool
            ? `>50ms ${pool.submitRttOver50MsCount} · >100ms ${pool.submitRttOver100MsCount}`
            : "—"}
        </div>
      </div>

      <div style={{ marginTop: 16 }}>
        <div className="bh-stat-row">
          <span className="bh-stat-row-label">Jobs Sent</span>
          <span className="bh-stat-row-value dim">{fmtK(pool?.jobsSent ?? 0)}</span>
        </div>
        <div className="bh-stat-row">
          <span className="bh-stat-row-label">Clean Jobs</span>
          <span className="bh-stat-row-value orange">{pool?.cleanJobsSent ?? 0}</span>
        </div>
        <div className="bh-stat-row">
          <span className="bh-stat-row-label">Rate Limited</span>
          <span className="bh-stat-row-value dim">{fmtK(pool?.notifyRateLimited ?? 0)}</span>
        </div>
        <div className="bh-stat-row">
          <span className="bh-stat-row-label">Jobs/Miner/Min</span>
          <span className="bh-stat-row-value cyan">
            {(pool?.jobsSentPerMinerPerMin ?? 0).toFixed(2)}
          </span>
        </div>
      </div>
    </div>
  );
}

// ─── Engine Panel ─────────────────────────────────────────────────────────────

function EnginePanel({ pool }: { pool: PoolStats | null }) {
  return (
    <div className="bh-card bh-animate bh-animate-d1">
      <div className="bh-card-title">
        <span className="bh-card-title-dot" style={{ background: "var(--orange)" }} />
        Engine State
      </div>

      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Version Rolling Violations</span>
        <span className="bh-stat-row-value" style={{ color: (pool?.versionRollingViolations ?? 0) > 0 ? "var(--red)" : "var(--green)" }}>
          {pool?.versionRollingViolations ?? 0}
        </span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Reconnects Total</span>
        <span className="bh-stat-row-value dim">{pool?.reconnectsTotal ?? 0}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Stales Expired</span>
        <span className="bh-stat-row-value dim">{pool?.stalesExpired ?? 0}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Stales Reconnect</span>
        <span className="bh-stat-row-value dim">{pool?.stalesReconnect ?? 0}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Pool Started</span>
        <span className="bh-stat-row-value dim" style={{ fontSize: 11 }}>
          {pool ? new Date(pool.poolStartedAt).toLocaleString() : "—"}
        </span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Uptime</span>
        <span className="bh-stat-row-value cyan">{pool ? fmtUptime(pool.uptimeSecs) : "—"}</span>
      </div>

      <div style={{ marginTop: 16 }}>
        <div className="bh-label" style={{ marginBottom: 10 }}>Submit Block</div>
        <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
          {[
            { label: "Accepted", val: pool?.submitblockAccepted ?? 0, color: "var(--green)" },
            { label: "Rejected", val: pool?.submitblockRejected ?? 0, color: "var(--red)" },
            { label: "RPC Fail", val: pool?.submitblockRpcFail ?? 0, color: "var(--yellow)" },
          ].map(({ label, val, color }) => (
            <div key={label} style={{
              flex: 1, minWidth: 80, textAlign: "center",
              padding: "10px 8px",
              background: "var(--card2)",
              border: `1px solid ${val > 0 && label !== "Accepted" ? color : "var(--border)"}`,
              borderRadius: "var(--radius)",
            }}>
              <div style={{ fontFamily: "var(--font-mono)", fontSize: 20, fontWeight: 500, color, lineHeight: 1 }}>{val}</div>
              <div style={{ fontFamily: "var(--font-mono)", fontSize: 9.5, color: "var(--text3)", marginTop: 4, letterSpacing: ".08em" }}>{label}</div>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

// ─── Block Candidate Panel ────────────────────────────────────────────────────

function BlockPanel({ pool, miners, network }: { pool: PoolStats | null; miners: Miner[]; network: NetworkInfo | null }) {
  const bestSubmitted = Math.max(
    pool?.globalBestSubmittedDifficulty ?? 0,
    ...miners.map((m) => m.best_submitted_difficulty ?? 0),
  );
  const bestAccepted = Math.max(
    pool?.globalBestAcceptedDifficulty ?? 0,
    ...miners.map((m) => m.best_accepted_difficulty ?? 0),
  );
  const currentBlockBestSubmitted = Math.max(
    pool?.currentBlockBestSubmittedDifficulty ?? 0,
    ...miners.map((m) => m.current_block_best_submitted_difficulty ?? 0),
  );
  const currentBlockBestAccepted = Math.max(
    pool?.currentBlockBestAcceptedDifficulty ?? 0,
    ...miners.map((m) => m.current_block_best_accepted_difficulty ?? 0),
  );
  const previousBlockBestSubmitted = Math.max(
    pool?.previousBlockBestSubmittedDifficulty ?? 0,
    ...miners.map((m) => m.previous_block_best_submitted_difficulty ?? 0),
  );
  const previousBlockBestAccepted = Math.max(
    pool?.previousBlockBestAcceptedDifficulty ?? 0,
    ...miners.map((m) => m.previous_block_best_accepted_difficulty ?? 0),
  );
  const networkDiff = network?.difficulty ?? 145e12;
  const bestPct = networkDiff > 0 ? (bestSubmitted / networkDiff * 100) : 0;
  const blocksFound = pool?.blocksFound ?? 0;
  const totalHr = (pool?.totalHashRate ?? 0) / 1e9; // in GH/s
  const expectedBlockSecs = networkDiff > 0 && totalHr > 0
    ? (networkDiff * 4294967296) / (totalHr * 1e9)
    : null;

  return (
    <div className={`bh-card ${blocksFound > 0 ? "bh-block-found" : "bh-block-empty"} bh-animate bh-animate-d2`}>
      <div className="bh-card-title">
        <span className="bh-card-title-dot" style={{ background: "var(--yellow)" }} />
        Block Candidate
        {blocksFound > 0 && (
          <span style={{
            marginLeft: "auto", padding: "2px 10px",
            background: "rgba(249,115,22,.2)",
            border: "1px solid var(--orange-mid)",
            borderRadius: 20, fontSize: 11,
            color: "var(--orange)", fontFamily: "var(--font-mono)", letterSpacing: ".06em"
          }}>
            {blocksFound} FOUND
          </span>
        )}
      </div>

      <div className="bh-best-share">
        <div className="bh-best-share-val">{fmtDiff(bestSubmitted)}</div>
        <div className="bh-best-share-pct">
          {bestPct > 0 ? `${bestPct.toFixed(6)}% of network difficulty` : "Awaiting first share…"}
        </div>
      </div>

      {bestSubmitted > 0 && (
        <div>
          <div className="bh-bar" style={{ height: 4, marginTop: 0, marginBottom: 12 }}>
            <div className="bh-bar-fill bh-bar-orange" style={{ width: `${Math.min(100, Math.log10(bestPct + 0.0001) / Math.log10(100) * 100 + 50)}%` }} />
          </div>
        </div>
      )}

      <div className="bh-stat-row">
        <span className="bh-stat-row-label" title="Highest difficulty hash submitted by this miner, including stale/rejected/low-diff submissions if they were parsed successfully.">
          Raw Best Submitted
        </span>
        <span className="bh-stat-row-value yellow">{fmtDiff(bestSubmitted)}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label" title="Highest share accepted by the configured pool difficulty rules.">
          Accepted Best
        </span>
        <span className="bh-stat-row-value green">{fmtDiff(bestAccepted)}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label" title="Alias of Raw Best Submitted. Matches the public-pool style best-difficulty display.">
          PublicPool Style Best
        </span>
        <span className="bh-stat-row-value yellow">{fmtDiff(bestSubmitted)}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label" title="Alias of Accepted Best. Matches cgminer-style accepted-share reporting.">
          CGMiner Style Best
        </span>
        <span className="bh-stat-row-value green">{fmtDiff(bestAccepted)}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label" title="Best share that reached or exceeded the Bitcoin network target.">
          Block Candidate Best
        </span>
        <span className="bh-stat-row-value yellow">{fmtDiff(pool?.globalBestBlockCandidateDifficulty ?? 0)}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Current Block Best Submitted</span>
        <span className="bh-stat-row-value cyan">{fmtDiff(currentBlockBestSubmitted)}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Current Block Best Accepted</span>
        <span className="bh-stat-row-value green">{fmtDiff(currentBlockBestAccepted)}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Current Block Best Candidate</span>
        <span className="bh-stat-row-value orange">{fmtDiff(pool?.currentBlockBestCandidateDifficulty ?? 0)}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Previous Block Best</span>
        <span className="bh-stat-row-value dim">
          {fmtDiff(Math.max(previousBlockBestSubmitted, previousBlockBestAccepted, pool?.previousBlockBestCandidateDifficulty ?? 0))}
        </span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Network Target</span>
        <span className="bh-stat-row-value dim">{fmtDiff(networkDiff)}</span>
      </div>
      <div className="bh-stat-row">
        <span className="bh-stat-row-label">Proximity</span>
        <span className="bh-stat-row-value" style={{ color: bestPct > 1 ? "var(--orange)" : "var(--text3)" }}>
          {bestPct > 0 ? `${bestPct.toFixed(6)}%` : "—"}
        </span>
      </div>

      <div className="bh-expected-block">
        <div className="bh-label" style={{ marginBottom: 8 }}>Expected Block Interval</div>
        <div style={{ fontFamily: "var(--font-mono)", fontSize: 18, color: "var(--blue)", lineHeight: 1 }}>
          {expectedBlockSecs
            ? fmtBlockInterval(expectedBlockSecs)
            : "—"}
        </div>
        <div style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--text3)", marginTop: 4 }}>
          at current {fmtHr((pool?.totalHashRate ?? 0) / 1e9)}
        </div>
      </div>

      {blocksFound === 0 && (
        <div style={{
          marginTop: 14, padding: "10px 14px",
          background: "rgba(251,191,36,.05)",
          border: "1px solid rgba(251,191,36,.12)",
          borderRadius: "var(--radius)",
          fontFamily: "var(--font-mono)", fontSize: 11,
          color: "var(--text3)", lineHeight: 1.5,
        }}>
          No block found yet. Mining in progress — every hash is a lottery ticket.
        </div>
      )}
    </div>
  );
}

// ─── System Health Panel ──────────────────────────────────────────────────────

function SystemHealth({ pool, miners }: { pool: PoolStats | null; miners: Miner[] }) {
  const staleRatioOk = (pool?.staleRatio ?? 0) < 0.005;
  const zmqOk = pool ? (pool.zmqBlockNotifications === 0 || pool.zmqBlockNotifications / Math.max(1, pool.zmqBlocksDetected) >= 1.8) : false;
  const templateOk = pool ? !pool.templateStale : false;
  const zmqConnectedOk = pool?.zmqConnected ?? false;
  const templateAgeSecs = pool?.currentTemplateAgeSecs ?? null;
  const vrOk = (pool?.versionRollingViolations ?? 0) === 0;
  const dupOk = (pool?.duplicateShares ?? 0) === 0;
  const rpcOk = (pool?.submitblockRpcFail ?? 0) === 0;
  const staleNewBlockOk = (pool?.stalesNewBlock ?? 0) === 0;
  const allOk = staleRatioOk && zmqOk && templateOk && zmqConnectedOk && vrOk && dupOk && rpcOk && staleNewBlockOk;

  return (
    <div className="bh-card bh-animate bh-animate-d3">
      <div className="bh-card-title">
        <span className="bh-card-title-dot" style={{ background: allOk ? "var(--green)" : "var(--yellow)" }} />
        System Health
        <span style={{
          marginLeft: "auto", padding: "2px 10px", borderRadius: 20,
          fontFamily: "var(--font-mono)", fontSize: 10, letterSpacing: ".08em",
          background: allOk ? "var(--green-lo)" : "var(--yellow-lo)",
          border: `1px solid ${allOk ? "rgba(74,222,128,.25)" : "rgba(251,191,36,.25)"}`,
          color: allOk ? "var(--green)" : "var(--yellow)",
        }}>
          {allOk ? "ALL OK" : "WARN"}
        </span>
      </div>

      {[
        { label: "Stale Rate", ok: staleRatioOk, detail: `${((pool?.staleRatio ?? 0) * 100).toFixed(4)}%` },
        { label: "New-Block Stales", ok: staleNewBlockOk, detail: String(pool?.stalesNewBlock ?? 0) },
        { label: "ZMQ Dual Endpoints", ok: zmqOk, detail: pool && pool.zmqBlocksDetected > 0 ? `${(pool.zmqBlockNotifications / pool.zmqBlocksDetected).toFixed(2)}x` : "idle" },
        { label: "Template Freshness", ok: templateOk, detail: templateAgeSecs != null ? `${fmtMs(templateAgeSecs * 1000)} / ${pool!.templateMaxAgeSecs}s` : "idle" },
        { label: "ZMQ Connected", ok: zmqConnectedOk, detail: pool?.zmqConnected ? `block+tx` : "offline" },
        { label: "Version Rolling", ok: vrOk, detail: `${pool?.versionRollingViolations ?? 0} violations` },
        { label: "Duplicate Shares", ok: dupOk, detail: String(pool?.duplicateShares ?? 0) },
        { label: "RPC Failures", ok: rpcOk, detail: String(pool?.submitblockRpcFail ?? 0) },
      ].map(({ label, ok, detail }) => (
        <div key={label} className="bh-health-item">
          <span className="bh-health-name">
            <span className={`bh-indicator ${ok ? "bh-ind-green" : "bh-ind-orange"}`} />
            {label}
          </span>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <span style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--text3)" }}>{detail}</span>
            <span className={`bh-health-badge ${ok ? "bh-health-ok" : "bh-health-warn"}`}>
              {ok ? "OK" : "WARN"}
            </span>
          </div>
        </div>
      ))}

      <div style={{ marginTop: 14 }}>
        <div className="bh-label" style={{ marginBottom: 8 }}>Active Sessions</div>
        <div style={{ display: "flex", flexWrap: "wrap", gap: 6 }}>
          {miners.map((m) => {
            const secsAgo = (Date.now() - new Date(m.last_seen).getTime()) / 1000;
            const ok = secsAgo < 60;
            return (
              <div key={m.worker} style={{
                padding: "3px 8px",
                background: ok ? "var(--green-lo)" : "var(--red-lo)",
                border: `1px solid ${ok ? "rgba(74,222,128,.2)" : "rgba(248,113,113,.2)"}`,
                borderRadius: 4,
                fontFamily: "var(--font-mono)", fontSize: 10,
                color: ok ? "var(--green)" : "var(--red)",
              }} title={`Last seen: ${timeAgo(m.last_seen)}`}>
                {shortWorker(m.worker)}
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

// ─── Mining Health Panel ─────────────────────────────────────────────────────

function MiningHealthPanel({
  pool,
  miners,
  hashrate,
}: {
  pool: PoolStats | null;
  miners: Miner[];
  hashrate: HashrateResponse | null;
}) {
  if (!pool) {
    return (
      <div className="bh-card bh-animate bh-animate-d4 bh-health-card">
        <div className="bh-card-title">
          <span className="bh-card-title-dot" style={{ background: "var(--text3)" }} />
          Mining Health
        </div>
        <div className="bh-block-feed-empty">Waiting for pool metrics…</div>
      </div>
    );
  }

  const health = deriveMiningHealth(pool, miners, hashrate);
  const severityLabel = health.severity === "critical"
    ? "Critical"
    : health.severity === "warning"
      ? "Warning"
      : "OK";
  const severityClass = health.severity === "critical"
    ? "bh-health-err"
    : health.severity === "warning"
      ? "bh-health-warn"
      : "bh-health-ok";

  const bestProbPct = health.bestProbability == null ? null : health.bestProbability * 100;
  const blockLabel = pool ? `Block #${pool.blockHeight.toLocaleString()}` : "Block —";

  return (
    <div className="bh-card bh-animate bh-animate-d4 bh-health-card">
      <div className="bh-card-title">
        <span className="bh-card-title-dot" style={{ background: health.severity === "critical" ? "var(--red)" : health.severity === "warning" ? "var(--yellow)" : "var(--green)" }} />
        Mining Health
        <span className={`bh-health-badge ${severityClass}`} style={{ marginLeft: "auto" }}>
          {severityLabel}
        </span>
      </div>

      <div className="bh-health-grid">
        <div className="bh-health-kpi">
          <div className="bh-health-kpi-label">Expected / min</div>
          <div className="bh-health-kpi-value">{health.expectedSharesPerMin > 0 ? health.expectedSharesPerMin.toFixed(2) : "—"}</div>
        </div>
        <div className="bh-health-kpi">
          <div className="bh-health-kpi-label">Observed / min</div>
          <div className="bh-health-kpi-value">{health.observedSharesPerMin > 0 ? health.observedSharesPerMin.toFixed(2) : "—"}</div>
        </div>
        <div className="bh-health-kpi">
          <div className="bh-health-kpi-label">Observed / Expected</div>
          <div className="bh-health-kpi-value">{health.expectedSharesPerMin > 0 ? `${(health.observedVsExpected * 100).toFixed(1)}%` : "—"}</div>
        </div>
        <div className="bh-health-kpi">
          <div className="bh-health-kpi-label">Current best chance</div>
          <div className="bh-health-kpi-value">{bestProbPct != null ? `${bestProbPct.toFixed(3)}%` : "—"}</div>
        </div>
      </div>

      <div className="bh-health-meta">
        <span>{blockLabel}</span>
        <span>Template {health.templateAgeSecs != null ? `${health.templateAgeSecs}s` : "—"}</span>
        <span>ZMQ block age {health.zmqLastBlockAgeSecs != null ? `${health.zmqLastBlockAgeSecs}s` : "—"}</span>
        <span>RPC {health.rpcHealthy ? "healthy" : "down"}</span>
        <span>ZMQ {health.zmqConnected ? "connected" : "down"}</span>
        <span>Last clean jobs {pool.lastCleanJobsNotifyAt ? timeAgo(pool.lastCleanJobsNotifyAt) : "—"}</span>
        <span>Runtime {fmtUptime(health.runtimeSecs)}</span>
      </div>

      <div className="bh-health-metrics">
        <div className="bh-health-metric">
          <span>Stale ratio</span>
          <strong style={{ color: health.staleRatio > 0.02 ? "var(--yellow)" : "var(--green)" }}>
            {(health.staleRatio * 100).toFixed(4)}%
          </strong>
        </div>
        <div className="bh-health-metric">
          <span>Reject ratio</span>
          <strong style={{ color: health.rejectRatio > 0.02 ? "var(--yellow)" : "var(--green)" }}>
            {(health.rejectRatio * 100).toFixed(4)}%
          </strong>
        </div>
        <div className="bh-health-metric">
          <span>Duplicate ratio</span>
          <strong style={{ color: health.duplicateRatio > 0.005 ? "var(--yellow)" : "var(--green)" }}>
            {(health.duplicateRatio * 100).toFixed(4)}%
          </strong>
        </div>
        <div className="bh-health-metric">
          <span>Submit RTT p99</span>
          <strong style={{ color: (pool?.submitRttP99Ms ?? 0) > 50 ? "var(--yellow)" : "var(--green)" }}>
            {(pool?.submitRttP99Ms ?? 0) > 0 ? `${pool!.submitRttP99Ms.toFixed(2)}ms` : "—"}
          </strong>
        </div>
        <div className="bh-health-metric">
          <span>Current block height</span>
          <strong style={{ color: "var(--cyan)" }}>
            {pool ? `#${pool.blockHeight.toLocaleString()}` : "—"}
          </strong>
        </div>
        <div className="bh-health-metric">
          <span>Current block best</span>
          <strong style={{ color: "var(--orange)" }}>
            {pool ? fmtDiff(pool.currentBlockBestSubmittedDifficulty) : "—"}
          </strong>
        </div>
      </div>

      <div className="bh-health-reasons">
        <div className="bh-health-reasons-head">Reasons</div>
        {health.reasons.length > 0 ? (
          health.reasons.map((reason) => (
            <div key={`${reason.label}:${reason.detail}`} className={`bh-health-reason bh-health-reason-${reason.severity}`}>
              <div className="bh-health-reason-label">
                <span className="bh-health-reason-dot" />
                {reason.label}
              </div>
              <div className="bh-health-reason-detail">{reason.detail}</div>
            </div>
          ))
        ) : (
          <div className="bh-health-reason bh-health-reason-ok">
            <div className="bh-health-reason-label">
              <span className="bh-health-reason-dot" />
              No anomalies detected
            </div>
            <div className="bh-health-reason-detail">
              Share rate, latency and template freshness are within normal bounds.
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

// ─── Miner Fleet ──────────────────────────────────────────────────────────────

/** Extract the payout address from a worker name like "bc1qXXX.Work1" → "bc1qXXX" */
function minerPayout(worker: string): string {
  const dot = worker.lastIndexOf(".");
  return dot > 0 ? worker.slice(0, dot) : worker;
}

function MinerFleet({ miners, network }: { miners: Miner[]; network: NetworkInfo | null }) {
  const [sort, setSort] = useState<"hr" | "best" | "shares" | "rtt">("hr");

  const sorted = [...miners].sort((a, b) => {
    switch (sort) {
      case "best":   return (b.best_submitted_difficulty ?? 0) - (a.best_submitted_difficulty ?? 0);
      case "shares": return b.shares - a.shares;
      case "rtt":    return a.submit_rtt_ms - b.submit_rtt_ms;
      default:       return b.hashrate_gh - a.hashrate_gh;
    }
  });

  const maxRtt = Math.max(...miners.map((m) => m.submit_rtt_ms), 0.001);
  const maxHr  = Math.max(...miners.map((m) => m.hashrate_gh), 1);

  function fwClass(ua: string | null): string {
    const fw = getFirmwareLabel(ua);
    if (fw === "Bitaxe")    return "bh-fw-bitaxe";
    if (fw === "NerdQAxe")  return "bh-fw-nerdqaxe";
    if (fw === "cgminer")   return "bh-fw-cgminer";
    return "";
  }

  // Best share coloring: RELATIVE + TIER
  // The worker with the HIGHEST best_diff right now = yellow/gold (the champion)
  // If someone overtakes, old champion turns green, new champion turns yellow
  //   ≥ 1T  → purple  (extraordinary)
  //   ≥ 100G → red    (exceptional)
  //   Champion (max across all miners) → yellow (current leader)
  //   ≥ 1G (non-champion) → green  (great, but not the best right now)
  //   < 1G  → white  (normal)
  const maxBestDiff = Math.max(...miners.map(m => m.best_submitted_difficulty ?? 0), 0);

  function bestDiffColor(d: number): string {
    if (d <= 0)                  return "var(--text3)";
    if (d >= 1e12)               return "var(--purple)";  // ≥ 1T
    if (d >= 1e11)               return "var(--red)";     // ≥ 100G
    if (d === maxBestDiff && d > 0) return "var(--yellow)"; // current champion
    if (d >= 1e9)                return "var(--green)";   // ≥ 1G, not champion
    return "var(--text)";                                  // < 1G white
  }

  function bestDiffGlow(d: number): string {
    if (d >= 1e12)               return "0 0 12px rgba(167,139,250,.55)";
    if (d >= 1e11)               return "0 0 10px rgba(248,113,113,.45)";
    if (d === maxBestDiff && d > 0) return "0 0 12px rgba(251,191,36,.55)";
    if (d >= 1e9)                return "0 0 8px rgba(74,222,128,.35)";
    return "none";
  }

  const th: React.CSSProperties = { cursor: "pointer" };
  const thActive: React.CSSProperties = { cursor: "pointer", color: "var(--orange)" };

  return (
    <div className="bh-card bh-animate bh-animate-d4" style={{ marginTop: 0, padding: "20px 20px 16px" }}>
      <div className="bh-fleet-header">
        <span className="bh-fleet-title">
          <span style={{ color: "var(--orange)" }}>◈</span>
          Miner Fleet
          <span className="bh-fleet-count">{miners.length} active</span>
        </span>
        <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
          {[
            { key: "hr", label: "Hashrate", title: "Sort by current hashrate" },
            { key: "best", label: "Raw Best", title: "Sort by highest raw submitted share" },
            { key: "shares", label: "Shares", title: "Sort by accepted share count" },
            { key: "rtt", label: "RTT", title: "Sort by pool processing time" },
          ].map(({ key, label, title }) => (
            <button
              key={key}
              onClick={() => setSort(key as typeof sort)}
              title={title}
              style={{
                padding: "4px 10px",
                background: sort === key ? "var(--orange-lo)" : "var(--card2)",
                border: `1px solid ${sort === key ? "var(--orange-mid)" : "var(--border)"}`,
                borderRadius: 6,
                fontFamily: "var(--font-mono)", fontSize: 10, letterSpacing: ".08em",
                color: sort === key ? "var(--orange)" : "var(--text3)",
                cursor: "pointer", whiteSpace: "nowrap",
              }}
            >
              {label}
            </button>
          ))}
        </div>
      </div>

      <div className="bh-fleet-table-wrap">
        <table className="bh-fleet-table">
          <thead>
            <tr>
              <th style={{ width: 40 }}>#</th>
              <th>Worker</th>
              <th>Payout</th>
              <th style={sort === "hr" ? thActive : th} onClick={() => setSort("hr")}>Hashrate ↕</th>
              <th>Difficulty</th>
              <th style={sort === "best" ? thActive : th} onClick={() => setSort("best")}>Raw Best Submitted ↕</th>
              <th>Accepted Best</th>
              <th>Current Block Best</th>
              <th style={sort === "shares" ? thActive : th} onClick={() => setSort("shares")}>Shares ↕</th>
              <th>Stale</th>
              <th style={sort === "rtt" ? thActive : th} onClick={() => setSort("rtt")}>RTT ↕</th>
              <th>N2S</th>
              <th>Last Seen</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map((m, i) => {
              const secsAgo = (Date.now() - new Date(m.last_seen).getTime()) / 1000;
              const hrBar = pct(m.hashrate_gh, maxHr);
              const rttBar = pct(m.submit_rtt_ms, maxRtt);
              const rankClass = i === 0 ? "bh-miner-rank-1" : i === 1 ? "bh-miner-rank-2" : i === 2 ? "bh-miner-rank-3" : "";
              const fw = getFirmwareLabel(m.user_agent);

              return (
                <tr key={m.worker}>
                  <td>
                    <span className={`bh-miner-rank ${rankClass}`}>{i + 1}</span>
                  </td>
                  <td>
                    <span className="bh-worker-name">{shortWorker(m.worker)}</span>
                    <span className={`bh-fw-badge ${fwClass(m.user_agent)}`}>{fw}</span>
                  </td>
                  <td>
                    <span style={{
                      fontFamily: "var(--font-mono)", fontSize: 11,
                      color: "var(--text2)",
                      background: "var(--card2)",
                      border: "1px solid var(--border)",
                      borderRadius: 4,
                      padding: "2px 7px",
                      letterSpacing: ".01em",
                    }} title={minerPayout(m.worker)}>
                      {shortAddress(minerPayout(m.worker))}
                    </span>
                  </td>
                  <td>
                    <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                      <span style={{ color: "var(--cyan)", minWidth: 70 }}>{fmtHr(m.hashrate_gh)}</span>
                      <div className="bh-bar" style={{ width: 50, marginTop: 0 }}>
                        <div className="bh-bar-fill" style={{ width: `${hrBar}%`, background: "var(--cyan)" }} />
                      </div>
                    </div>
                  </td>
                  <td style={{ color: "var(--text3)" }}>{fmtDiff(m.difficulty)}</td>
                  <td>
                    <span style={{
                      color: bestDiffColor(m.best_submitted_difficulty),
                      fontWeight: m.best_submitted_difficulty >= 1e9 ? 600 : 500,
                      textShadow: bestDiffGlow(m.best_submitted_difficulty),
                    }}>
                      {fmtDiff(m.best_submitted_difficulty)}
                    </span>
                    {network && m.best_submitted_difficulty > 0 && (
                      <span style={{ marginLeft: 6, fontSize: 10, color: "var(--text3)" }}>
                        {(m.best_submitted_difficulty / network.difficulty * 100).toFixed(6)}%
                      </span>
                    )}
                  </td>
                  <td>
                    <span style={{ color: "var(--green)" }} title="Accepted by configured share difficulty.">
                      {fmtDiff(m.best_accepted_difficulty)}
                    </span>
                  </td>
                  <td>
                    <span style={{ color: "var(--cyan)" }} title="Best share in the current block/template window.">
                      {fmtDiff(m.current_block_best_submitted_difficulty)}
                    </span>
                  </td>
                  <td style={{ color: "var(--text)" }}>{m.shares.toLocaleString()}</td>
                  <td>
                    {(m.stale ?? 0) > 0
                      ? <span style={{ color: "var(--yellow)" }}>{m.stale}</span>
                      : <span style={{ color: "var(--text3)" }}>0</span>}
                  </td>
                  <td>
                    <div style={{ display: "flex", alignItems: "center", gap: 5 }}>
                      <span style={{ color: "var(--blue)", minWidth: 52 }}>
                        {(m.submit_rtt_ms * 1000).toFixed(0)}µs
                      </span>
                      <div className="bh-rtt-bar">
                        <div className="bh-rtt-fill" style={{ width: `${rttBar}%` }} />
                      </div>
                    </div>
                  </td>
                  <td style={{ color: "var(--text3)" }}>
                    {m.notify_to_submit_ms > 0 ? fmtMs(m.notify_to_submit_ms) : "—"}
                  </td>
                  <td>
                    <span className={`bh-indicator ${secsAgo < 60 ? "bh-ind-green" : secsAgo < 300 ? "bh-ind-orange" : "bh-ind-dim"}`} />
                    <span style={{ color: "var(--text3)", fontSize: 11 }}>{timeAgo(m.last_seen)}</span>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}

// ─── Header ───────────────────────────────────────────────────────────────────

function Header({ live }: { live: boolean }) {
  return (
    <header className="bh-header">
      <div className="bh-logo">
        <img src="/blockfinder-logo.svg" alt="BlockFinder" className="bh-logo-img" />
        <div className="bh-logo-text">Block<span>Finder</span></div>
      </div>

      <div className={`bh-status-badge ${live ? "bh-status-live" : "bh-status-offline"}`}>
        {live ? "LIVE" : "OFFLINE"}
      </div>
    </header>
  );
}

// ─── Hero ─────────────────────────────────────────────────────────────────────

// ─── Network Bar ──────────────────────────────────────────────────────────────

function NetworkBar({ pool, network }: { pool: PoolStats | null; network: NetworkInfo | null }) {
  const poolHr = (pool?.totalHashRate ?? 0);
  const netHr = network?.networkhashps ?? 0;
  const sharePct = netHr > 0 ? (poolHr / netHr * 100) : 0;

  return (
    <div className="bh-netbar">
      {[
        { label: "Block Height", value: `#${(pool?.blockHeight ?? network?.blocks ?? 0).toLocaleString()}`, color: "var(--blue)" },
        { label: "Network Difficulty", value: fmtDiff(network?.difficulty ?? 0), color: "var(--text)" },
        { label: "Network Hashrate", value: fmtNetHash(network?.networkhashps ?? 0), color: "var(--text)" },
        { label: "Pool Hashrate", value: fmtHr(poolHr / 1e9), color: "var(--orange)" },
        { label: "Pool Share", value: sharePct > 0 ? `${sharePct.toFixed(8)}%` : "—", color: "var(--cyan)" },
        { label: "Blocks Found", value: String(pool?.blocksFound ?? 0), color: (pool?.blocksFound ?? 0) > 0 ? "var(--orange)" : "var(--text3)" },
      ].map(({ label, value, color }) => (
        <div key={label} className="bh-netbar-item">
          <div className="bh-netbar-label">{label}</div>
          <div className="bh-netbar-value" style={{ color }}>{value}</div>
        </div>
      ))}
    </div>
  );
}

// ─── Footer ───────────────────────────────────────────────────────────────────

function Footer({ pool, miners }: { pool: PoolStats | null; miners: Miner[] }) {
  // Collect unique payout addresses from all active miners
  const uniquePayouts = Array.from(new Set(miners.map(m => minerPayout(m.worker)))).filter(Boolean);
  return (
    <footer className="bh-footer">
      <div className="bh-footer-info">
        <span>BLACKHOLE MINING POOL</span>
        <span style={{ color: "var(--border2)" }}>·</span>
        {uniquePayouts.length === 1 ? (
          <span title={uniquePayouts[0]}>Payout: {shortAddress(uniquePayouts[0])}</span>
        ) : uniquePayouts.length > 1 ? (
          <span>{uniquePayouts.length} payout addresses</span>
        ) : (
          <span style={{ color: "var(--text3)" }}>No active miners</span>
        )}
        <span style={{ color: "var(--border2)" }}>·</span>
        <span>Fee: 0%</span>
        <span style={{ color: "var(--border2)" }}>·</span>
        <span>Stratum: :3333</span>
        <span style={{ color: "var(--border2)" }}>·</span>
        <span title="Donate address">Donate: bc1pzvqagy932kmts9rluzpq39upk0hnttz22gdyeslf8lpc4aepyrqslfds96</span>
      </div>
      <div style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: "var(--text3)" }}>
        Refresh 5s · {new Date().toLocaleTimeString()}
      </div>
    </footer>
  );
}

// ─── Block Found Celebration ──────────────────────────────────────────────────

// Falling particles
const PARTICLES = Array.from({ length: 55 }, (_, i) => ({
  id: i,
  left: `${Math.random() * 100}%`,
  delay: `${Math.random() * 2.5}s`,
  duration: `${2.5 + Math.random() * 3}s`,
  size: `${3 + Math.random() * 7}px`,
  color: i % 5 === 0 ? "var(--purple)" :
         i % 4 === 0 ? "var(--blue)"   :
         i % 3 === 0 ? "var(--yellow)" :
         i % 2 === 0 ? "var(--orange)" : "var(--green)",
  shape: i % 3 === 0 ? "50%" : i % 3 === 1 ? "2px" : "0%",
}));

// Orbit dots around rings
const RING_DOTS = [
  { size: 216, speed: "38s", dir: "normal",  w: 4, color: "rgba(249,115,22,.7)" },
  { size: 172, speed: "22s", dir: "reverse", w: 3, color: "rgba(96,165,250,.7)" },
  { size: 130, speed: "13s", dir: "normal",  w: 3, color: "rgba(249,115,22,.8)" },
  { size:  94, speed: "7s",  dir: "reverse", w: 2, color: "rgba(34,211,238,.6)" },
];

function BlockFinderAnim() {
  const SIZE = 240;
  return (
    <div style={{ width: SIZE, height: SIZE, position: "relative", flexShrink: 0 }}>
      {/* Rings with orbit dots */}
      {RING_DOTS.map((r, i) => (
        <div key={i} style={{
          position: "absolute",
          width: r.size, height: r.size,
          top: (SIZE - r.size) / 2, left: (SIZE - r.size) / 2,
          borderRadius: "50%",
          border: `1px solid rgba(249,115,22,${.06 + i * .04})`,
          animation: `ring-spin ${r.speed} linear ${r.dir} infinite`,
        }}>
          <div style={{
            position: "absolute",
            width: r.w, height: r.w, borderRadius: "50%",
            background: r.color, boxShadow: `0 0 6px ${r.color}`,
            top: -r.w / 2, left: "50%", transform: "translateX(-50%)",
          }} />
        </div>
      ))}

      {/* Accretion glow */}
      <div style={{
        position: "absolute",
        width: 100, height: 100,
        top: (SIZE - 100) / 2, left: (SIZE - 100) / 2,
        borderRadius: "50%",
        background: "radial-gradient(circle, rgba(249,115,22,.45) 0%, rgba(249,115,22,.15) 45%, transparent 70%)",
        animation: "bh-breathe 3.5s ease-in-out infinite",
      }} />

      {/* Event horizon */}
      <div style={{
        position: "absolute",
        width: 46, height: 46,
        top: (SIZE - 46) / 2, left: (SIZE - 46) / 2,
        borderRadius: "50%",
        background: "radial-gradient(circle, #000 50%, #070208 100%)",
        border: "2px solid rgba(249,115,22,.55)",
        animation: "bh-horizon-pulse 3s ease-in-out infinite",
        zIndex: 2,
      }} />

      {/* Spiraling block — orbit wrapper anchored at exact center */}
      <div style={{
        position: "absolute",
        top: SIZE / 2, left: SIZE / 2,
        width: 0, height: 0,
        animation: "bh-orbit-rotate 10s linear infinite",
        zIndex: 3,
      }}>
        {/* The block moves inward along the radial axis */}
        <div style={{
          position: "absolute",
          background: "linear-gradient(135deg, var(--orange), var(--yellow))",
          border: "1px solid rgba(249,115,22,.9)",
          boxShadow: "0 0 10px var(--orange), 0 0 20px rgba(249,115,22,.4)",
          animation: "bh-block-inward 10s linear infinite",
        }} />
      </div>
    </div>
  );
}

function BlockFoundOverlay({ blocksFound, onDismiss }: {
  blocksFound: number;
  onDismiss: () => void;
}) {
  useEffect(() => {
    const t = setTimeout(onDismiss, 18000);
    return () => clearTimeout(t);
  }, [onDismiss]);

  return (
    <div
      onClick={onDismiss}
      style={{
        position: "fixed",
        top: 0, left: 0, right: 0, bottom: 0,
        zIndex: 9999,
        cursor: "pointer",
        backdropFilter: "blur(3px)",
        background: "radial-gradient(ellipse at 50% 50%, rgba(249,115,22,.14) 0%, rgba(2,2,8,.94) 65%)",
        overflow: "hidden",
        /* Single flex column — everything centered */
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        justifyContent: "center",
      }}
    >
      {/* Rain particles — absolute, never affect flex layout */}
      {PARTICLES.map(p => (
        <div key={p.id} style={{
          position: "absolute",
          left: p.left, top: "-20px",
          width: p.size, height: p.size,
          borderRadius: p.shape,
          background: p.color,
          boxShadow: `0 0 5px ${p.color}`,
          animation: `block-particle ${p.duration} ${p.delay} ease-in forwards`,
          pointerEvents: "none",
        }} />
      ))}

      {/* ── Centered content column ── */}
      <div style={{
        display: "flex", flexDirection: "column", alignItems: "center",
        textAlign: "center",
        position: "relative", zIndex: 4,
        pointerEvents: "none",
      }}>

        {/* Black hole animation — centered via flex parent */}
        <BlockFinderAnim />

        <div style={{ height: 24 }} />

        <div style={{
          fontFamily: "var(--font-head)", fontSize: 44, fontWeight: 700,
          letterSpacing: "-.02em", color: "#fff",
          textShadow: "0 0 40px rgba(249,115,22,.65), 0 0 80px rgba(249,115,22,.25)",
          lineHeight: 1, marginBottom: 12,
        }}>
          BLOCK FOUND!
        </div>

        <div style={{
          fontFamily: "var(--font-mono)", fontSize: 13,
          color: "var(--orange)", letterSpacing: ".14em",
          textTransform: "uppercase", marginBottom: 8,
          textShadow: "0 0 14px rgba(249,115,22,.55)",
        }}>
          BlockFinder Pool · Block #{blocksFound}
        </div>

        <div style={{ fontFamily: "var(--font-mono)", fontSize: 12, color: "var(--text2)", marginBottom: 32 }}>
          Reward sent to miner's wallet
        </div>

        <div style={{
          fontFamily: "var(--font-mono)", fontSize: 10, color: "var(--text3)",
          animation: "pulse-dot 2s ease-in-out infinite",
        }}>
          Click anywhere to close · auto-closes in 18s
        </div>

      </div>
    </div>
  );
}

// ─── App ──────────────────────────────────────────────────────────────────────

export default function App() {
  const [pool, setPool] = useState<PoolStats | null>(null);
  const [miners, setMiners] = useState<Miner[]>([]);
  const [blocks, setBlocks] = useState<BlockRow[]>([]);
  const [blockCandidates, setBlockCandidates] = useState<BlockCandidateRow[]>([]);
  const [publicBlocks, setPublicBlocks] = useState<PublicBlockRow[]>([]);
  const [hashrate, setHashrate] = useState<HashrateResponse | null>(null);
  const [network, setNetwork] = useState<NetworkInfo | null>(null);
  const [live, setLive] = useState(false);
  const [celebrating, setCelebrating] = useState(false);

  const prevBlocksFound    = useRef<number>(0);
  const prevSubmitAccepted = useRef<number>(0);
  // Set to true after the first successful /pool load so we don't
  // celebrate historical blocks that were already present on page open.
  const initializedRef = useRef<boolean>(false);
  // Guard against overlapping poll cycles: only one load may be in flight.
  const loadingRef = useRef<boolean>(false);

  const load = useCallback(async () => {
    // [Fix 2] Skip if a load cycle is already running.
    if (loadingRef.current) return;
    loadingRef.current = true;
    try {
      // [Fix 1] Fetch pool and miners in parallel; network info is now
      // embedded inside /pool (networkDifficulty, networkHashps) so no
      // separate /network call is needed — eliminates the duplicate
      // getmininginfo RPC.
      // [Fix 3] Use allSettled so a flaky /miners response does NOT flip
      // the dashboard OFFLINE; pool health drives the LIVE indicator.
      const [poolResult, minersResult, blocksResult, blockCandidatesResult, hashrateResult] = await Promise.allSettled([
        fetchPool(),
        fetchMiners(),
        fetchBlocks(),
        fetchBlockCandidates(),
        fetchHashrate(),
      ]);

      if (poolResult.status === "fulfilled") {
        const p = poolResult.value;
        setPool(p);
        // Derive NetworkInfo from the pool response (same data, no extra RPC).
        setNetwork({
          blocks:       p.blockHeight,
          difficulty:   p.networkDifficulty,
          networkhashps: p.networkHashps,
        });
        setLive(true);

        // [Fix 4] Celebrate on the FIRST real increment, including 0→1.
        // initializedRef prevents false celebrations on initial page load
        // when historical blocks already exist.
        const newBlocks   = p.blocksFound ?? 0;
        const newAccepted = p.submitblockAccepted ?? 0;
        if (initializedRef.current) {
          if (
            newBlocks   > prevBlocksFound.current ||
            newAccepted > prevSubmitAccepted.current
          ) {
            setCelebrating(true);
          }
        }
        // Record baseline on every successful fetch.
        initializedRef.current   = true;
        prevBlocksFound.current  = newBlocks;
        prevSubmitAccepted.current = newAccepted;
      } else {
        // [Fix 3] /pool itself failed → genuinely offline.
        setLive(false);
      }

      // [Fix 3] /miners failure keeps the previous miner list visible
      // rather than clearing it or marking the pool as offline.
      if (minersResult.status === "fulfilled") {
        setMiners(minersResult.value);
      }

      if (blocksResult.status === "fulfilled") {
        setBlocks(blocksResult.value);
      }

      if (blockCandidatesResult.status === "fulfilled") {
        setBlockCandidates(blockCandidatesResult.value);
      }

      if (hashrateResult.status === "fulfilled") {
        setHashrate(hashrateResult.value);
      }
    } finally {
      // [Fix 2] Always release the guard, even on errors.
      loadingRef.current = false;
    }
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, REFRESH_MS);
    return () => clearInterval(t);
  }, [load]);

  useEffect(() => {
    let alive = true;
    const loadPublic = async () => {
      const rows = await fetchPublicBlocks();
      if (alive) setPublicBlocks(rows);
    };
    loadPublic();
    const t = setInterval(loadPublic, PUBLIC_REFRESH_MS);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  return (
    <div className="bh-app">
      {celebrating && (
        <BlockFoundOverlay
          blocksFound={pool?.blocksFound ?? 0}
          onDismiss={() => setCelebrating(false)}
        />
      )}

      <Header live={live} />
      <NetworkBar pool={pool} network={network} />
      <div className="bh-top-grid">
        <BlocksTable blocks={blocks} />
        <BlockCandidatesTable candidates={blockCandidates} />
        <PublicBlocksTable blocks={publicBlocks} />
      </div>

      {/* Row 1: Bitcoin Core | Core Visual | Share Flow */}
      <div className="bh-section-title" id="bh-core">
        <span>Bitcoin Core &amp; Pool Engine</span>
        <div className="bh-section-line" />
      </div>
      <div className="bh-main-grid">
        <BitcoinCorePanel pool={pool} network={network} />
        <div className="bh-core-col">
          <CoreVisual pool={pool} />
        </div>
        <ShareFlowPanel pool={pool} miners={miners} />
      </div>

      {/* Row 2: Engine | Block Candidate | System Health */}
      <div className="bh-section-title" id="bh-stats">
        <span>Engine &amp; Block Status</span>
        <div className="bh-section-line" />
      </div>
      <div className="bh-row2">
        <EnginePanel pool={pool} />
        <BlockPanel pool={pool} miners={miners} network={network} />
        <SystemHealth pool={pool} miners={miners} />
      </div>

      {/* Mining Health */}
      <div className="bh-section-title" id="bh-health">
        <span>Mining Health</span>
        <div className="bh-section-line" />
      </div>
      <MiningHealthPanel pool={pool} miners={miners} hashrate={hashrate} />

      {/* Miner Fleet */}
      <div className="bh-section-title" id="bh-fleet">
        <span>Miner Fleet</span>
        <div className="bh-section-line" />
      </div>
      <MinerFleet miners={miners} network={network} />

      <Footer pool={pool} miners={miners} />
    </div>
  );
}
