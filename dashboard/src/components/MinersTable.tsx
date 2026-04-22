import React from "react";
import { Miner } from "../api";

type Props = { miners: Miner[] };

/** Auto-scale hashrate through KH/s → MH/s → GH/s → TH/s → PH/s → EH/s */
function fmtHr(gh: number): string {
  const hs = gh * 1e9; // convert GH/s → H/s
  if (hs >= 1e18) return `${(hs / 1e18).toFixed(3)} EH/s`;
  if (hs >= 1e15) return `${(hs / 1e15).toFixed(3)} PH/s`;
  if (hs >= 1e12) return `${(hs / 1e12).toFixed(2)} TH/s`;
  if (hs >= 1e9)  return `${(hs / 1e9).toFixed(2)} GH/s`;
  if (hs >= 1e6)  return `${(hs / 1e6).toFixed(2)} MH/s`;
  if (hs >= 1e3)  return `${(hs / 1e3).toFixed(2)} KH/s`;
  return `${hs.toFixed(0)} H/s`;
}

/** Format difficulty with K/M suffix */
function fmtDiff(d: number): string {
  if (d >= 1_000_000) return `${(d / 1_000_000).toFixed(2)}M`;
  if (d >= 1_000)     return `${(d / 1_000).toFixed(1)}K`;
  return d.toFixed(0);
}

/** K → M → G → T → P → E (same SI scale as hashrate) */
function fmtBest(d: number): string {
  if (d <= 0)    return "—";
  if (d >= 1e18) return `${(d / 1e18).toFixed(3)} E`;
  if (d >= 1e15) return `${(d / 1e15).toFixed(3)} P`;
  if (d >= 1e12) return `${(d / 1e12).toFixed(3)} T`;
  if (d >= 1e9)  return `${(d / 1e9).toFixed(3)} G`;
  if (d >= 1e6)  return `${(d / 1e6).toFixed(2)} M`;
  if (d >= 1e3)  return `${(d / 1e3).toFixed(1)} K`;
  return d.toFixed(0);
}

/** Extract just the worker name (after last dot) */
function workerName(full: string): string {
  const dot = full.lastIndexOf(".");
  return dot >= 0 ? full.slice(dot + 1) : full;
}

/** Relative time from ISO string */
function relTime(iso: string | null): string {
  if (!iso) return "—";
  const sec = Math.floor((Date.now() - new Date(iso).getTime()) / 1000);
  if (sec < 60)  return `${sec}s ago`;
  if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
  return `${Math.floor(sec / 3600)}h ago`;
}

export default function MinersTable({ miners }: Props) {
  if (!miners.length) {
    return (
      <div className="card table-card">
        <div className="table-card-header"><div className="card-label">Active Miners</div></div>
        <div className="empty">No miners connected</div>
      </div>
    );
  }

  const globalBest = Math.max(...miners.map((m) => m.best_submitted_difficulty ?? 0));

  return (
    <div className="card table-card">
      <div className="table-card-header">
        <div className="card-label">Active Miners — {miners.length} connected</div>
      </div>
      <div style={{ overflowX: "auto" }}>
        <table>
          <thead>
            <tr>
              <th>Worker</th>
              <th>Hashrate</th>
              <th>Difficulty</th>
              <th title="Highest difficulty hash submitted by the miner, including stale/rejected shares if they were parsed successfully.">Raw Best Submitted ★</th>
              <th title="Highest share accepted by the configured pool difficulty rules.">Accepted Best</th>
              <th title="Best share since the current prevhash/template.">Current Block Best</th>
              <th>Shares</th>
              <th>Stale</th>
              <th>Rejected</th>
              <th title="Time from last notify → share submit (hashing time, NOT network RTT)">Hash Cycle</th>
              <th title="Pool processing time per submit (should be 1–5 ms)">Pool RTT</th>
              <th>Last Share</th>
            </tr>
          </thead>
          <tbody>
            {miners.map((m) => {
              const best = m.best_submitted_difficulty ?? 0;
              const isTopBest = best > 0 && best === globalBest;
              const bestAccepted = m.best_accepted_difficulty ?? 0;
              const bestBlock = m.current_block_best_submitted_difficulty ?? 0;
              const stale = m.stale ?? 0;
              const hashCycle = m.notify_to_submit_ms ?? 0;
              const rtt       = m.submit_rtt_ms ?? 0;
              // Hash cycle color: relative to session difficulty — just show dim always
              // (value is dominated by hashing time, not network, so no alert needed)
              const rttColor = rtt < 10 ? "num-green" : rtt < 50 ? "num-dim" : "num-orange";

              // Best Diff color by magnitude:
              // ★ gold  = overall pool best
              // P (≥1e15) = light purple
              // T (≥1e12) = light red
              // G (≥1e9)  = light green
              // default   = white/dim
              const bestColor = isTopBest
                ? "num-yellow"
                : best >= 1e15 ? "best-p"
                : best >= 1e12 ? "best-t"
                : best >= 1e9  ? "best-g"
                : "num-dim";

              return (
                <tr key={m.worker}>
                  {/* Worker */}
                  <td>
                    <span className="worker-name">{workerName(m.worker)}</span>
                  </td>

                  {/* Hashrate */}
                  <td className="num-cyan">{fmtHr(m.hashrate_gh)}</td>

                  {/* Difficulty */}
                  <td className="num-dim">{fmtDiff(m.difficulty)}</td>

                  {/* Best Diff */}
                  <td>
                    <span className={bestColor}>
                      {fmtBest(best)}
                    </span>
                    {isTopBest && <span className="best-star">★</span>}
                  </td>
                  <td className="num-green">{fmtBest(bestAccepted)}</td>
                  <td className="num-cyan">{fmtBest(bestBlock)}</td>

                  {/* Shares */}
                  <td className="num-green">{m.shares.toLocaleString()}</td>

                  {/* Stale */}
                  <td className={stale > 0 ? "num-orange" : "num-dim"}>{stale}</td>

                  {/* Rejected */}
                  <td className={m.rejected > 0 ? "num-red" : "num-dim"}>{m.rejected}</td>

                  {/* Hash Cycle: notify→submit delay (hashing time) */}
                  <td className="num-dim">
                    {hashCycle >= 60000
                      ? `${(hashCycle / 60000).toFixed(1)}m`
                      : hashCycle >= 1000
                      ? `${(hashCycle / 1000).toFixed(1)}s`
                      : `${hashCycle.toFixed(0)}ms`}
                  </td>
                  {/* Pool RTT: actual server processing time */}
                  <td className={rttColor}>{rtt < 1 ? "<1" : rtt.toFixed(0)} ms</td>

                  {/* Last Share */}
                  <td className="num-dim" title={`${m.last_share_status ?? "unknown"} @ ${fmtBest(m.last_share_difficulty ?? 0)}`}>
                    {relTime(m.last_share_at ?? m.last_share_time ?? m.last_seen)}
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
