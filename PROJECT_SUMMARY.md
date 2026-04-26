# BlockFinderPool Project Summary

This document records what we built, why it exists, and the important design
decisions behind the current BlockFinderPool solo-hunting node.

## What BlockFinderPool Is

BlockFinderPool is a self-hosted Bitcoin solo hunter node for small home or LAN
mining fleets, typically 1-5 miners.

The goal is not to become a large public pool. The goal is:

- low practical Stratum latency
- honest best-share reporting
- fast block-candidate submission
- clear dashboard visibility
- simple home deployment next to Bitcoin Core
- zero pool fee

When a valid block is found, the coinbase payout is built for the miner's own
Bitcoin address when the miner uses a Bitcoin address as the Stratum username.
`PAYOUT_ADDRESS` is only the fallback when a miner username is not a valid
Bitcoin address.

## Provenance And License

BlockFinderPool began as an experimental fork of
`BlackHole-Axe/BlackHolePool`.

The codebase has been substantially reworked, but the repository intentionally
keeps a non-commercial "No Sale" license because there may still be derived
code, structure, or implementation lineage from the original project.

This means:

- personal and non-commercial use is allowed
- selling the software or using it as a paid product/service is not granted
- commercial users need separate legal review and permission
- this repository should not claim MIT, Apache-2.0, GPL, or another standard
  open-source license unless the provenance is resolved

See `LICENSE` and `NOTICE.md`.

## Main Components

| Component | Purpose |
|---|---|
| Rust Stratum server | Handles miner TCP sessions and `mining.submit` |
| Template engine | Maintains Bitcoin Core block templates via RPC and ZMQ |
| Share validation | Builds coinbase/header, hashes share, computes true difficulty |
| Metrics store | Tracks hashrate, workers, raw best, accepted best, block windows |
| SQLite storage | Persists blocks, candidates, best records, block window summaries |
| REST API | Exposes pool state, miners, windows, candidates, dashboard snapshot |
| React dashboard | Shows live solo-hunting state and recent block windows |
| Docker setup | Runs the pool and dashboard as a small self-hosted stack |

## Solo Hunter Philosophy

BlockFinderPool is optimized for solo block hunting, not public pool accounting.

Important priorities:

1. Submit a valid block candidate as quickly as possible.
2. Never let SQLite or dashboard work block the Stratum ACK path.
3. Track raw submitted best share separately from accepted best share.
4. Show users exactly what happened during each Bitcoin network block window.
5. Prefer local Bitcoin Core, local ZMQ, and LAN miners.

Things it does not try to do:

- PPS/PPLNS accounting
- public user balances
- large public-pool scaling
- luck prediction
- hash prediction
- "AI-guided" nonce selection

Every SHA256d attempt is independent. AI can help monitor and tune the system,
but it cannot predict which hash will be a block.

## Stratum Protocol

Stratum is TCP, not UDP.

Miners connect like:

```text
miner -> TCP -> BlockFinderPool:3333
```

Messages are newline-delimited JSON-RPC, for example:

```json
{"id":1,"method":"mining.subscribe","params":[]}
```

The pool returns:

- `extranonce1`
- `extranonce2_size`
- subscription data

`EXTRANONCE2_SIZE` is configurable through `env/.env`. The current code supports
larger extranonce2 values safely because duplicate-share keys keep
`extranonce2` as a string instead of truncating it into an integer.

Recommended modern solo-hunter value:

```env
EXTRANONCE2_SIZE=8
```

This should not slow mining. It only gives miners/proxies more search-space
room and improves compatibility with tools that require `extranonce2_size >= 7`.

## Best-Share Semantics

A major change was making best-share reporting explicit.

BlockFinderPool tracks separate views:

| Metric | Meaning |
|---|---|
| Best Submitted Share | Raw best hash attempt seen by the pool |
| Best Accepted Share | Best share accepted at configured/session difficulty |
| Best This Block | Best share during the current network block window |
| Previous Block Best | Best share during the previous network block window |
| All-Time Best | Persisted historical best |
| Block Candidate Best | Best share that reached network target |

This matters because ckpool/public-pool-style displays often show raw submitted
best share, while a strict accepted-only view can look worse even when the
actual mining performance is identical.

In BlockFinderPool:

- raw submitted best updates after a submit is parsed, header is built, and
  share difficulty is known
- accepted best updates only after the share passes pool/session difficulty
- stale/rejected shares can still update raw best if they were validly parsed
- accepted best is not polluted by stale/rejected shares
- block-candidate best updates only when network target is reached

## Block Candidate Path

The candidate path was changed for solo hunting.

The intended order is:

1. Parse `mining.submit`
2. Build and verify the header
3. Calculate hash and true difficulty
4. Update raw best-submitted metrics
5. If hash reaches network target, submit the block immediately
6. Then handle duplicate/stale/session-difficulty logic
7. Send miner response quickly
8. Persist non-critical data asynchronously

Important rule:

```text
submitblock must not wait behind SQLite.
```

Block candidate forensic records are still persisted, but persistence happens
after or outside the critical `submitblock` action.

The candidate forensic record includes:

- worker
- payout address
- session id
- job id
- height
- prevhash
- ntime
- nonce
- version
- version-rolling mask
- extranonce values
- merkle root
- coinbase hex
- block header hex
- block/full block hex when available
- block hash
- submitted difficulty
- network difficulty
- current share difficulty
- submitblock result
- RPC latency
- RPC error

## Persistence Model

Solo mode defaults are intentionally light:

```env
SOLO_MODE=true
PERSIST_SHARES=false
PERSIST_BLOCKS=true
PERSIST_BEST=true
BEST_PERSIST_INTERVAL_SECS=10
```

By default, BlockFinderPool does not write every accepted share into SQLite.

It persists important things:

- found blocks
- block candidates
- all-time best summaries
- per-worker best summaries
- recent block window summaries
- selected session/engine summaries

This keeps SQLite away from the hot path.

## Recent Block Windows

The dashboard includes "Recent Block Windows".

This is not a found-block list.

It shows the best BlockFinder share seen during each Bitcoin network block
interval. The current window is shown live, and previous windows are finalized
when a new prevhash/template is detected.

Each window can show:

- Bitcoin block height
- prevhash / block hash when known
- duration
- external pool if known
- transaction count
- best submitted difficulty
- best accepted difficulty
- best worker
- payout address
- proximity to current network difficulty
- pool hashrate estimate for that window

Correct label:

```text
Best Share This Network Block
```

Incorrect label:

```text
Block Found
```

It is only a found block if the share reaches network difficulty and appears in
Block Candidates / Blocks Found.

## Dashboard And API Optimization

The dashboard originally made several independent API requests every refresh.
That was acceptable on LAN, but it could create unnecessary load and slow SQLite
warnings.

The current direction is:

```text
dashboard -> /dashboard-snapshot -> RAM/cached summaries
```

Important API/cache decisions:

- `/dashboard-snapshot` provides the main dashboard payload in one request
- dashboard refresh can run every 1 second
- `getmininginfo` is cached for 1 second
- public mempool.space blocks are cached for 30 seconds
- SQLite-backed dashboard sections are cached for 5 seconds
- stale values are returned immediately while background refresh runs
- atomic refresh guards prevent multiple concurrent refresh tasks

The goal is not to save CPU. The goal is to avoid dashboard/RPC/SQLite latency
affecting the mining process.

## ZMQ And Template Handling

Bitcoin Core ZMQ is used for low-latency block and transaction notifications.

Recommended local settings:

```env
ZMQ_BLOCKS=tcp://127.0.0.1:28334,tcp://127.0.0.1:28335
ZMQ_TXS=tcp://127.0.0.1:28336,tcp://127.0.0.1:28337
```

Mining-critical signal:

- block ZMQ
- current template age
- RPC health

TX ZMQ is useful for fee/template freshness, but it is not as critical as block
ZMQ for stale reduction.

The template engine also keeps polling as a fallback. ZMQ is low-latency, but
longpoll/poll fallback prevents total dependency on ZMQ.

## Recommended Home Solo Settings

Typical home/LAN solo-hunter configuration:

```env
SOLO_MODE=true
PERSIST_SHARES=false
PERSIST_BLOCKS=true
PERSIST_BEST=true
BEST_PERSIST_INTERVAL_SECS=10

STRATUM_BIND=0.0.0.0
STRATUM_PORT=3333
API_BIND=0.0.0.0
API_PORT=8080

EXTRANONCE1_SIZE=4
EXTRANONCE2_SIZE=8

VARDIFF_ENABLED=true
TARGET_SHARE_TIME_SECS=15
VARDIFF_RETARGET_SECS=30
MIN_DIFFICULTY=8192
MAX_DIFFICULTY=16777216
STRATUM_START_DIFFICULTY=32768
RECONNECT_RECENT_SECS=15

ZMQ_DEBOUNCE_MS=1500
POST_BLOCK_SUPPRESS_MS=8000
NOTIFY_BUCKET_CAPACITY=2
NOTIFY_BUCKET_REFILL_MS=1500
JOB_REFRESH_MS=30000
TEMPLATE_POLL_MS=30000
TEMPLATE_MAX_AGE_SECS=20

POOL_TAG=/BlockFinder Solo Pool
COINBASE_MESSAGE=

RUST_LOG=info
```

These are not magic luck settings. They are operational settings intended to
keep the pool stable, low-latency, and honest.

## What To Watch In Production

Useful health checks:

```bash
curl -s http://127.0.0.1:8080/pool | jq '{
  miners: .totalMiners,
  templateAge: .templateAgeSecs,
  rpcHealthy: .rpcHealthy,
  zmqConnected: .zmqConnected,
  staleRatio: .staleRatio,
  submitP99: .submitRttP99Ms,
  submitMax: .submitRttMaxMs,
  reconnects: .reconnectsTotal
}'
```

Dashboard snapshot timing:

```bash
for i in $(seq 1 20); do
  curl -s -w "time=%{time_total}s code=%{http_code}\n" \
    -o /dev/null http://127.0.0.1:8080/dashboard-snapshot
  sleep 1
done
```

SQLite slow warnings:

```bash
docker logs --since 10m blockfinderpool_blackhole-pool_1 | grep -i "slow statement"
```

Miner share sanity:

```bash
curl -s http://127.0.0.1:8080/miners | jq '.[] | {
  worker,
  shares,
  rejected,
  stale,
  last_seen,
  best_submitted_difficulty
}'
```

## What AI Can And Cannot Do

AI cannot:

- predict SHA256 hashes
- choose a better nonce
- improve luck directly
- know which share will become a block

AI can help with:

- anomaly detection
- miner underperformance detection
- stale/reconnect monitoring
- vardiff tuning recommendations
- template-age alerts
- ZMQ/RPC health checks
- dashboard interpretation
- post-mortem analysis after a block candidate

## Practical Limits

At home-miner scale, expected block interval is dominated by hashrate.

Latency optimization matters most when:

- a miner actually finds a valid block candidate
- a new block arrives and stale work must be replaced quickly
- the node is competing to submit a block before another miner/pool

Latency tuning does not change the mathematical expected value of hashrate.
It reduces avoidable operational losses.

## Current Project Direction

The current BlockFinderPool direction is:

- stay focused on home solo hunting
- keep Stratum simple and fast
- keep dashboard truthful
- avoid SQLite in hot paths
- keep public-pool/ckpool-compatible raw best-share semantics
- improve observability without adding public-pool complexity
- keep the code practical and deployable with Docker

High-diff listener work is intentionally skipped for now.

## Donation

Donation address:

```text
bc1pzvqagy932kmts9rluzpq39upk0hnttz22gdyeslf8lpc4aepyrqslfds96
```

Feel free to use it. We are all one family.
