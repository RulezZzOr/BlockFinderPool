import React from "react";
import { PublicBlockRow, timeAgo } from "../api";

type Props = { blocks: PublicBlockRow[] };

function poolLabel(pool: string | null): string {
  if (!pool) return "unknown";
  return pool;
}

export default function PublicBlocksTable({ blocks }: Props) {
  const latest = blocks.slice(0, 3);

  return (
    <div className="bh-card bh-block-feed bh-block-feed-public bh-animate bh-animate-d2">
      <div className="bh-block-feed-header">
        <div>
          <div className="bh-block-feed-title">Mempool Blocks</div>
          <div className="bh-block-feed-subtitle">
            Last 3 network blocks from mempool.space.
          </div>
        </div>
        <div className="bh-block-feed-count">3 latest</div>
      </div>

      {latest.length > 0 ? (
        <div className="bh-public-blocks-grid">
          {latest.map((b) => (
            <div key={`${b.height}-${b.hash}`} className="bh-public-block-card">
              <div className="bh-public-block-top">
                <span className="bh-block-feed-height">#{b.height.toLocaleString()}</span>
                <span className="bh-public-block-age" title={b.timestamp}>
                  {timeAgo(b.timestamp)}
                </span>
              </div>

              <div className="bh-public-block-pool" title={b.pool ?? "unknown"}>
                {poolLabel(b.pool)}
              </div>

              <div className="bh-public-block-hash" title={b.hash}>
                {b.hash.slice(0, 18)}…{b.hash.slice(-8)}
              </div>
            </div>
          ))}
        </div>
      ) : (
        <div className="bh-block-feed-empty">Public feed unavailable</div>
      )}
    </div>
  );
}
