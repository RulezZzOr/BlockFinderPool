import React from "react";
import { BlockRow, shortWorker, timeAgo } from "../api";

type Props = { blocks: BlockRow[] };

function statusLabel(status: string): string {
  switch (status) {
    case "submitted":
      return "submitted";
    case "submit_failed":
      return "submit failed";
    default:
      return status.replace(/_/g, " ");
  }
}

function statusTone(status: string): { color: string; bg: string; border: string } {
  switch (status) {
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

function foundByLabel(foundBy: string | null): string {
  if (!foundBy) return "unknown";
  return shortWorker(foundBy);
}

export default function BlocksTable({ blocks }: Props) {
  return (
    <div className="bh-card bh-block-feed bh-animate bh-animate-d1">
      <div className="bh-block-feed-header">
        <div>
          <div className="bh-block-feed-title">Recent Blocks</div>
          <div className="bh-block-feed-subtitle">
            Latest block submissions and who found them.
          </div>
        </div>
        <div className="bh-block-feed-count">{blocks.length} records</div>
      </div>

      <div className="bh-block-feed-list">
        {blocks.length > 0 ? (
          blocks.map((b) => {
            const tone = statusTone(b.status);
            return (
              <div key={`${b.height}-${b.hash}-${b.created_at}`} className="bh-block-feed-row">
                <div className="bh-block-feed-main">
                  <span className="bh-block-feed-height">#{b.height.toLocaleString()}</span>
                  <span className="bh-block-feed-hash" title={b.hash}>
                    {b.hash.slice(0, 18)}…
                  </span>
                </div>

                <div className="bh-block-feed-meta">
                  <span className="bh-block-feed-foundby" title={b.found_by ?? "unknown"}>
                    by {foundByLabel(b.found_by)}
                  </span>
                  <span className="bh-block-feed-age" title={b.created_at}>
                    {timeAgo(b.created_at)}
                  </span>
                  <span
                    className="bh-block-feed-status"
                    style={{
                      color: tone.color,
                      background: tone.bg,
                      borderColor: tone.border,
                    }}
                  >
                    {statusLabel(b.status)}
                  </span>
                </div>
              </div>
            );
          })
        ) : (
          <div className="bh-block-feed-empty">No blocks submitted yet</div>
        )}
      </div>
    </div>
  );
}
