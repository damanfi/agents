#!/usr/bin/env bash
# Container entrypoint: start humd, wait for the thrum socket, then
# start daman-watchdog. Both processes share the container; humd is
# backgrounded so the watchdog stays foreground for docker's log
# attribution.
set -euo pipefail

mkdir -p /run/hum/hum /state/hum /config/hum

# Keys + peers.json are mounted into the container via the compose file.
# humd reads the ed25519 key at $XDG_STATE_HOME/hum/humd.key and peers
# at $XDG_CONFIG_HOME/hum/peers.json.
if [ -f /keys/humd.key ]; then
  cp /keys/humd.key /state/hum/humd.key
  chmod 600 /state/hum/humd.key
fi
if [ -f /keys/peers.json ]; then
  cp /keys/peers.json /config/hum/peers.json
fi

# Boot humd in background. It binds /run/hum/hum/thrum.sock per the
# XDG_RUNTIME_DIR convention.
humd &
HUMD_PID=$!

# Wait for the socket to appear. humd creates it as part of startup.
for i in $(seq 1 50); do
  if [ -S "$HUM_THRUM_SOCK" ]; then
    break
  fi
  sleep 0.2
done

if [ ! -S "$HUM_THRUM_SOCK" ]; then
  echo "thrum socket did not appear at $HUM_THRUM_SOCK" >&2
  kill $HUMD_PID || true
  exit 1
fi

# Run the watchdog in the foreground. When humd dies (signal, crash),
# we terminate ourselves so docker restarts the whole container.
trap "kill $HUMD_PID 2>/dev/null || true" EXIT

exec /usr/local/bin/daman-watchdog
