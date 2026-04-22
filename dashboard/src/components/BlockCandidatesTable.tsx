import React from "react";
import { BlockCandidateRow, fmtDiff, timeAgo } from "../api";

type Props = { candidates: BlockCandidateRow[] };

function resultLabel(result: string): string {
  switch (result) {
    case "submitted":
      return "submitted";
    case "submit_failed":
      return "submit failed";
    default:
      return result.replace(/_/g, " ");
  }
}

function resultTone(result: string): { color: string; bg: string; border: string } {
  switch (result) {
    case "submitted":
      return {
        color: "var(--green)",
        bg: "rgba(74,222,128,.08)",
        border: "rgba(74,222,128,.20)",
      };
    case "submit_failed":
      return {
        color: "var(--red)",
        bg: "rgba(248,113,113,.08)",
        border: "rgba(248,113,113,.20)",
      };
    default:
      return {
        color: "var(--yellow)",
        bg: "rgba(251,191,36,.08)",
        border: "rgba(251,191,36,.18)",
      };
  }
}

export default function BlockCandidatesTable({ candidates }: Props) {
  return (
    <div className="bh-card bh-candidate-feed bh-animate bh-animate-d1">
      <div className="bh-block-feed-header">
        <div>
          <div className="bh-block-feed-title">Block Candidates</div>
          <div className="bh-block-feed-subtitle">
            Recent block candidate submissions and submitblock results.
          </div>
        </div>
        <div className="bh-block-feed-count">{candidates.length} records</div>
      </div>

      {candidates.length > 0 ? (
        <div className="bh-candidate-table-wrap">
          <table className="bh-candidate-table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Worker</th>
                <th>Height</th>
                <th>Difficulty</th>
                <th>Result</th>
                <th>RPC ms</th>
                <th>Error</th>
              </tr>
            </thead>
            <tbody>
              {candidates.map((c) => {
                const tone = resultTone(c.submitblock_result);
                return (
                  <tr key={`${c.timestamp}-${c.worker}-${c.block_hash}`}>
                    <td title={c.timestamp}>{timeAgo(c.timestamp)}</td>
                    <td title={c.worker}>{c.worker}</td>
                    <td>#{c.height.toLocaleString()}</td>
                    <td title={`submitted ${fmtDiff(c.submitted_difficulty)} / network ${fmtDiff(c.network_difficulty)}`}>
                      {fmtDiff(c.submitted_difficulty)}
                    </td>
                    <td>
                      <span
                        className="bh-candidate-result"
                        style={{
                          color: tone.color,
                          background: tone.bg,
                          borderColor: tone.border,
                        }}
                      >
                        {resultLabel(c.submitblock_result)}
                      </span>
                    </td>
                    <td>{c.submitblock_rpc_latency_ms > 0 ? `${c.submitblock_rpc_latency_ms}ms` : "—"}</td>
                    <td className="bh-candidate-error" title={c.rpc_error ?? ""}>
                      {c.rpc_error ?? "—"}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : (
        <div className="bh-block-feed-empty">No block candidates yet</div>
      )}
    </div>
  );
}
