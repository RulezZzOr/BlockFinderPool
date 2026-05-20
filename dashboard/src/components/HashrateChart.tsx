import React from "react";
import {
  BarChart,
  Bar,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
  CartesianGrid,
} from "recharts";

type Point = { timestamp: string; shares: number };

type Props = { data: Point[] };

const CustomTooltip = ({ active, payload, label }: any) => {
  if (active && payload?.length) {
    const timestamp = new Date(label);
    const timeLabel = Number.isNaN(timestamp.getTime())
      ? String(label)
      : timestamp.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    return (
      <div style={{
        background: "#0b1020",
        border: "1px solid #2a3560",
        borderRadius: 8, padding: "8px 12px", fontSize: 12,
      }}>
        <div style={{ color: "#94a3b8", marginBottom: 4 }}>{timeLabel}</div>
        <div style={{ color: "#22d3ee", fontFamily: "JetBrains Mono, monospace", fontWeight: 600 }}>
          {Number(payload[0].value).toLocaleString()} accepted shares
        </div>
      </div>
    );
  }
  return null;
};

function floorToMinute(ts: Date): Date {
  const out = new Date(ts);
  out.setSeconds(0, 0);
  return out;
}

function minuteKey(ts: Date): string {
  return floorToMinute(ts).toISOString();
}

function normalizeSeries(data: Point[], minutes: number): Point[] {
  const bucketed = new Map<string, number>();
  for (const point of data) {
    const parsed = new Date(point.timestamp);
    if (Number.isNaN(parsed.getTime())) continue;
    const key = minuteKey(parsed);
    bucketed.set(key, (bucketed.get(key) ?? 0) + (point.shares ?? 0));
  }

  const now = floorToMinute(new Date());
  const normalized: Point[] = [];
  for (let i = minutes - 1; i >= 0; i -= 1) {
    const bucketTime = new Date(now.getTime() - i * 60_000);
    const key = bucketTime.toISOString();
    normalized.push({
      timestamp: key,
      shares: bucketed.get(key) ?? 0,
    });
  }
  return normalized;
}

function formatTick(value: string): string {
  const ts = new Date(value);
  if (Number.isNaN(ts.getTime())) return String(value);
  return ts.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export default function HashrateChart({ data }: Props) {
  const series = normalizeSeries(data, 30);
  const total = series.reduce((sum, point) => sum + point.shares, 0);

  return (
    <div className="bh-card bh-card-blue bh-histogram-card bh-animate bh-animate-d4">
      <div className="bh-card-title">
        <span className="bh-card-title-dot" style={{ background: "var(--cyan)" }} />
        Share Histogram
        <span className="bh-histogram-badge">
          {total.toLocaleString()} shares · 30m
        </span>
      </div>

      <div className="bh-histogram-subtitle">
        Accepted shares per minute. Buckets are zero-filled so quiet minutes stay visible.
      </div>

      <ResponsiveContainer width="100%" height={250}>
        <BarChart data={series} margin={{ top: 8, right: 4, left: -16, bottom: 0 }}>
          <defs>
            <linearGradient id="shareHistGrad" x1="0" y1="0" x2="0" y2="1">
              <stop offset="5%" stopColor="#22d3ee" stopOpacity={0.95} />
              <stop offset="95%" stopColor="#60a5fa" stopOpacity={0.65} />
            </linearGradient>
          </defs>
          <CartesianGrid strokeDasharray="3 3" stroke="rgba(30,42,64,.8)" vertical={false} />
          <XAxis
            dataKey="timestamp"
            tick={{ fontSize: 10, fill: "#475569" }}
            axisLine={false} tickLine={false}
            tickFormatter={formatTick}
            interval="preserveStartEnd"
            minTickGap={14}
          />
          <YAxis
            allowDecimals={false}
            tick={{ fontSize: 10, fill: "#475569" }}
            axisLine={false} tickLine={false}
          />
          <Tooltip
            content={<CustomTooltip />}
            cursor={{ fill: "rgba(34,211,238,.05)" }}
          />
          <Bar
            dataKey="shares"
            fill="url(#shareHistGrad)"
            radius={[6, 6, 0, 0]}
            maxBarSize={18}
          />
        </BarChart>
      </ResponsiveContainer>
    </div>
  );
}
