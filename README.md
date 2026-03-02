# SoloPool

A self-hosted Bitcoin solo mining pool designed for Umbrel home nodes.
Zero fee — 100% of every block reward goes directly to the miner's own address.

---

## Architecture

| Component | Technology | Port |
|---|---|---|
| Stratum server | Rust + Tokio | 2018 |
| REST API | Axum | 8081 (host) |
| Dashboard | React + Nginx | 3334 (host) |
| Bitcoin node | Bitcoin Core via RPC + ZMQ | (external) |
| Database | SQLite | (Docker volume) |

---

## Quick Start

### 1. Prerequisites

- [Docker](https://docs.docker.com/engine/install/) + Docker Compose
- An Umbrel node (or any Bitcoin Core full node) running and synced
- ZMQ enabled on your Bitcoin node (see below)

### 2. Clone the repo

```bash
git clone https://github.com/YOUR_USERNAME/SoloPool.git
cd SoloPool
```

### 3. Create your config file

```bash
cp env/.env.example env/.env
```

Then edit `env/.env` and fill in your values (see the detailed sections below).

### 4. Start the pool

```bash
docker compose up -d
```

Check that everything started cleanly:

```bash
docker compose logs -f pool
```

You should see:

```
INFO  stratum listening on 0.0.0.0:2018
INFO  ZMQ block connected: tcp://...
INFO  new template: height=... txs=... ...
```

### 5. Connect your miner

Configure your miner with:

| Field | Value |
|---|---|
| URL | `stratum+tcp://YOUR_POOL_HOST:2018` |
| Username | `YOUR_BITCOIN_ADDRESS` (e.g. `bc1q...`) |
| Password | *(anything — leave blank or type `x`)* |

> **Important:** Set your Bitcoin address as the **username**. The pool automatically
> uses it as the coinbase payout address, so the block reward goes directly to
> your wallet. If the username is not a valid Bitcoin address, the pool falls back
> to the `PAYOUT_ADDRESS` set in `env/.env`.

---

## Configuration (`env/.env`)

All settings live in `env/.env`. Copy `env/.env.example` to `env/.env` as shown above.

### Finding your Bitcoin Core RPC credentials (Umbrel)

1. Open your Umbrel dashboard → **Bitcoin Node** app
2. Go to **Connect** or **Advanced** tab
3. Or read them directly from your node:

```bash
# SSH into Umbrel, then:
cat ~/umbrel/app-data/bitcoin/data/bitcoin/bitcoin.conf | grep -E "rpc|zmq"
```

| `.env` variable | Where to find it |
|---|---|
| `RPC_URL` | `http://<umbrel-ip>:8332` — on Umbrel the internal Docker IP is `10.21.21.8` |
| `RPC_USER` | Usually `umbrel` |
| `RPC_PASS` | The long base64 string in `bitcoin.conf` under `rpcpassword=` |
| `ZMQ_BLOCKS` | `tcp://<umbrel-ip>:28334` |
| `ZMQ_TXS` | `tcp://<umbrel-ip>:28336` |

### Enabling ZMQ on Bitcoin Core

ZMQ is required for low-latency block notifications (reduces stale shares).
On Umbrel it is enabled by default. On a custom node, add to `bitcoin.conf`:

```ini
zmqpubhashblock=tcp://0.0.0.0:28334
zmqpubhashtx=tcp://0.0.0.0:28336
```

Then restart Bitcoin Core.

### Network (Umbrel Docker)

The pool runs inside `umbrel_main_network` (the same Docker network as Umbrel's
Bitcoin Core container). This is why the RPC/ZMQ IPs use `10.21.21.8` — that is
Bitcoin Core's fixed IP inside the Umbrel network.

If you run outside Umbrel, change `RPC_URL` and `ZMQ_*` to your node's actual IP.

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

## API

The REST API is available on port `8081`:

| Endpoint | Description |
|---|---|
| `GET /health` | Health check |
| `GET /pool` | Pool stats (fee, connected miners, hashrate) |
| `GET /miners` | Per-worker stats |
| `GET /hashrate` | Hashrate history |
| `GET /blocks` | Found blocks |
| `GET /network` | Network difficulty, block height |
| `GET /metrics` | Internal counters (ZMQ, jobs, stales…) |

Example:
```bash
curl http://localhost:8081/pool | python3 -m json.tool
```

---

## Resetting the database

To wipe all share/block data and restart clean:

```bash
bash reset-pool.sh
```

> Run this from the `SoloPool` directory. You may need `sudo` depending on Docker
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
- Verify the Stratum port (2018) is reachable from the miner

### Block found but rejected
- Check `docker compose logs pool | grep SUBMIT` for the rejection reason
- Common causes: stale block (ZMQ latency), nTime out of range, witness commitment mismatch

---

## Project structure

```
SoloPool/
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

- `AUTH_TOKEN` is empty by default — any miner that can reach port 2018 can connect.
  Set it in `.env` if you want password-protected access.
- The pool is designed for a **personal home node**. Do not expose port 2018 to the
  internet unless you understand the implications.
- Never commit `env/.env` to version control — it is listed in `.gitignore`.
