# BlockFinder

A self-hosted Bitcoin solo hunter node for small LAN fleets.
Zero fee — 100% of every block reward goes directly to the miner's own address.

License: non-commercial use only. See `LICENSE` and `NOTICE.md` for provenance.

---

## Architecture

| Component | Technology | Port |
|---|---|---|
| Stratum server | Rust + Tokio | 3333 |
| REST API | Axum | 8080 (standalone) / 8081 (bridge) |
| Dashboard | React + Nginx | 3334 (host) |
| Bitcoin node | Bitcoin Core via RPC + ZMQ | (external) |
| Database | SQLite | (Docker volume) |

---

## Quick Start — One Command

### Automatic setup

```bash
git clone https://github.com/RulezZzOr/BlockFinderPool.git
cd BlockFinderPool
bash setup-blackhole.sh
```

The installer **auto-detects** your Bitcoin Core container, reads RPC credentials
and ZMQ settings, writes `env/.env`, builds, and starts everything.

### Standard Linux / macOS (any Bitcoin Core node)

```bash
git clone https://github.com/RulezZzOr/BlockFinderPool.git
cd BlockFinderPool
bash setup-blackhole.sh
```

If Bitcoin Core is running locally (native or Docker) the installer finds it
automatically. For a remote node it will ask for the IP and credentials.

### Non-interactive / CI

```bash
bash setup-blackhole.sh --unattended
# Then edit env/.env and run: bash run.sh
```

---

## What the installer does

| Step | Action |
|------|--------|
| 1 | Checks Docker + Docker Compose |
| 2 | Detects local/standalone deployment mode |
| 3 | Finds Bitcoin Core (Docker container or native) |
| 4 | Reads RPC credentials from bitcoin.conf or node config |
| 5 | Tests RPC connectivity |
| 6 | Optionally adds ZMQ lines to bitcoin.conf and restarts Core |
| 7 | Asks for your payout Bitcoin address |
| 8 | Writes `env/.env` with all discovered values |
| 9 | Selects the correct docker-compose overlay for your platform |
| 10 | Builds pool + dashboard with git provenance |
| 11 | Starts services |
| 12 | Verifies health and prints a status report |

---

## Manual setup (if you prefer)

### 1. Clone the repo

```bash
git clone https://github.com/RulezZzOr/BlockFinderPool.git
cd BlockFinderPool
```

### 2. Create your config file

```bash
cp env/.env.example env/.env
# Edit env/.env — at minimum set RPC_URL, RPC_USER, RPC_PASS, PAYOUT_ADDRESS
```

### 3. Start the pool

**Automatic setup:**
```bash
bash run.sh
```

**Standalone:**
```bash
docker compose -f docker-compose.yml -f docker-compose.standalone.yml up -d
```

**Solo hunter profile:**
```bash
docker compose -f docker-compose.solo.yml up -d --build
# Optional dashboard:
docker compose -f docker-compose.solo.yml --profile dashboard up -d --build
```

### 4. Check logs

```bash
docker compose logs -f blackhole-pool
```

You should see:

```
INFO  stratum listening on 0.0.0.0:3333
INFO  ZMQ block connected: tcp://...
INFO  new template: height=... txs=... ...
```

### 5. Connect your miner

Configure your miner with:

| Field | Value |
|---|---|
| URL | `stratum+tcp://YOUR_POOL_HOST:3333` |
| Username | `YOUR_BITCOIN_ADDRESS` (e.g. `bc1q...`) |
| Password | *(anything — leave blank or type `x`)* |

> **Important:** Set your Bitcoin address as the **username**. The pool automatically
> uses it as the coinbase payout address, so the block reward goes directly to
> your wallet. If the username is not a valid Bitcoin address, the pool falls back
> to the `PAYOUT_ADDRESS` set in `env/.env`.

---

## Configuration (`env/.env`)

All settings live in `env/.env`. Copy `env/.env.example` to `env/.env` as shown above.

### Finding your Bitcoin Core RPC credentials (local node)

1. Open your node's Bitcoin Core settings or node dashboard
2. Find the RPC credentials and ZMQ settings
3. Or read them directly from the host:

```bash
# SSH into your Bitcoin node host, then:
grep -E "rpc|zmq" /path/to/bitcoin.conf
```

| `.env` variable | Where to find it |
|---|---|
| `RPC_URL` | `http://<node-ip>:8332` |
| `RPC_USER` | The RPC user from `bitcoin.conf` |
| `RPC_PASS` | The long base64 string in `bitcoin.conf` under `rpcpassword=` |
| `ZMQ_BLOCKS` | `tcp://<node-ip>:28334` |
| `ZMQ_TXS` | `tcp://<node-ip>:28336` |

### Enabling ZMQ on Bitcoin Core

ZMQ is required for low-latency block notifications (reduces stale shares).
Add to your node's `bitcoin.conf`:

```ini
zmqpubhashblock=tcp://0.0.0.0:28334
zmqpubhashtx=tcp://0.0.0.0:28336
```

Then restart Bitcoin Core.

### Network

If your Bitcoin Core node is on the same host or LAN, set `RPC_URL` and `ZMQ_*`
to the node's actual IP or `127.0.0.1` when using host networking.

### Payout address

The recommended approach is to set the miner's username to a Bitcoin address.
The pool then uses that address automatically — no `.env` change needed per miner.

The `PAYOUT_ADDRESS` in `.env` is the **fallback** used when the miner's username
is not a recognisable Bitcoin address (e.g. a plain worker name like `rig1`).

Supported address formats:
- Bech32 (P2WPKH / P2WSH): `bc1q...` / `bc1p...`
- P2SH: `3...`
- P2PKH (legacy): `1...`

---

## Dashboard

Open `http://YOUR_POOL_HOST:3334` in a browser to see:

- Live hashrate chart
- Connected miners table
- Shares accepted / rejected
- Found blocks log
- Network difficulty & estimated time to block

---

## Solo Hunter Mode

BlockFinder includes a solo-hunter path tuned for small LAN fleets and block discovery:

- `mining.submit` updates raw best-share counters immediately after the hash is known
- block candidates are detected on the fast path before slow persistence work
- candidate data is written to the `block_candidates` SQLite table even when `PERSIST_SHARES=false`
- the dashboard shows raw best, accepted best, current-block best, previous-block best, and a recent candidate log

Recent Block Windows show the best share seen by BlockFinder during each Bitcoin network block interval. These are not found blocks unless the share reaches network difficulty and appears in Block Candidates / Blocks Found.

This mode is intended for home users with 1–5 miners on the same LAN. It favors low-latency block discovery and honest reporting over public-pool accounting features.

### Candidate logging

When a share reaches or exceeds the Bitcoin network target, BlockFinder:

1. updates raw best-share metrics immediately
2. spawns the high-priority `submitblock` path immediately
3. persists a forensic row into `block_candidates`
4. updates the final `submitblock_result` and RPC latency after the call returns

The candidate record stores the submit-time forensic details needed for post-mortem analysis:
worker, payout address, session id, job id, height, prevhash, ntime, nonce, version,
version-rolling mask, extranonce values, merkle root, coinbase hex, block header hex,
full block hex when available, block hash, submitted difficulty, network difficulty,
share difficulty, submitblock result, RPC latency, and RPC error.

This happens independently of `PERSIST_SHARES=false`, so a solo hunter can keep the block-candidate audit trail without retaining every accepted share.

Donation address:

`bc1pzvqagy932kmts9rluzpq39upk0hnttz22gdyeslf8lpc4aepyrqslfds96`

Feel free to use it. We are all one family.

---

## API

The REST API is available on port `8080` in standalone mode, or `8081` when
running through the Docker bridge setup:

| Endpoint | Description |
|---|---|
| `GET /health` | Health check |
| `GET /pool` | Pool stats (fee, connected miners, hashrate) |
| `GET /miners` | Per-worker stats |
| `GET /hashrate` | Hashrate history |
| `GET /blocks` | Found blocks |
| `GET /block-candidates` | Recent block candidates and submitblock results |
| `GET /network` | Network difficulty, block height |
| `GET /metrics` | Internal counters (ZMQ, jobs, stales…) |

Example:
```bash
curl http://localhost:8080/pool | python3 -m json.tool
```

---

## Resetting the database

To wipe all share/block data and restart clean:

```bash
bash reset-pool.sh
```

> Run this from the `BlockFinder` directory. You may need `sudo` depending on Docker
> volume permissions.

---

## Troubleshooting

### Pool starts but shows "initial template refresh failed"
Bitcoin Core is not reachable. Check:
- `RPC_URL`, `RPC_USER`, `RPC_PASS` in `env/.env`
- Bitcoin Core is fully synced
- Firewall allows port 8332 from the pool container

### Lots of stale shares
- Verify ZMQ is enabled and `ZMQ_BLOCKS` points to the correct IP/port
- `docker compose logs pool | grep ZMQ` should show "ZMQ block connected"

### Miner connects but no jobs are sent
- Check that `mining.authorize` succeeds in the pool logs
- Verify the Stratum port (3333) is reachable from the miner

### Block found but rejected
- Check `docker compose logs pool | grep SUBMIT` for the rejection reason
- Common causes: stale block (ZMQ latency), nTime out of range, witness commitment mismatch

---

## Project structure

```
BlockFinder/
├── docker-compose.yml        — Compose service definitions
├── env/
│   ├── .env                  — Your config (gitignored)
│   └── .env.example          — Template with instructions
├── pool/                     — Rust backend (Stratum + API)
│   ├── Dockerfile
│   └── src/
│       ├── main.rs
│       ├── config.rs         — Environment variable parsing
│       ├── stratum/mod.rs    — Stratum v1 server
│       ├── template/mod.rs   — GBT fetching + coinbase building
│       ├── share/mod.rs      — Share validation
│       ├── api/mod.rs        — REST API (Axum)
│       ├── metrics.rs        — In-memory stats
│       ├── vardiff.rs        — Auto difficulty adjustment
│       ├── rpc.rs            — Bitcoin Core RPC client
│       └── storage/          — SQLite + Redis (optional)
├── dashboard/                — React frontend
│   ├── Dockerfile
│   ├── nginx.conf
│   └── src/
└── reset-pool.sh             — Wipe DB and restart
```

---

## Security notes

- `AUTH_TOKEN` is empty by default — any miner that can reach port 3333 can connect.
  Set it in `env/.env` if you want password-protected access.
- Set `REQUIRE_AUTH_TOKEN=true` to make the pool refuse to start if `AUTH_TOKEN` is
  not set — useful as a safety net in network-facing deployments.
- The pool is designed for a **personal home node**. Do not expose port 3333 to the
  internet unless you understand the implications.
- Never commit `env/.env` to version control — it is listed in `.gitignore`.

---

## Operational tuning reference

### Bitcoin Core version

**Recommended: v30.2.0 or later.**

Measured GBT tail latency (p95):
- v30.0.0: ~80ms (occasionally spikes to 565ms)
- v30.2.0: ~50ms (more consistent)

Upgrade your node using its normal update path.

### `PERSIST_SHARES`

| Value | Behaviour | When to use |
|-------|-----------|-------------|
| `false` (default) | No share rows written; `worker_best` still persisted | High hashrate, NVMe not a concern |
| `true` | Every accepted share stored in SQLite | Forensic audit trail, historical analysis |

Disk budget: ~200 bytes/share → ~300 MB per million shares (~69 h at 240 TH/s).

### Solo Hunter defaults

For a 1-5 miner LAN node, the recommended defaults are:

```env
SOLO_MODE=true
PERSIST_SHARES=false
PERSIST_BLOCKS=true
PERSIST_BEST=true
BEST_PERSIST_INTERVAL_SECS=10
TARGET_SHARE_TIME_SECS=20
VARDIFF_RETARGET_SECS=45
MIN_DIFFICULTY=8192
MAX_DIFFICULTY=262144
STRATUM_START_DIFFICULTY=32768
```

### `ZMQ_DEBOUNCE_MS`

Controls how often ZMQ TX events trigger a `getblocktemplate` call.
The dedup layer absorbs ~86% of responses as identical, so miner job rate
is largely independent of this value.  The main effect is Bitcoin Core CPU load.

| Value | GBT calls/min | Core data | Use case |
|-------|--------------|-----------|---------|
| 1000 | ~66 | ~3.2 MB/s | Maximum fee freshness |
| 1500 | ~44 | ~2.1 MB/s | Balanced (recommended) |
| 3000 | ~22 | ~1.1 MB/s | Reduce Core CPU, minimal freshness cost |

### `POST_BLOCK_SUPPRESS_MS`

After a new block, ZMQ TX events are suppressed for this many ms to avoid
hammering Bitcoin Core while it processes the new mempool burst.
Wire-measured: mempool stabilises 10–12 s after a block.
Default: 12000 ms. Increase to 15000–20000 if Core CPU spikes after blocks.

### Timing synchronisation rule

For a perfectly coordinated job pipeline, keep these equal:
```
NOTIFY_BUCKET_REFILL_MS = ZMQ_DEBOUNCE_MS
```
This ensures every new template from Bitcoin Core finds a token ready in the
per-miner bucket — no artificial delays, no wasted refills.

---

## API Reference

 The REST API is available on **port 8080** in standalone/solo mode, or **8081**
 when routed through the Docker bridge setup.

| Endpoint | Description |
|---|---|
| `GET /health` | Health check `{"ok":true}` |
| `GET /pool` | Full pool stats (hashrate, miners, blocks, counters) |
| `GET /miners` | Per-worker stats (hashrate, shares, best diff, RTT) |
| `GET /blockfinder/miners` | Enriched miner list with firmware, session info |
| `GET /blockfinder/connection-status` | RPC + ZMQ + Stratum live connectivity |
| `GET /blockfinder/template-info` | Current block template (height, txs, target) |
| `GET /blockfinder/mempool` | Live mempool info from Bitcoin Core |
| `GET /blocks` | Found blocks history |
| `GET /build-info` | Build provenance (git SHA, timestamp, image ID) |

Quick check:
```bash
curl http://localhost:8081/pool | python3 -m json.tool
curl http://localhost:8081/blockfinder/connection-status | python3 -m json.tool
```

---

## Monitoring commands

```bash
# Live pool logs
docker compose logs -f blackhole-pool

# Container health
docker ps --filter "name=blackhole"

# Pool stats snapshot
curl -s http://localhost:8081/pool | python3 -m json.tool

# Miner list
curl -s http://localhost:8081/blockfinder/miners | python3 -m json.tool

# Connection status (RPC + ZMQ + Stratum)
curl -s http://localhost:8081/blockfinder/connection-status | python3 -m json.tool

# Check for errors in logs
docker logs blackhole-blackhole-pool-1 2>&1 | grep -E "ERROR|WARN|BLOCK"
```
