#!/bin/bash
# Start the MDK demo: fake pools, the MDK stack, the demo user, the dashboard.
set -eu
cd /app/mdk
USER_NAME=${MDK_DEMO_USER:?MDK_DEMO_USER is not set: the dashboard sign-in, e.g. demo@example.com}
PASSWORD=${MDK_DEMO_PASSWORD:?MDK_DEMO_PASSWORD is not set: the dashboard password (12+ characters)}

node scripts/fake-pool.js --host 0.0.0.0 --port 3340 --name home &
node scripts/fake-pool.js --host 0.0.0.0 --port 3341 --name redeemer &

# Fresh Kernel state on every start: the demo keeps nothing.
rm -rf .mdk
mdk run &
MDK_PID=$!

i=0
up() { node -e "require('http').get('http://127.0.0.1:3847/auth/me', () => process.exit(0)).on('error', () => process.exit(1))"; }
until up; do
  i=$((i + 1))
  if [ "$i" -gt 120 ] || ! kill -0 "$MDK_PID" 2>/dev/null; then
    echo "mdk run did not come up" >&2
    exit 1
  fi
  sleep 1
done

echo "$PASSWORD" | mdk user add "$USER_NAME" --admin --password-stdin || true

node /app/deploy/mdk/serve.js &

# Exit (and let compose restart us) if any piece dies.
wait -n
