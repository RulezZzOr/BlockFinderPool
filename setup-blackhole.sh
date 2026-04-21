#!/usr/bin/env bash
# ════════════════════════════════════════════════════════════════════════════
#  BlockFinder Pool — Universal Installer
#
#  Supports:
#    • Umbrel Home (auto-detects Bitcoin Core container & credentials)
#    • Standard Linux  (Ubuntu / Debian / Fedora / Arch)
#    • macOS with Docker Desktop
#    • Any machine with a reachable Bitcoin Core node (local or remote)
#
#  Usage:
#    bash setup-blackhole.sh              # interactive
#    bash setup-blackhole.sh --unattended # non-interactive (uses defaults)
#
#  What it does:
#    1. Checks prerequisites (Docker, Docker Compose)
#    2. Auto-detects Bitcoin Core (Docker container or native)
#    3. Reads RPC credentials + ZMQ settings from bitcoin.conf
#    4. Offers to enable ZMQ if missing (required for fast block detection)
#    5. Generates env/.env from discovered values
#    6. Writes the correct docker-compose override for your environment
#    7. Builds BlockFinder with git provenance
#    8. Starts the pool + dashboard
#    9. Verifies all connections are healthy
#   10. Prints a full status report
# ════════════════════════════════════════════════════════════════════════════
set -euo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

UNATTENDED=false
for arg in "$@"; do [[ "$arg" == "--unattended" ]] && UNATTENDED=true; done

# ── Terminal colours ─────────────────────────────────────────────────────────
if [ -t 1 ]; then
  RED='\033[0;31m'; YEL='\033[1;33m'; GRN='\033[0;32m'
  CYN='\033[0;36m'; BLD='\033[1m'; DIM='\033[2m'; RST='\033[0m'
else
  RED=''; YEL=''; GRN=''; CYN=''; BLD=''; DIM=''; RST=''
fi

ok()   { echo -e "${GRN}[✓]${RST} $*"; }
warn() { echo -e "${YEL}[!]${RST} $*"; }
err()  { echo -e "${RED}[✗]${RST} $*" >&2; }
info() { echo -e "${CYN}[→]${RST} $*"; }
sep()  { echo -e "${DIM}────────────────────────────────────────────────────${RST}"; }
ask()  { echo -e "${BLD}${CYN}[?]${RST}${BLD} $*${RST}"; }

# ── Banner ───────────────────────────────────────────────────────────────────
echo ""
echo -e "${BLD}${CYN}  ╔══════════════════════════════════════════════╗"
echo    "  ║   B L A C K H O L E   S O L O   P O O L    ║"
echo -e "  ║         Universal Installer v1.0            ║${RST}"
echo -e "${DIM}  ╚══════════════════════════════════════════════╝${RST}"
echo ""

# ════════════════════════════════════════════════════════════════════════════
# PHASE 1 — Prerequisites
# ════════════════════════════════════════════════════════════════════════════
sep; info "Phase 1 — Checking prerequisites"

need_cmd() {
  if ! command -v "$1" &>/dev/null; then
    err "Required: '$1' not found. Please install it first."
    exit 1
  fi
}
need_cmd docker
need_cmd curl

# Docker Compose (v2 plugin preferred, v1 fallback)
# Detect by executing `version`, because some environments fail `docker compose ls`
# even when Compose itself is installed and working.
DC=()
if docker compose version >/dev/null 2>&1; then
  DC=(docker compose)
elif docker-compose version >/dev/null 2>&1; then
  DC=(docker-compose)
fi

if [ ${#DC[@]} -eq 0 ]; then
  err "Docker Compose not found."
  err "Install Docker Desktop or the compose plugin:"
  err "  https://docs.docker.com/compose/install/"
  err ""
  err "On macOS with Homebrew:"
  err "  brew install docker-compose  (v1)"
  err "  OR install Docker Desktop (includes v2 plugin)"
  exit 1
fi

_DC_VER=$($DC version --short 2>/dev/null \
         || $DC version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 \
         || echo "unknown")
ok "Docker:  $(docker --version | cut -d' ' -f3 | tr -d ',')"
ok "Compose: $_DC_VER  ($DC)"

# ════════════════════════════════════════════════════════════════════════════
# PHASE 2 — Environment Detection
# ════════════════════════════════════════════════════════════════════════════
sep; info "Phase 2 — Detecting environment"

PLATFORM="standalone"
BTC_IP=""
BTC_PORT=8332
BTC_USER=""
BTC_PASS=""
BTC_NETWORK="mainnet"
BTC_CONF=""
BTC_CONTAINER=""
ZMQ_BLOCK_PORT=28334
ZMQ_TX_PORT=28336
ZMQ_BLOCKS_URL=""
ZMQ_TXS_URL=""
DOCKER_NETWORK="blackhole_net"
NETWORK_EXTERNAL=false
VOLUME_EXTERNAL=false
VOLUME_NAME="blackhole_pool_data"
COMPOSE_FILES=(-f docker-compose.yml -f docker-compose.standalone.yml)

# ── Umbrel detection ─────────────────────────────────────────────────────────
if [ -f "/home/umbrel/umbrel/umbrel.yaml" ] || \
   [ -d "/home/umbrel/umbrel/app-data" ]   || \
   command -v umbreld &>/dev/null 2>&1; then
  PLATFORM="umbrel"
  DOCKER_NETWORK="umbrel_main_network"
  NETWORK_EXTERNAL=true
  VOLUME_EXTERNAL=true
  COMPOSE_FILES=(-f docker-compose.yml)
  ok "Platform: Umbrel Home"
else
  ok "Platform: Standalone (Linux/macOS)"
  COMPOSE_FILES=(-f docker-compose.standalone.yml)
fi

# ════════════════════════════════════════════════════════════════════════════
# PHASE 3 — Bitcoin Core Auto-Discovery
# Priority order (first match wins):
#   1. Container environment vars  (docker exec env)  ← fastest, most reliable
#   2. Docker inspect networks     (IP address)
#   3. bitcoin-cli RPC calls       (ZMQ, network, chain info)
#   4. Umbrel app-data .env file   (fallback credentials)
#   5. bitcoin.conf parsing        (fallback credentials + ZMQ)
#   6. Interactive prompt          (last resort)
# ════════════════════════════════════════════════════════════════════════════
sep; info "Phase 3 — Auto-detecting Bitcoin Core (zero-touch)"

# ── Find Bitcoin Core container ──────────────────────────────────────────────
BTC_CONTAINER=""
_find_btc_container() {
  local names=("bitcoin_app_1" "bitcoin-bitcoin-1" "bitcoin-core" "bitcoind"
               "btc_node" "bitcoin" "umbrel_bitcoin_1")
  for n in "${names[@]}"; do
    if docker inspect "$n" &>/dev/null 2>&1; then echo "$n"; return 0; fi
  done
  # fuzzy: any running container whose name contains 'bitcoin'
  docker ps --format '{{.Names}}' 2>/dev/null \
    | grep -iE 'bitcoin|btc' | head -1 || true
}
BTC_CONTAINER=$(_find_btc_container)

if [ -n "$BTC_CONTAINER" ]; then
  ok "Found Bitcoin Core container: ${BLD}$BTC_CONTAINER${RST}"

  # ── 1. Pull EVERYTHING from container env vars (most reliable) ─────────────
  CENV=$(docker exec "$BTC_CONTAINER" env 2>/dev/null || true)

  _cenv() { echo "$CENV" | grep -i "^$1=" | cut -d= -f2- | head -1 || true; }

  BTC_USER=$(_cenv RPC_USER)
  BTC_PASS=$(_cenv RPC_PASS)
  BTC_PORT_CENV=$(_cenv RPC_PORT); [ -n "$BTC_PORT_CENV" ] && BTC_PORT="$BTC_PORT_CENV"

  # ZMQ ports directly from container env
  ZMQ_HASHBLOCK=$(_cenv ZMQ_HASHBLOCK_PORT)
  ZMQ_RAWBLOCK=$(_cenv ZMQ_RAWBLOCK_PORT)
  ZMQ_HASHTX=$(_cenv ZMQ_HASHTX_PORT)
  ZMQ_RAWTX=$(_cenv ZMQ_RAWTX_PORT)
  [ -n "$ZMQ_HASHBLOCK" ] && ZMQ_BLOCK_PORT="$ZMQ_HASHBLOCK"
  [ -n "$ZMQ_HASHTX" ]    && ZMQ_TX_PORT="$ZMQ_HASHTX"

  # Network from container env — lowercase-safe for bash 3.2 (macOS default)
  NET_ENV=$(echo "$(_cenv BITCOIN_NETWORK)" | tr '[:upper:]' '[:lower:]')
  case "$NET_ENV" in
    testnet*) BTC_NETWORK="testnet" ;;
    signet)   BTC_NETWORK="signet"  ;;
    regtest)  BTC_NETWORK="regtest" ;;
    *)        BTC_NETWORK="mainnet" ;;
  esac

  [ -n "$BTC_USER" ] && ok "  RPC user:  ${BLD}$BTC_USER${RST}  (from container env)"
  [ -n "$BTC_PASS" ] && ok "  RPC pass:  ${BLD}${BTC_PASS:0:8}…${RST}  (from container env)"

  # ── 2. Get correct IP for pool→bitcoin communication ─────────────────────
  # Strategy depends on platform:
  #  Umbrel     → pool joins umbrel_main_network → use container's IP there
  #  Standalone → pool is on blackhole_net, different from Bitcoin's network
  #               → connect pool container to Bitcoin's network after start
  #               → use Bitcoin container's IP on that network

  _net_ip() {
    docker inspect "$BTC_CONTAINER" \
      --format "{{range \$k,\$v := .NetworkSettings.Networks}}{{if eq \$k \"$1\"}}{{\$v.IPAddress}}{{end}}{{end}}" \
      2>/dev/null || true
  }

  if [ "$PLATFORM" = "umbrel" ]; then
    # Umbrel: pool will be on umbrel_main_network, get BTC IP there
    BTC_IP=$(_net_ip "umbrel_main_network")
    [ -z "$BTC_IP" ] && BTC_IP=$(_net_ip "$DOCKER_NETWORK")
  else
    # Standalone: find Bitcoin Core's primary Docker network
    # We'll connect pool to this network after it starts
    BTC_DOCKER_NET=$(docker inspect "$BTC_CONTAINER" \
      --format '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' \
      2>/dev/null | tr ' ' '\n' | grep -vE '^$|^bridge$' | head -1 || true)
    # Fallback to bridge
    [ -z "$BTC_DOCKER_NET" ] && BTC_DOCKER_NET="bridge"
    # Get BTC IP on that network
    BTC_IP=$(_net_ip "$BTC_DOCKER_NET")
    ok "  BTC network: ${BLD}$BTC_DOCKER_NET${RST} (pool will join this network)"
  fi

  # Final fallback
  if [ -z "$BTC_IP" ]; then
    BTC_IP=$(docker inspect "$BTC_CONTAINER" \
      --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' \
      2>/dev/null | grep -v '^$' | head -1 || true)
  fi
  [ -z "$BTC_IP" ] && BTC_IP="127.0.0.1"
  ok "  Node IP:   ${BLD}$BTC_IP${RST}  (via docker inspect)"

  # ── 3. Live RPC: get chain + ZMQ URLs from running bitcoind ───────────────
  if [ -n "$BTC_USER" ] && [ -n "$BTC_PASS" ]; then
    # Auto-detect datadir inside container (Umbrel uses /data/bitcoin, others vary)
    _btc_datadir=$(docker exec "$BTC_CONTAINER" \
      sh -c 'for d in /data/bitcoin /root/.bitcoin /home/bitcoin/.bitcoin /bitcoin; do
               [ -f "$d/bitcoin.conf" ] && echo "$d" && exit 0; done; echo ""' \
      2>/dev/null || true)
    _datadir_flag=""
    [ -n "$_btc_datadir" ] && _datadir_flag="-datadir=${_btc_datadir}"

    _rpc() {
      docker exec "$BTC_CONTAINER" \
        bitcoin-cli $_datadir_flag -rpcuser="$BTC_USER" -rpcpassword="$BTC_PASS" \
        "$@" 2>/dev/null || true
    }

    # Network / chain
    CHAIN_INFO=$(_rpc getblockchaininfo)
    if [ -n "$CHAIN_INFO" ]; then
      CHAIN=$(echo "$CHAIN_INFO" | grep -o '"chain":"[^"]*"' | cut -d'"' -f4)
      HEIGHT=$(echo "$CHAIN_INFO" | grep -o '"blocks":[0-9]*' | cut -d: -f2)
      case "$CHAIN" in
        test) BTC_NETWORK="testnet" ;;
        signet) BTC_NETWORK="signet" ;;
        regtest) BTC_NETWORK="regtest" ;;
        main|*) BTC_NETWORK="mainnet" ;;
      esac
      ok "  Chain:     ${BLD}$CHAIN${RST}  height=$HEIGHT"
    fi

    # ZMQ: get actual listening URLs from bitcoind
    ZMQ_INFO=$(_rpc getzmqnotifications)
    if [ -n "$ZMQ_INFO" ]; then
      # Flatten multi-line JSON to single line for reliable grep
      ZMQ_FLAT=$(echo "$ZMQ_INFO" | tr -d '\n' | tr -s ' ')
      # Extract address for each ZMQ type using awk (works on BSD+GNU)
      _zmq_addr() {
        echo "$ZMQ_FLAT" | awk -F'"' -v t="$1" '
          {for(i=1;i<=NF;i++) if($i==t){for(j=i;j<=NF;j++) if($j=="address"){print $(j+2);exit}}}
        '
      }
      HB_ADDR=$(_zmq_addr "pubhashblock")
      RB_ADDR=$(_zmq_addr "pubrawblock")
      HT_ADDR=$(_zmq_addr "pubhashtx")
      RT_ADDR=$(_zmq_addr "pubrawtx")

      # Replace 0.0.0.0 with actual IP
      _fix_addr() { echo "${1//0.0.0.0/$BTC_IP}"; }

      [ -n "$HB_ADDR" ] && ZMQ_BLOCKS_URL=$(_fix_addr "$HB_ADDR")
      [ -n "$RB_ADDR" ] && ZMQ_BLOCKS_URL2=$(_fix_addr "$RB_ADDR")
      [ -n "$HT_ADDR" ] && ZMQ_TXS_URL=$(_fix_addr "$HT_ADDR")
      [ -n "$RT_ADDR" ] && ZMQ_TXS_URL2=$(_fix_addr "$RT_ADDR")

      [ -n "$ZMQ_BLOCKS_URL" ] && ok "  ZMQ blocks: ${BLD}$ZMQ_BLOCKS_URL${RST}  (from getzmqnotifications)"
      [ -n "$ZMQ_TXS_URL"    ] && ok "  ZMQ txs:   ${BLD}$ZMQ_TXS_URL${RST}"
    fi
  fi

  # ── 4. Fallback: Umbrel app-data .env ──────────────────────────────────────
  if [ -z "$BTC_USER" ] || [ -z "$BTC_PASS" ]; then
    for env_path in \
        "/home/umbrel/umbrel/app-data/bitcoin/.env" \
        "/home/umbrel/umbrel/app-data/bitcoin/data/.env"; do
      if [ -f "$env_path" ]; then
        _fv() { grep -E "^(export )?$1=" "$env_path" 2>/dev/null | head -1 \
                | sed "s/.*=//;s/'//g;s/\"//g" || true; }
        u=$(_fv APP_BITCOIN_RPC_USER); p=$(_fv APP_BITCOIN_RPC_PASS)
        [ -n "$u" ] && BTC_USER="$u"; [ -n "$p" ] && BTC_PASS="$p"
        [ -n "$u" ] && ok "  Credentials from $env_path"
        break
      fi
    done
  fi

  # ── 5. Fallback: bitcoin.conf ────────────────────────────────────────────
  if [ -z "$BTC_USER" ] || [ -z "$BTC_PASS" ]; then
    DATA_MOUNT=$(docker inspect "$BTC_CONTAINER" \
      --format '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Source}}{{end}}{{end}}' \
      2>/dev/null | head -1 || true)
    BTC_CONF="${DATA_MOUNT}/bitcoin/bitcoin.conf"
    if [ -f "$BTC_CONF" ]; then
      while IFS='=' read -r k v; do
        k=$(echo "$k" | sed 's/#.*//;s/[[:space:]]//g'); v=$(echo "$v" | sed 's/#.*//;s/[[:space:]\r]//g')
        [ -z "$k" ] && continue
        case "$k" in
          rpcuser)     [ -z "$BTC_USER" ] && BTC_USER="$v" ;;
          rpcpassword) [ -z "$BTC_PASS" ] && BTC_PASS="$v" ;;
          rpcport)     BTC_PORT="$v" ;;
        esac
      done < "$BTC_CONF"
      # rpcauth fallback: get username only (can't reverse password hash)
      [ -z "$BTC_USER" ] && \
        BTC_USER=$(grep '^rpcauth=' "$BTC_CONF" 2>/dev/null | head -1 \
          | cut -d= -f2 | cut -d: -f1 || true)
      ok "  Parsed bitcoin.conf: $BTC_CONF"
    fi
  fi

else
  # ── No Docker container: look for native bitcoind ────────────────────────
  # IMPORTANT: pool runs in Docker → can't reach host's 127.0.0.1 directly.
  # Use the Docker bridge gateway (host gateway) so the pool container can
  # reach services running on the host machine.
  #   Linux:  Docker bridge gateway = 172.17.0.1  (or ip route default)
  #   macOS:  host.docker.internal  (Docker Desktop resolves this)
  _host_gw() {
    if [ "$(uname -s)" = "Darwin" ]; then
      echo "host.docker.internal"
    else
      # Get default gateway IP — reachable from any Docker container
      local gw
      gw=$(ip route show default 2>/dev/null | awk 'NR==1 { print $3; exit }' | tr -d '\r' | head -1 || true)
      if [ -n "$gw" ]; then
        echo "$gw"
        return 0
      fi
      gw=$(docker network inspect bridge \
             --format '{{range .IPAM.Config}}{{.Gateway}}{{end}}' 2>/dev/null \
             | tr -d '\r' | awk 'NF { print; exit }' || true)
      if [ -n "$gw" ]; then
        echo "$gw"
        return 0
      fi
      echo "172.17.0.1"
    fi
  }

  _first_line() {
    printf '%s\n' "$1" | tr -d '\r' | awk 'NF { print; exit }'
  }

  for dir in \
      "$HOME/.bitcoin" \
      "$HOME/Library/Application Support/Bitcoin" \
      "/var/lib/bitcoind" "/var/lib/bitcoin" \
      "/etc/bitcoin" "/opt/bitcoin" \
      "/usr/local/var/bitcoin"; do
    if [ -f "$dir/bitcoin.conf" ]; then
      BTC_CONF="$dir/bitcoin.conf"
      # Use host gateway so pool Docker container can reach native bitcoind
      BTC_IP=$(_first_line "$(_host_gw)")
      ok "Found native bitcoin.conf: $BTC_CONF"
      ok "  Host gateway: ${BLD}$BTC_IP${RST}  (reachable from pool container)"
      # Parse conf
      while IFS='=' read -r k v; do
        k=$(echo "$k" | sed 's/#.*//;s/[[:space:]]//g'); v=$(echo "$v" | sed 's/#.*//;s/[[:space:]\r]//g')
        [ -z "$k" ] && continue
        case "$k" in
          rpcuser)        [ -z "$BTC_USER" ] && BTC_USER="$v" ;;
          rpcpassword)    [ -z "$BTC_PASS" ] && BTC_PASS="$v" ;;
          rpcport)        BTC_PORT="$v" ;;
          testnet)        [ "$v" = "1" ] && BTC_NETWORK="testnet" ;;
          regtest)        [ "$v" = "1" ] && BTC_NETWORK="regtest" ;;
          signet)         [ "$v" = "1" ] && BTC_NETWORK="signet" ;;
          zmqpubhashblock) ZMQ_BLOCKS_URL="${v//0.0.0.0/$BTC_IP}" ;;
          zmqpubrawblock)  [ -z "$ZMQ_BLOCKS_URL" ] && ZMQ_BLOCKS_URL="${v//0.0.0.0/$BTC_IP}" ;;
          zmqpubhashtx)    ZMQ_TXS_URL="${v//0.0.0.0/$BTC_IP}" ;;
          zmqpubrawtx)     [ -z "$ZMQ_TXS_URL" ] && ZMQ_TXS_URL="${v//0.0.0.0/$BTC_IP}" ;;
        esac
      done < "$BTC_CONF"
      break
    fi
  done
fi

# ── Interactive fallback (only if auto-detection completely failed) ───────────
if [ -z "$BTC_IP" ]; then
  warn "Could not auto-detect Bitcoin Core."
  if $UNATTENDED; then err "Set BTC node details in env/.env manually."; BTC_IP="127.0.0.1";
  else ask "Bitcoin Core IP/hostname:"; read -r BTC_IP; fi
fi
if [ -z "$BTC_USER" ]; then
  if $UNATTENDED; then BTC_USER="bitcoin";
  else ask "RPC username:"; read -r BTC_USER; fi
fi
if [ -z "$BTC_PASS" ]; then
  if $UNATTENDED; then BTC_PASS="CHANGE_ME_IN_ENV";
  else ask "RPC password (hidden):"; read -r -s BTC_PASS; echo ""; fi
fi

# ── Build ZMQ URLs AFTER we have BTC_IP (including user input above) ─────────
[ -z "$ZMQ_BLOCKS_URL" ] && ZMQ_BLOCKS_URL="tcp://${BTC_IP}:${ZMQ_BLOCK_PORT}"
[ -z "$ZMQ_TXS_URL"    ] && ZMQ_TXS_URL="tcp://${BTC_IP}:${ZMQ_TX_PORT}"
# Secondary ZMQ endpoints (rawblock / rawtx)
ZMQ_BLOCKS_URL2="${ZMQ_BLOCKS_URL2:-tcp://${BTC_IP}:${ZMQ_RAWBLOCK_PORT:-28332}}"
ZMQ_TXS_URL2="${ZMQ_TXS_URL2:-tcp://${BTC_IP}:${ZMQ_RAWTX_PORT:-28333}}"

RPC_URL="http://${BTC_IP}:${BTC_PORT}"
echo ""
ok "Bitcoin Core: ${BLD}$RPC_URL${RST}"
ok "Network:      ${BLD}$BTC_NETWORK${RST}"
ok "ZMQ blocks:   ${BLD}$ZMQ_BLOCKS_URL${RST}"

# ════════════════════════════════════════════════════════════════════════════
# PHASE 4 — Verify RPC Connection
# ════════════════════════════════════════════════════════════════════════════
sep; info "Phase 4 — Testing RPC connection"

rpc_test=$(curl -sf --max-time 8 \
  -u "${BTC_USER}:${BTC_PASS}" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"1.0","id":"bh","method":"getblockchaininfo","params":[]}' \
  "${RPC_URL}/" 2>/dev/null || echo "FAIL")

if echo "$rpc_test" | grep -q '"chain"'; then
  BTC_CHAIN=$(echo "$rpc_test" | grep -o '"chain":"[^"]*"' | cut -d'"' -f4)
  BTC_HEIGHT=$(echo "$rpc_test" | grep -o '"blocks":[0-9]*' | cut -d: -f2)
  BTC_SYNC=$(echo "$rpc_test" | grep -o '"verificationprogress":[0-9.]*' | cut -d: -f2)
  SYNC_PCT=$(python3 -c "print(f'{float(\"${BTC_SYNC}\")*100:.2f}')" 2>/dev/null \
            || awk "BEGIN{printf \"%.2f\", ${BTC_SYNC:-0}*100}" 2>/dev/null || echo "?")
  ok "RPC OK — chain=${BTC_CHAIN} height=${BTC_HEIGHT} sync=${SYNC_PCT}%"
else
  warn "RPC connection failed (will retry at pool startup)."
  warn "Check BTC_IP, RPC_USER, RPC_PASS in env/.env after setup."
fi

# ════════════════════════════════════════════════════════════════════════════
# PHASE 5 — ZMQ Setup
# ════════════════════════════════════════════════════════════════════════════
sep; info "Phase 5 — Checking ZMQ"

ZMQ_OK=false
[ -n "$ZMQ_BLOCKS_URL" ] && [ -n "$ZMQ_TXS_URL" ] && ZMQ_OK=true

if $ZMQ_OK; then
  ok "ZMQ blocks: $ZMQ_BLOCKS_URL"
  ok "ZMQ txs:    $ZMQ_TXS_URL"
else
  # Build default ZMQ URLs
  ZMQ_BLOCKS_URL="tcp://${BTC_IP}:${ZMQ_BLOCK_PORT}"
  ZMQ_TXS_URL="tcp://${BTC_IP}:${ZMQ_TX_PORT}"

  warn "ZMQ not configured in bitcoin.conf."
  warn "ZMQ enables <100ms block detection vs 30s polling."

  if ! $UNATTENDED && [ -n "$BTC_CONF" ] && [ -f "$BTC_CONF" ]; then
    echo ""
    ask "Add ZMQ to bitcoin.conf and restart Bitcoin Core? [y/N]"
    read -r ans
    if [[ "$(echo "$ans" | tr '[:upper:]' '[:lower:]')" == y* ]]; then
      {
        printf '\n# ZMQ — added by BlockFinder installer %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'zmqpubhashblock=tcp://0.0.0.0:%s\n' "$ZMQ_BLOCK_PORT"
        printf 'zmqpubrawblock=tcp://0.0.0.0:%s\n'  "$ZMQ_BLOCK_PORT"
        printf 'zmqpubhashtx=tcp://0.0.0.0:%s\n'    "$ZMQ_TX_PORT"
        printf 'zmqpubrawtx=tcp://0.0.0.0:%s\n'     "$ZMQ_TX_PORT"
      } >> "$BTC_CONF"
      ok "ZMQ lines added to $BTC_CONF"

      if [ -n "$BTC_CONTAINER" ]; then
        info "Restarting Bitcoin Core container ($BTC_CONTAINER)…"
        docker restart "$BTC_CONTAINER"
        info "Waiting 40s for Bitcoin Core to come back up…"
        sleep 40
        ok "Bitcoin Core restarted"
      else
        warn "Please restart Bitcoin Core manually, then press ENTER."
        read -r _
      fi
    else
      warn "ZMQ skipped — pool will fall back to 30s polling."
    fi
  else
    warn "ZMQ skipped — add to bitcoin.conf manually for best performance."
  fi
fi

# ════════════════════════════════════════════════════════════════════════════
# PHASE 6 — Payout Address
# ════════════════════════════════════════════════════════════════════════════
sep; info "Phase 6 — Payout address"

PAYOUT_ADDR=""

# Try to find existing env
[ -f "env/.env" ] && \
  PAYOUT_ADDR=$(grep '^PAYOUT_ADDRESS=' "env/.env" 2>/dev/null | cut -d= -f2 | tr -d "'" || true)

if [ -z "$PAYOUT_ADDR" ] || [[ "$PAYOUT_ADDR" == *"YOUR_BITCOIN"* ]]; then
  if $UNATTENDED; then
    PAYOUT_ADDR="CHANGE_ME_YOUR_BITCOIN_ADDRESS"
    warn "Payout address not set — update PAYOUT_ADDRESS in env/.env"
  else
    echo ""
    echo "  Your Bitcoin address is used as the fallback payout when a miner"
    echo "  does not provide an address in their username."
    ask "Your Bitcoin payout address (mainnet bc1q... / 1... / 3...):"
    read -r PAYOUT_ADDR
    [ -z "$PAYOUT_ADDR" ] && PAYOUT_ADDR="CHANGE_ME_YOUR_BITCOIN_ADDRESS"
  fi
fi

ok "Payout address: ${BLD}$PAYOUT_ADDR${RST}"

PROJECT_NAME="$(basename "$SCRIPT_DIR" | tr '[:upper:]' '[:lower:]' | tr -cd 'a-z0-9')"
COMPOSE_PROJECT_ARGS=(-p "$PROJECT_NAME")

# ════════════════════════════════════════════════════════════════════════════
# PHASE 7 — Generate env/.env
# ════════════════════════════════════════════════════════════════════════════
sep; info "Phase 7 — Writing env/.env"

mkdir -p env

# Backup existing .env
[ -f "env/.env" ] && cp "env/.env" "env/.env.backup.$(date +%Y%m%d_%H%M%S)"

cat > "env/.env" << ENVEOF
# ═══════════════════════════════════════════════════════════════════════════
#  BlockFinder Pool — Runtime Configuration
#  Generated by setup-blackhole.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)
#  Edit this file to change pool settings, then restart: bash run.sh
# ═══════════════════════════════════════════════════════════════════════════

# ── Bitcoin Core ─────────────────────────────────────────────────────────────
RPC_URL=${RPC_URL}
RPC_USER=${BTC_USER}
RPC_PASS=${BTC_PASS}

# ── ZMQ (low-latency block/tx notifications) ─────────────────────────────────
# Both hashblock+rawblock endpoints for maximum reliability (dual ZMQ)
ZMQ_BLOCKS=${ZMQ_BLOCKS_URL},${ZMQ_BLOCKS_URL2}
ZMQ_TXS=${ZMQ_TXS_URL},${ZMQ_TXS_URL2}

# ── Network ──────────────────────────────────────────────────────────────────
BITCOIN_NETWORK=${BTC_NETWORK}

# ── Payout address (fallback when miner does not provide one) ─────────────────
PAYOUT_ADDRESS=${PAYOUT_ADDR}

# ── Stratum ───────────────────────────────────────────────────────────────────
STRATUM_BIND=0.0.0.0
STRATUM_PORT=3333

# ── API ───────────────────────────────────────────────────────────────────────
API_BIND=0.0.0.0
API_PORT=8080

# ── Logging ───────────────────────────────────────────────────────────────────
RUST_LOG=info

# ── Persistence ───────────────────────────────────────────────────────────────
DATABASE_URL=sqlite:///data/pool.db?mode=rwc
PERSIST_BLOCKS=true
PERSIST_SHARES=false

# ── Vardiff ────────────────────────────────────────────────────────────────────
VARDIFF_ENABLED=true
TARGET_SHARE_TIME_SECS=20
VARDIFF_RETARGET_SECS=45
MIN_DIFFICULTY=8192
MAX_DIFFICULTY=262144
STRATUM_START_DIFFICULTY=32768

# ── Job timing (optimised — see .env.example for tuning guide) ───────────────
# Wire-measured optimal values for Umbrel Home / low-power hardware:
#   ZMQ_DEBOUNCE_MS=1500 → ~44 GBT calls/min, 9s avg job interval per miner
#   POST_BLOCK_SUPPRESS_MS=12000 → absorbs post-block mempool burst
ZMQ_DEBOUNCE_MS=1500
POST_BLOCK_SUPPRESS_MS=12000
NOTIFY_BUCKET_CAPACITY=2
NOTIFY_BUCKET_REFILL_MS=600
JOB_REFRESH_MS=30000
TEMPLATE_POLL_MS=30000

# ── Coinbase tag (visible on-chain if you find a block) ──────────────────────
POOL_TAG=/BlockFinder Solo Pool
COINBASE_MESSAGE=BlockFinder

# ── Authentication ────────────────────────────────────────────────────────────
# Set AUTH_TOKEN to require miners to use a password.
AUTH_TOKEN=
REQUIRE_AUTH_TOKEN=false
ENVEOF

ok "env/.env written"

# ════════════════════════════════════════════════════════════════════════════
# PHASE 8 — Docker Compose Configuration
# ════════════════════════════════════════════════════════════════════════════
sep; info "Phase 8 — Preparing Docker Compose"

if [ "$PLATFORM" = "umbrel" ]; then
  # Ensure the umbrel_main_network exists
  if ! docker network inspect umbrel_main_network &>/dev/null 2>&1; then
    warn "umbrel_main_network not found — creating it"
    docker network create umbrel_main_network || true
  fi

  # Ensure the volume exists
  if ! docker volume inspect blackhole_pool_data &>/dev/null 2>&1; then
    docker volume create blackhole_pool_data
    ok "Created Docker volume: blackhole_pool_data"
  fi

  ok "Using Umbrel compose (umbrel_main_network + external volume)"
else
  # Standalone: ensure docker-compose.standalone.yml exists
  if [ ! -f "docker-compose.standalone.yml" ]; then
    err "docker-compose.standalone.yml not found."
    err "Make sure you cloned/extracted the full BlockFinder repo."
    exit 1
  fi

  rm -f docker-compose.network-override.yml

  ok "Using standalone compose (blackhole_net + local volume)"
fi

# ════════════════════════════════════════════════════════════════════════════
# PHASE 9 — Build with Git Provenance
# ════════════════════════════════════════════════════════════════════════════
sep; info "Phase 9 — Building BlockFinder"

BUILD_SHA="unknown"
BUILD_DIRTY="unknown"
BUILD_TIME="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

if git rev-parse --git-dir &>/dev/null 2>&1; then
  BUILD_SHA="$(git rev-parse --verify HEAD 2>/dev/null || echo unknown)"
  BUILD_DIRTY="$([ -n "$(git status --porcelain 2>/dev/null)" ] && echo true || echo false)"
  ok "Git SHA: ${BUILD_SHA:0:12} (dirty=$BUILD_DIRTY)"
else
  warn "Not a git repo — build provenance will show 'unknown'"
fi

BUILD_ARGS=(
  "BUILD_GIT_SHA=$BUILD_SHA"
  "BUILD_GIT_DIRTY=$BUILD_DIRTY"
  "BUILD_SOURCE=setup-blackhole.sh"
  "BUILD_TIME_UTC=$BUILD_TIME"
)

info "Building pool image (this may take a few minutes on first run)…"
env "${BUILD_ARGS[@]}" "${DC[@]}" "${COMPOSE_PROJECT_ARGS[@]}" "${COMPOSE_FILES[@]}" build blackhole-pool 2>&1

info "Building dashboard image…"
env "${BUILD_ARGS[@]}" "${DC[@]}" "${COMPOSE_PROJECT_ARGS[@]}" "${COMPOSE_FILES[@]}" build blackhole-dashboard 2>&1

ok "Build complete"

# ════════════════════════════════════════════════════════════════════════════
# PHASE 10 — Start Services
# ════════════════════════════════════════════════════════════════════════════
sep; info "Phase 10 — Starting BlockFinder"

IMAGE_ID=$(docker image inspect "${PROJECT_NAME}_blackhole-pool:latest" \
  --format '{{.Id}}' 2>/dev/null || echo unknown)

# Docker Compose v1 can get stuck trying to recreate old containers whose
# metadata no longer matches the rebuilt image. Remove every container that
# still belongs to this project before bringing services back up.
EXISTING_CONTAINERS=$(
  {
    docker ps -aq \
      --filter "label=com.docker.compose.project=${PROJECT_NAME}" \
      2>/dev/null || true
    docker ps -aq \
      --filter "name=${PROJECT_NAME}_" \
      2>/dev/null || true
  } | awk 'NF && !seen[$0]++'
)
if [ -n "$EXISTING_CONTAINERS" ]; then
  echo "$EXISTING_CONTAINERS" | xargs -r docker rm -f >/dev/null 2>&1 || true
fi

( export BUILD_GIT_SHA="$BUILD_SHA" BUILD_GIT_DIRTY="$BUILD_DIRTY" \
         BUILD_SOURCE="setup-blackhole.sh" BUILD_TIME_UTC="$BUILD_TIME" \
         RUNTIME_IMAGE_ID="$IMAGE_ID" \
         RUNTIME_IMAGE_REF="${PROJECT_NAME}_blackhole-pool:latest" \
         RUNTIME_CONTAINER_NAME="${PROJECT_NAME}_blackhole-pool_1"; \
  "${DC[@]}" "${COMPOSE_PROJECT_ARGS[@]}" "${COMPOSE_FILES[@]}" up -d --remove-orphans )

ok "Containers started"

# ════════════════════════════════════════════════════════════════════════════
# PHASE 11 — Health Verification
# ════════════════════════════════════════════════════════════════════════════
sep; info "Phase 11 — Verifying health"

# Cross-platform IP: Linux uses hostname -I, macOS uses ipconfig/ifconfig
HOST_IP=$(hostname -I 2>/dev/null | awk '{print $1}')
[ -z "$HOST_IP" ] && HOST_IP=$(ipconfig getifaddr en0 2>/dev/null || true)
[ -z "$HOST_IP" ] && HOST_IP=$(ipconfig getifaddr en1 2>/dev/null || true)
[ -z "$HOST_IP" ] && HOST_IP=$(ifconfig 2>/dev/null \
  | awk '/inet /{if($2!="127.0.0.1"){print $2;exit}}')
HOST_IP="${HOST_IP:-localhost}"
if [ "$PLATFORM" = "umbrel" ]; then
  API="http://localhost:8081"
  API_HOST_PORT=8081
else
  API="http://localhost:8080"
  API_HOST_PORT=8080
fi

info "Waiting for pool API to become ready…"
for i in $(seq 1 120); do
  if curl -sf --max-time 3 "$API/health" &>/dev/null; then
    ok "API healthy (${i}×2s = $((i*2))s)"
    break
  fi
  [ "$i" -eq 120 ] && { err "Pool API did not respond within 240s."; }
  sleep 2
done

# Health report
echo ""
sep
echo -e "${BLD}  Health Report${RST}"
sep

# Container status
echo ""
echo -e "${BLD}Containers:${RST}"
docker ps --filter "name=blackhole" \
  --format "  {{.Names}}\t{{.Status}}" 2>/dev/null | column -t

# API checks
echo ""
echo -e "${BLD}API checks:${RST}"

check_endpoint() {
  local label="$1" url="$2"
  local res
  res=$(curl -sf --max-time 5 "$url" 2>/dev/null || echo "FAIL")
  if [ "$res" = "FAIL" ]; then
    echo -e "  ${RED}✗${RST} $label — not reachable"
  else
    echo -e "  ${GRN}✓${RST} $label"
    echo "$res"
  fi
}

pool_json=$(curl -sf --max-time 5 "$API/pool" 2>/dev/null || echo "{}")
if echo "$pool_json" | grep -q '"totalMiners"'; then
  miners=$(echo "$pool_json" | grep -o '"totalMiners":[0-9]*' | cut -d: -f2)
  height=$(echo "$pool_json" | grep -o '"blockHeight":[0-9]*' | cut -d: -f2)
  hashrate=$(echo "$pool_json" | grep -o '"totalHashRate":[0-9.e+]*' | cut -d: -f2)
  echo -e "  ${GRN}✓${RST} /pool   — miners=${miners} height=${height}"
else
  echo -e "  ${YEL}!${RST} /pool   — pool starting up"
fi

conn_json=$(curl -sf --max-time 5 "$API/blockfinder/connection-status" 2>/dev/null || echo "{}")
rpc_ok=$(echo "$conn_json" | grep -o '"connected":true' | head -1)
zmq_ok=$(echo "$conn_json" | grep '"zmq"' | grep -c 'enabled.*true' || true)

[ -n "$rpc_ok" ] && \
  echo -e "  ${GRN}✓${RST} Bitcoin Core RPC — connected" || \
  echo -e "  ${YEL}!${RST} Bitcoin Core RPC — check RPC_URL / RPC_USER / RPC_PASS in env/.env"

[ "$zmq_ok" -gt 0 ] && \
  echo -e "  ${GRN}✓${RST} ZMQ — enabled" || \
  echo -e "  ${YEL}!${RST} ZMQ — check ZMQ_BLOCKS in env/.env"

build_json=$(curl -sf --max-time 5 "$API/build-info" 2>/dev/null || echo "{}")
build_sha_live=$(echo "$build_json" | grep -o '"git_sha":"[^"]*"' | cut -d'"' -f4)
[ -n "$build_sha_live" ] && [ "$build_sha_live" != "unknown" ] && \
  echo -e "  ${GRN}✓${RST} Build provenance — ${build_sha_live:0:12}"

# ════════════════════════════════════════════════════════════════════════════
# DONE
# ════════════════════════════════════════════════════════════════════════════
sep
echo ""
echo -e "${BLD}${GRN}  ✓ BlockFinder Pool is running!${RST}"
echo ""
echo -e "  ${BLD}Dashboard${RST}  →  http://${HOST_IP}:3334"
echo -e "  ${BLD}API${RST}        →  http://${HOST_IP}:${API_HOST_PORT}/pool"
echo -e "  ${BLD}Stratum${RST}    →  ${HOST_IP}:3333"
echo ""
echo -e "  ${DIM}Configure your miner:${RST}"
echo -e "  ${DIM}  URL:      stratum+tcp://${HOST_IP}:3333${RST}"
echo -e "  ${DIM}  Username: YOUR_BITCOIN_ADDRESS (e.g. bc1q...)${RST}"
echo -e "  ${DIM}  Password: x  (or leave blank)${RST}"
echo ""
echo -e "  ${DIM}To update settings: edit env/.env then run  bash run.sh${RST}"
echo -e "  ${DIM}To reset database:  bash reset-pool.sh${RST}"
echo -e "  ${DIM}To rebuild:         bash redeploy-with-provenance.sh${RST}"
echo ""
sep
