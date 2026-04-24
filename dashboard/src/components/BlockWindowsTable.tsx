import React from "react";
import { BlockWindowRow, PublicBlockRow, fmtBlockInterval, fmtDiff, fmtHr, shortAddress, timeAgo } from "../api";

type Props = {
  windows: BlockWindowRow[];
  publicBlocks: PublicBlockRow[];
  compact: boolean;
  onToggleCompact: () => void;
};

function blockLabel(window: BlockWindowRow): string {
  return window.inProgress
    ? `Mining... #${window.height.toLocaleString()}`
    : `Block #${window.height.toLocaleString()}`;
}

function blockMeta(window: BlockWindowRow): string {
  if (window.inProgress) {
    return `Live ${timeAgo(window.startedAt)}`;
  }
  if (window.durationSecs != null) {
    return fmtBlockInterval(window.durationSecs);
  }
  if (window.endedAt) {
    return timeAgo(window.endedAt);
  }
  return "—";
}

function maskWorkerLabel(worker: string | null): string {
  if (!worker) return "—";
  const dot = worker.indexOf(".");
  if (dot < 0) {
    return worker.length <= 3 ? `${worker}*` : `${worker.slice(0, 3)}*`;
  }

  const prefix = worker.slice(0, Math.min(3, dot));
  const suffix = worker.slice(dot + 1);
  return `${prefix}*.${suffix}`;
}

export default function BlockWindowsTable({ windows, publicBlocks, compact, onToggleCompact }: Props) {
  const visible = windows.slice(0, compact ? 5 : 10);
  const publicByHeight = new Map(publicBlocks.map((block) => [block.height, block]));

  return (
    <div className="bh-card bh-block-feed bh-block-feed-windows bh-animate bh-animate-d3">
      <div className="bh-block-feed-header">
        <div>
          <div className="bh-block-feed-title">Recent Block Windows</div>
          <div className="bh-block-feed-subtitle">
            Best BlockFinder share seen during each Bitcoin network block interval. Not found blocks unless the share reaches network difficulty.
          </div>
        </div>
        <button
          className="bh-block-window-toggle"
          onClick={onToggleCompact}
          type="button"
          title={compact ? "Show last 10 windows" : "Show last 5 windows"}
        >
          {compact ? "5 compact" : "10 latest"}
        </button>
      </div>

      {visible.length > 0 ? (
        <div className="bh-block-window-grid">
          {visible.map((window) => {
            const external = window.externalPool ?? publicByHeight.get(window.height)?.pool ?? null;
            const payout = window.bestPayoutAddress ?? window.bestSubmittedPayoutAddress ?? null;
            const bestPct = window.networkDifficulty > 0
              ? (window.bestSubmittedDifficulty / window.networkDifficulty) * 100
              : 0;
            return (
              <div key={window.id} className={`bh-block-window-card ${window.inProgress ? "bh-block-window-live" : ""}`}>
                <div className="bh-block-window-top">
                  <div className="bh-block-window-height">{blockLabel(window)}</div>
                  <div className="bh-block-window-age">{blockMeta(window)}</div>
                </div>

                <div className="bh-block-window-subtitle">
                  {window.prevhash.slice(0, 18)}…{window.prevhash.slice(-8)}
                </div>

                <div className="bh-block-window-stats">
                  <div className="bh-block-window-stat">
                    <span>BlockFinder Best Share</span>
                    <strong style={{ color: "var(--orange)" }}>{fmtDiff(window.bestSubmittedDifficulty)}</strong>
                  </div>
                  <div className="bh-block-window-stat">
                    <span>Best Worker</span>
                    <strong title={window.bestWorker ?? window.bestSubmittedWorker ?? "—"}>
                      {maskWorkerLabel(window.bestWorker ?? window.bestSubmittedWorker ?? null)}
                    </strong>
                  </div>
                  <div className="bh-block-window-stat">
                    <span>Payout</span>
                    <strong title={payout ?? "—"}>{payout ? shortAddress(payout) : "—"}</strong>
                  </div>
                  <div className="bh-block-window-stat">
                    <span>Proximity</span>
                    <strong style={{ color: bestPct > 1 ? "var(--orange)" : "var(--text)" }}>
                      {bestPct > 0 ? `${bestPct.toFixed(6)}%` : "—"}
                    </strong>
                  </div>
                  <div className="bh-block-window-stat">
                    <span>Tx / Fee</span>
                    <strong>
                      {window.txCount > 0
                        ? `${window.txCount.toLocaleString()} tx • ${window.feeRateSatVb != null ? `${window.feeRateSatVb.toFixed(2)} sat/vB` : "—"}`
                        : "—"}
                    </strong>
                  </div>
                  <div className="bh-block-window-stat">
                    <span>Pool / Hashrate</span>
                    <strong>
                      {external ?? "unknown"}
                      {window.avgPoolHashrate != null ? ` • ${fmtHr(window.avgPoolHashrate)}` : ""}
                    </strong>
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      ) : (
        <div className="bh-block-feed-empty">No block windows yet</div>
      )}
    </div>
  );
}
