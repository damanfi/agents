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
# Per BRIEF_PERSONA_AS_FORAGER, each persona is its own self-contained forager
# process and owns its single EOA private key. There is no shared keyring; the
# launcher passes --key-path to each spawned persona, pointing at its own keyfile
# under ~/.config/hum/daman-personas/<bee_name>.key. Process boundary IS identity
# boundary. Mirrors humfs's per-instance fs.roots scoping pattern.
#
# Required env (sourced from ~/.config/hum/daman-swarm.env or process env):
#   HUM_THRUM_SOCK         humd's NDJSON socket
#   DAMAN_PERSONA_KEY_DIR  per-bee keyfile dir (default ~/.config/hum/daman-personas)
#   DAMAN_PERSONA_LOG_ROOT log root dir; per-bee subdirs created (default ./logs)
#
# Spawns persona processes with `&`. For production use pm2 or systemd; see
# scripts/launch-swarm.systemd.sh for the systemd-unit-generator variant.

set -euo pipefail

MODE="${1:-full}"
LOG_ROOT="${DAMAN_PERSONA_LOG_ROOT:-./logs}"
KEY_DIR="${DAMAN_PERSONA_KEY_DIR:-$HOME/.config/hum/daman-personas}"

mkdir -p "$LOG_ROOT"

if [[ ! -d "$KEY_DIR" ]]; then
  echo "error: keyfile dir not found at $KEY_DIR" >&2
  echo "       run scripts/mint-persona-keys.sh first" >&2
  exit 2
fi

spawn_one() {
  local role="$1"
  local variant="$2"
  local bee_name="$3"
  local keyfile="$KEY_DIR/${bee_name}.key"
  if [[ ! -r "$keyfile" ]]; then
    echo "error: no keyfile for $bee_name at $keyfile" >&2
    return 1
  fi
  local addr
  addr=$(cast wallet address --private-key "0x$(cat "$keyfile")")
  local logdir="$LOG_ROOT/$bee_name"
  mkdir -p "$logdir"
  echo "spawning $bee_name (role=$role variant=$variant addr=$addr)"
  RUST_LOG="${RUST_LOG:-info}" \
    daman-persona \
      --role "$role" \
      --variant "$variant" \
      --bee-name "$bee_name" \
      --eoa-addr "$addr" \
      --key-path "$keyfile" \
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
