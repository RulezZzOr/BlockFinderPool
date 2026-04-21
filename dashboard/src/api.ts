// ─── API types ────────────────────────────────────────────────────────────────

export type PoolStats = {
  totalHashRate: number;
  totalMiners: number;
  blockHeight: number;
  blocksFound: number;
  fee: number;
  poolStartedAt: string;
  uptimeSecs: number;
  jobsSent: number;
  cleanJobsSent: number;
  jobsSentPerMiner: number;
  jobsSentPerMinerPerMin: number;
  notifyDeduped: number;
  notifyRateLimited: number;
  duplicateShares: number;
  reconnectsTotal: number;
  submitblockAccepted: number;
  submitblockRejected: number;
  submitblockRpcFail: number;
  versionRollingViolations: number;
  stalesNewBlock: number;
  stalesExpired: number;
  stalesReconnect: number;
  zmqBlocksDetected: number;
  zmqBlockNotifications: number;
  zmqTxTriggered: number;
  zmqTxDebounced: number;
  zmqTxPostBlockSuppressed: number;
  staleRatio: number;
  // Network info embedded in /pool to avoid a second getmininginfo RPC call.
  networkDifficulty: number;
  networkHashps: number;
};

export type Miner = {
  worker: string;
  difficulty: number;
  best_difficulty: number;
  best_submitted_difficulty: number;
  shares: number;
  rejected: number;
  stale: number;
  hashrate_gh: number;
  last_seen: string;
  last_share_time: string | null;
  notify_to_submit_ms: number;
  submit_rtt_ms: number;
  user_agent: string | null;
  session_id: string | null;
};

export type Metrics = {
  total_hashrate_gh: number;
  total_shares: number;
  total_rejected: number;
  total_blocks: number;
  updated_at: string;
};

export type HashrateResponse = {
  total_hashrate_gh: number;
  updated_at: string;
  recent: { timestamp: string; shares: number }[];
};

export type BlockRow = {
  height: number;
  hash: string;
  found_by: string | null;
  status: string;
  created_at: string;
};

export type PublicBlockRow = {
  height: number;
  hash: string;
  timestamp: string;
  pool: string | null;
};

export type NetworkInfo = {
  blocks: number;
  difficulty: number;
  networkhashps: number;
};

export type TemplateInfo = {
  height: number;
  coinbasevalue: number;   // satoshis (subsidy + fees)
  transactions: number;
  bits: string;
  target: string;
  job_id: string;
  created_at: string;
};

// ─── URL resolution ───────────────────────────────────────────────────────────

const configuredBase = (import.meta.env.VITE_API_BASE ?? "").trim();
const configuredBases = (import.meta.env.VITE_API_BASES ?? "").trim();
const configuredMempoolBase = (import.meta.env.VITE_MEMPOOL_API_BASE ?? "https://mempool.space/api").trim();

const apiUrl = (base: string, path: string) => `${base.replace(/\/$/, "")}${path}`;

async function fetchJson<T>(url: string): Promise<T> {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json() as Promise<T>;
}

function mempoolUrl(path: string): string {
  return `${configuredMempoolBase.replace(/\/$/, "")}${path}`;
}

function getBases(): string[] {
  if (configuredBases)
    return configuredBases.split(",").map((s: string) => s.trim()).filter(Boolean);
  if (configuredBase) return [configuredBase];
  return ["/api"];
}

function getDirectFallbackBases(): string[] {
  const hostname = typeof window !== "undefined" ? window.location.hostname : "localhost";
  return [`http://${hostname}:8080`, `http://${hostname}:8081`];
}

async function apiGet<T>(path: string): Promise<T> {
  const bases = getBases();
  for (const base of bases) {
    try {
      return await fetchJson<T>(apiUrl(base, path));
    } catch {}
  }
  for (const base of getDirectFallbackBases()) {
    try {
      return await fetchJson<T>(apiUrl(base, path));
    } catch {}
  }
  throw new Error(`API unavailable for ${path}`);
}

async function apiGetAll<T>(path: string): Promise<T[]> {
  const bases = getBases();
  const results = await Promise.allSettled(
    bases.map((b) => fetchJson<T>(apiUrl(b, path)))
  );
  const ok = results
    .filter((r): r is PromiseFulfilledResult<T> => r.status === "fulfilled")
    .map((r) => r.value);
  if (ok.length > 0) return ok;
  for (const base of getDirectFallbackBases()) {
    try {
      return [await fetchJson<T>(apiUrl(base, path))];
    } catch {}
  }
  throw new Error(`API unavailable for ${path}`);
}

// ─── API fetch functions ───────────────────────────────────────────────────────

export const fetchPool = (): Promise<PoolStats> => apiGet<PoolStats>("/pool");

export const fetchTemplateInfo = async (): Promise<TemplateInfo | null> => {
  try {
    return await apiGet<TemplateInfo>("/blockfinder/template-info");
  } catch {
    return null;
  }
};

/** Bitcoin block subsidy in satoshis for a given height. */
export function blockSubsidy(height: number): number {
  const halvings = Math.floor(height / 210_000);
  if (halvings >= 64) return 0;
  return Math.floor(50 * 1e8 / Math.pow(2, halvings));
}

/** Format satoshis → "X.XXXX BTC" */
export function fmtBtc(sat: number): string {
  return `${(sat / 1e8).toFixed(4)} BTC`;
}

export const fetchMiners = (): Promise<Miner[]> => apiGet<Miner[]>("/miners");

export const fetchNetwork = async (): Promise<NetworkInfo | null> => {
  try {
    return await apiGet<NetworkInfo>("/network");
  } catch {
    return null;
  }
};

export const fetchBlocks = async (): Promise<BlockRow[]> => {
  try {
    const parts = await apiGetAll<BlockRow[]>("/blocks");
    const uniq = new Map<string, BlockRow>();
    for (const b of ([] as BlockRow[]).concat(...parts))
      uniq.set(`${b.height}:${b.hash}`, b);
    return Array.from(uniq.values()).sort((a, b) => b.height - a.height);
  } catch {
    return [];
  }
};

type MempoolBlock = {
  id?: string;
  hash?: string;
  height?: number;
  timestamp?: number;
  extras?: {
    pool?: { name?: string; slug?: string };
    pool_name?: string;
    poolName?: string;
    miner?: string;
  };
};

export const fetchPublicBlocks = async (): Promise<PublicBlockRow[]> => {
  try {
    try {
      return await apiGet<PublicBlockRow[]>("/public-blocks");
    } catch {}

    const tipHeight = Number(await fetchJson<unknown>(mempoolUrl("/blocks/tip/height")));
    const blocks = await fetchJson<MempoolBlock[]>(mempoolUrl(`/blocks/${tipHeight}`));
    return (blocks || []).slice(0, 10).map((block) => {
      const hash = block.id ?? block.hash ?? "";
      const pool =
        block.extras?.pool?.name ??
        block.extras?.pool?.slug ??
        block.extras?.pool_name ??
        block.extras?.poolName ??
        block.extras?.miner ??
        null;

      return {
        height: Number(block.height ?? 0),
        hash,
        timestamp: new Date((block.timestamp ?? 0) * 1000).toISOString(),
        pool,
      };
    }).filter((b) => b.height > 0 && b.hash.length > 0);
  } catch {
    return [];
  }
};

// Legacy metrics for backward compat
export const fetchMetrics = async (): Promise<Metrics> => {
  try {
    const all = await apiGetAll<Metrics>("/metrics");
    return {
      total_hashrate_gh: all.reduce((s, m) => s + (m.total_hashrate_gh || 0), 0),
      total_shares: all.reduce((s, m) => s + (m.total_shares || 0), 0),
      total_rejected: all.reduce((s, m) => s + (m.total_rejected || 0), 0),
      total_blocks: all.reduce((s, m) => s + (m.total_blocks || 0), 0),
      updated_at: all.map((m) => m.updated_at).sort().slice(-1)[0] ?? "",
    };
  } catch {
    return { total_hashrate_gh: 0, total_shares: 0, total_rejected: 0, total_blocks: 0, updated_at: "" };
  }
};

// ─── Formatters ───────────────────────────────────────────────────────────────

export function fmtHr(gh: number): string {
  const hs = gh * 1e9;
  if (hs >= 1e21) return `${(hs / 1e21).toFixed(2)} ZH/s`;
  if (hs >= 1e18) return `${(hs / 1e18).toFixed(2)} EH/s`;
  if (hs >= 1e15) return `${(hs / 1e15).toFixed(2)} PH/s`;
  if (hs >= 1e12) return `${(hs / 1e12).toFixed(2)} TH/s`;
  if (hs >= 1e9)  return `${(hs / 1e9).toFixed(2)} GH/s`;
  if (hs >= 1e6)  return `${(hs / 1e6).toFixed(2)} MH/s`;
  if (hs >= 1e3)  return `${(hs / 1e3).toFixed(2)} KH/s`;
  return `${hs.toFixed(0)} H/s`;
}

export function fmtDiff(d: number): string {
  if (d <= 0)     return "—";
  if (d >= 1e18)  return `${(d / 1e18).toFixed(2)} E`;
  if (d >= 1e15)  return `${(d / 1e15).toFixed(2)} P`;
  if (d >= 1e12)  return `${(d / 1e12).toFixed(2)} T`;
  if (d >= 1e9)   return `${(d / 1e9).toFixed(2)} G`;
  if (d >= 1e6)   return `${(d / 1e6).toFixed(2)} M`;
  if (d >= 1e3)   return `${(d / 1e3).toFixed(1)} K`;
  return d.toFixed(0);
}

export function fmtNetHash(h: number): string {
  if (h >= 1e21) return `${(h / 1e21).toFixed(2)} ZH/s`;
  if (h >= 1e18) return `${(h / 1e18).toFixed(2)} EH/s`;
  if (h >= 1e15) return `${(h / 1e15).toFixed(2)} PH/s`;
  if (h >= 1e12) return `${(h / 1e12).toFixed(2)} TH/s`;
  return `${(h / 1e9).toFixed(2)} GH/s`;
}

export function fmtUptime(s: number): string {
  if (s < 60)   return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (h < 24)   return `${h}h ${m}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}

/** Format an expected block interval in the most human-readable unit.
 *  Optimised for solo-mining timescales that are typically multi-year. */
export function fmtBlockInterval(s: number): string {
  if (s <= 0) return "—";
  const YEAR  = 365.25 * 86400;
  const MONTH = YEAR / 12;
  const DAY   = 86400;
  if (s < 60)    return `${s.toFixed(0)}s`;
  if (s < 3600)  return `${Math.floor(s / 60)}m ${Math.floor(s % 60)}s`;
  if (s < DAY)   { const h = Math.floor(s / 3600); const m = Math.floor((s % 3600) / 60); return `${h}h ${m}m`; }
  if (s < YEAR)  { const d = Math.floor(s / DAY);  const h = Math.floor((s % DAY) / 3600); return `${d}d ${h}h`; }
  const years = s / YEAR;
  if (years < 2)    { const y = Math.floor(years); const mo = Math.round((s - y * YEAR) / MONTH); return `${y}y ${mo}mo`; }
  if (years < 100)  return `${years.toFixed(1)} yrs`;
  if (years < 1000) return `${years.toFixed(0)} yrs`;
  return `${(years / 1000).toFixed(1)}K yrs`;
}

export function timeAgo(iso: string): string {
  const diff = (Date.now() - new Date(iso).getTime()) / 1000;
  if (diff < 5)   return "just now";
  if (diff < 60)  return `${Math.floor(diff)}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  return `${Math.floor(diff / 3600)}h ago`;
}

export function shortWorker(worker: string): string {
  const parts = worker.split(".");
  return parts[parts.length - 1] || worker;
}

export function shortAddress(addr: string): string {
  if (addr.length <= 16) return addr;
  return `${addr.slice(0, 8)}…${addr.slice(-6)}`;
}

export function getFirmwareLabel(ua: string | null): string {
  if (!ua) return "Unknown";
  if (ua.includes("bitaxe"))    return "Bitaxe";
  if (ua.includes("NerdQAxe"))  return "NerdQAxe";
  if (ua.includes("cgminer"))   return "cgminer";
  if (ua.includes("bmminer"))   return "bmminer";
  return ua.split("/")[0] ?? ua;
}
