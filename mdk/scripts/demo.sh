#!/usr/bin/env bash
# Hashu Heater: the whole demo on mock hardware, in one command.
#
#   ./scripts/demo.sh            # then open http://localhost:3030 (demo@hashu.io / hashu-demo-2026)
#
# Starts, all on 127.0.0.1:
#   fake pools   :3340 "home" and :3341 "redeemer" (accept every share)
#   hashu mint   :8085 Cashu mint (fake Lightning), :3333 stratum, :3334 control API
#   MDK stack    Kernel, the heater Worker with its mock heater, the Hashu app, Gateway :3847
#   dashboard    :3030
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
LOGS="$ROOT/.mdk/demo-logs"
mkdir -p "$LOGS"
USER_NAME=${DEMO_USER:-demo@hashu.io}
PASSWORD=${DEMO_PASSWORD:-hashu-demo-2026}

command -v mdk >/dev/null || { echo "Install the MDK CLI first: npm i -g @tetherto/mdk-cli@0.13.1 (Node >= 24)"; exit 1; }

HASHU=${HASHU_BIN:-$ROOT/../target/release/hashu}
if [ ! -x "$HASHU" ]; then
  echo "Building the hashu binary (cargo build --release -p hashu-cli)…"
  (cd "$ROOT/.." && cargo build --release -p hashu-cli)
fi

[ -d node_modules ] || npm install
[ -f apps/hashu/ui/index.js ] || (cd apps/hashu && npm --prefix ui-src install && npm run build:ui)
[ -d apps/dashboard ] || mdk create dashboard

pids=()
cleanup() { echo; echo "Stopping…"; for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; }
trap cleanup EXIT INT TERM

node scripts/fake-pool.js --port 3340 --name home >"$LOGS/pool-home.log" 2>&1 & pids+=($!)
node scripts/fake-pool.js --port 3341 --name redeemer >"$LOGS/pool-redeemer.log" 2>&1 & pids+=($!)
RUST_LOG=${RUST_LOG:-info} "$HASHU" mint run \
  --listen 127.0.0.1:8085 --data-dir "$LOGS/mint-data" \
  --lightning fake --price fixed --sats-per-thh 40 \
  --stratum-bind 127.0.0.1:3333 --upstream 127.0.0.1:3340 \
  --control-bind 127.0.0.1:3334 --control-allow-open \
  --metrics-interval 0 >"$LOGS/proxy.log" 2>&1 & pids+=($!)
echo "$!" >"$LOGS/proxy.pid"

mdk run >"$LOGS/mdk.log" 2>&1 & pids+=($!)
echo -n "Waiting for the MDK Gateway"
for _ in $(seq 1 90); do
  grep -q "Stack running" "$LOGS/mdk.log" 2>/dev/null && break
  echo -n "."; sleep 1
done
echo
grep -q "Stack running" "$LOGS/mdk.log" || { echo "mdk run did not start; see $LOGS/mdk.log"; tail -20 "$LOGS/mdk.log"; exit 1; }

echo "$PASSWORD" | mdk user add "$USER_NAME" --admin --password-stdin 2>&1 | grep -v "^ " || true

mdk run dashboard >"$LOGS/dashboard.log" 2>&1 & pids+=($!)

cat <<MSG

Hashu Heater demo is running.

  Dashboard   http://localhost:3030  (sign in as $USER_NAME / $PASSWORD, open "Hashu Heater")
  App API     http://localhost:3847/apps/hashu/api/hashu/state  (Bearer token from POST /auth/login)
  Hashu       http://127.0.0.1:3334/status  (mint: http://127.0.0.1:8085/v1/info)
  Logs        $LOGS

Try it:
  1. Press "Redeem" (1 THH). The heater switches to the Hashu proxy, its shares go to the
     redeemer's pool, and it comes back to the home pool when the 1 THH is filled (~90 s).
  2. Raise or lower the target temperature and watch the heater sleep or heat.
  3. Failure case: kill the proxy (kill \$(cat $LOGS/proxy.pid)). The app reports Hashu unreachable
     and moves the heater back to the home pool.

Ctrl+C stops everything.
MSG
wait
