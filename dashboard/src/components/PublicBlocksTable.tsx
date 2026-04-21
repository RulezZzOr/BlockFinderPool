import React from "react";
import { PublicBlockRow, timeAgo } from "../api";

type Props = { blocks: PublicBlockRow[] };

function poolLabel(pool: string | null): string {
  if (!pool) return "unknown";
  return pool;
}

export default function PublicBlocksTable({ blocks }: Props) {
  return (
    <div className="bh-card bh-block-feed bh-block-feed-public bh-animate bh-animate-d2">
      <div className="bh-block-feed-header">
        <div>
          <div className="bh-block-feed-title">Mempool Blocks</div>
          <div className="bh-block-feed-subtitle">
            Recent network blocks from mempool.space public API.
          </div>
        </div>
        <div className="bh-block-feed-count">public feed</div>
      </div>

      <div className="bh-block-feed-list">
        {blocks.length > 0 ? (
          blocks.map((b) => (
            <div key={`${b.height}-${b.hash}`} className="bh-block-feed-row bh-block-feed-row-public">
              <div className="bh-block-feed-main">
                <span className="bh-block-feed-height">#{b.height.toLocaleString()}</span>
                <span className="bh-block-feed-hash" title={b.hash}>
                  {b.hash.slice(0, 18)}…
                </span>
              </div>

              <div className="bh-block-feed-meta">
                <span className="bh-block-feed-foundby" title={b.pool ?? "unknown"}>
                  {poolLabel(b.pool)}
                </span>
                <span className="bh-block-feed-age" title={b.timestamp}>
                  {timeAgo(b.timestamp)}
                </span>
              </div>
            </div>
          ))
        ) : (
          <div className="bh-block-feed-empty">Public feed unavailable</div>
        )}
      </div>
    </div>
  );
}
