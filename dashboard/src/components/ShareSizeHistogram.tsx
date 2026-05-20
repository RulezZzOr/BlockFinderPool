import React from "react";
import {
  BarChart,
  Bar,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
  CartesianGrid,
  Cell,
} from "recharts";

import { ShareSizeHistogramResponse, fmtDiff } from "../api";

type Props = {
  histogram: ShareSizeHistogramResponse | null;
};

const CustomTooltip = ({ active, payload }: any) => {
  if (!active || !payload?.length) return null;
  const bucket = payload[0]?.payload;
  if (!bucket) return null;

  const rangeLabel =
    bucket.lowerBoundDifficulty > 0
      ? `${fmtDiff(bucket.lowerBoundDifficulty)} – ${fmtDiff(bucket.upperBoundDifficulty)}`
      : `≤ ${fmtDiff(bucket.upperBoundDifficulty)}`;

  return (
    <div
      style={{
        background: "#0b1020",
        border: "1px solid #2a3560",
        borderRadius: 8,
        padding: "8px 12px",
        fontSize: 12,
      }}
    >
      <div style={{ color: "#94a3b8", marginBottom: 4 }}>{rangeLabel}</div>
      <div style={{ color: "#f97316", fontFamily: "JetBrains Mono, monospace", fontWeight: 600 }}>
        {Number(bucket.count).toLocaleString()} windows
      </div>
    </div>
  );
};

export default function ShareSizeHistogram({ histogram }: Props) {
  const buckets = histogram?.buckets ?? [];
  const maxCount = buckets.reduce((max, bucket) => Math.max(max, bucket.count), 0);
  const sampleCount = histogram?.sampleCount ?? 0;
  const topDifficulty = histogram?.maxDifficulty ?? 0;

  return (
    <div className="bh-card bh-card-orange bh-histogram-card bh-animate bh-animate-d4">
      <div className="bh-card-title">
        <span className="bh-card-title-dot" style={{ background: "var(--orange)" }} />
        Best Share Sizes
        <div className="bh-histogram-badges">
          <span className="bh-histogram-badge">
            {sampleCount.toLocaleString()} windows · {histogram?.days ?? 30}d
          </span>
          <span className="bh-histogram-badge bh-histogram-top-badge">
            <span className="bh-histogram-top-dot" />
            Top {fmtDiff(topDifficulty)}
          </span>
        </div>
      </div>

      <div className="bh-histogram-subtitle">
        Best accepted share from each Bitcoin block window. Buckets use 1-2-5 logarithmic ranges.
      </div>

      {buckets.length === 0 ? (
        <div className="bh-histogram-empty">No 30-day block-window history yet.</div>
      ) : (
        <ResponsiveContainer width="100%" height={250}>
          <BarChart data={buckets} margin={{ top: 8, right: 4, left: -16, bottom: 16 }}>
            <CartesianGrid strokeDasharray="3 3" stroke="rgba(30,42,64,.8)" vertical={false} />
            <XAxis
              dataKey="label"
              tick={{ fontSize: 10, fill: "#475569" }}
              axisLine={false}
              tickLine={false}
              minTickGap={10}
            />
            <YAxis
              allowDecimals={false}
              domain={[0, Math.max(4, maxCount)]}
              tick={{ fontSize: 10, fill: "#475569" }}
              axisLine={false}
              tickLine={false}
            />
            <Tooltip content={<CustomTooltip />} cursor={{ fill: "rgba(249,115,22,.06)" }} />
            <Bar dataKey="count" radius={[6, 6, 0, 0]} maxBarSize={22}>
              {buckets.map((bucket) => (
                <Cell
                  key={`${bucket.label}-${bucket.upperBoundDifficulty}`}
                  fill={
                    bucket.upperBoundDifficulty >= topDifficulty && topDifficulty > 0
                      ? "rgba(249,115,22,0.95)"
                      : "rgba(251,191,36,0.78)"
                  }
                />
              ))}
            </Bar>
          </BarChart>
        </ResponsiveContainer>
      )}
    </div>
  );
}
