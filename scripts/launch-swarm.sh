#!/usr/bin/env bash
# launch-swarm.sh — spawn the Daman persona swarm against a local humd + claude-cli hive.
#
# Defaults to the full 27-persona constellation per BRIEF_OPERATION_MULTI_EOA_SWARM:
#   5 leaders   (alpha bravo charlie delta echo)
#   15 followers (3 variants x 5 instances)
#   3 watchdogs (2 variant-v1 + 1 variant-v2)
#   2 arbiters  (v1 + v2)
#   2 relief
#
# Use `--smoke` to spawn just the 4-persona smoke subset (1 leader + 1 follower +
# 1 watchdog + 1 arbiter) per the brief's pre-scale gate.
#
# Per-persona keyring entries are loaded by daman-arc-fs at boot from
# ~/.config/hum/daman-arc-fs/keyring.json. This launcher does NOT mint EOAs; that lives
# in scripts/mint-persona-keys.sh which the operator runs once.
#
# Required env (sourced from ~/.config/hum/daman-swarm.env or process env):
#   HUM_THRUM_SOCK         humd's NDJSON socket
#   DAMAN_ARC_FS_KEYRING   keyring path (default ~/.config/hum/daman-arc-fs/keyring.json)
#   DAMAN_PERSONA_LOG_ROOT log root dir; per-bee subdirs created (default ./logs)
#
# Spawns persona processes with `&`. For production use pm2 or systemd; see
# scripts/launch-swarm.systemd.sh for the systemd-unit-generator variant.

set -euo pipefail

MODE="${1:-full}"
LOG_ROOT="${DAMAN_PERSONA_LOG_ROOT:-./logs}"
KEYRING="${DAMAN_ARC_FS_KEYRING:-$HOME/.config/hum/daman-arc-fs/keyring.json}"

mkdir -p "$LOG_ROOT"

if [[ ! -r "$KEYRING" ]]; then
  echo "error: keyring not readable at $KEYRING" >&2
  echo "       run scripts/mint-persona-keys.sh first" >&2
  exit 2
fi

# Read EOA for each bee from the keyring (which is { "bee_name": "0x<priv>" }).
# We need the address per bee for the system prompt. derive_addr_from_key() uses cast.
# Operator can override via DAMAN_PERSONA_ADDR_<bee_name_uppercase_with_underscores> env.

derive_addr_from_key() {
  local bee="$1"
  # Allow direct override.
  local env_var="DAMAN_PERSONA_ADDR_$(echo "$bee" | tr '[:lower:]-' '[:upper:]_')"
  local override="${!env_var:-}"
  if [[ -n "$override" ]]; then echo "$override"; return; fi

  local key
  key=$(jq -r --arg b "$bee" '.[$b] // empty' "$KEYRING")
  if [[ -z "$key" ]]; then
    echo "error: no keyring entry for bee `$bee`" >&2
    exit 2
  fi
  cast wallet address --private-key "$key"
}

spawn_one() {
  local role="$1"
  local variant="$2"
  local bee_name="$3"
  local addr
  addr=$(derive_addr_from_key "$bee_name")
  local logdir="$LOG_ROOT/$bee_name"
  mkdir -p "$logdir"
  echo "spawning $bee_name (role=$role variant=$variant addr=$addr)"
  RUST_LOG="${RUST_LOG:-info}" \
    daman-persona \
      --role "$role" \
      --variant "$variant" \
      --bee-name "$bee_name" \
      --eoa-addr "$addr" \
      --log-dir "$logdir" \
      > "$logdir/stdout.log" 2> "$logdir/stderr.log" &
}

# -----------------------------------------------------------------------
# Smoke: 4 personas
# -----------------------------------------------------------------------
if [[ "$MODE" == "--smoke" ]]; then
  spawn_one leader alpha daman-leader-alpha
  spawn_one follower v1 daman-follower-v1-1
  spawn_one watchdog v1 daman-watchdog-v1-1
  spawn_one arbiter v1 daman-arbiter-v1
  echo "smoke swarm spawned (4 personas). tailing logs in $LOG_ROOT/"
  wait
  exit 0
fi

# -----------------------------------------------------------------------
# Full: 27 personas
# -----------------------------------------------------------------------

# leaders (5)
for variant in alpha bravo charlie delta echo; do
  spawn_one leader "$variant" "daman-leader-$variant"
done

# followers (15: 5 per variant)
for variant in 1 2 3; do
  for i in 1 2 3 4 5; do
    spawn_one follower "v$variant" "daman-follower-v${variant}-${i}"
  done
done

# watchdogs (3)
for i in 1 2; do
  spawn_one watchdog v1 "daman-watchdog-v1-${i}"
done
spawn_one watchdog v2 "daman-watchdog-v2"

# arbiters (2)
spawn_one arbiter v1 "daman-arbiter-v1"
spawn_one arbiter v2 "daman-arbiter-v2"

# relief (2)
for i in 1 2; do
  spawn_one relief mechanical "daman-relief-${i}"
done

echo ""
echo "full swarm spawned (27 personas). logs in $LOG_ROOT/"
echo "Ctrl-C to terminate all. dashboard: https://damanfi.github.io/app/"
wait
