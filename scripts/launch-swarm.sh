#!/usr/bin/env bash
# launch-swarm.sh — register the Daman persona swarm with hum, then drive lifecycle
# through `hum bee` verbs. Each persona becomes a per-bee user service (`hum-daman-
# persona-<bee>`) via the daman-personas install script which wraps hum's svc.sh
# helper. After install, `hum bee <bee> enter|exit|reenter` is the supported way to
# manage individual bees; this launcher fans those verbs across the swarm.
#
# Per BRIEF_PERSONA_AS_FORAGER: each persona is its own forager process holding one
# EOA private key, one stable ed25519 hid, one namespaced tool surface. Process
# boundary = identity boundary, mirroring humfs's per-instance fs.roots scoping.
#
# Commands:
#   launch-swarm.sh                   # install + start the full 27-persona swarm
#   launch-swarm.sh --smoke           # install + start the 4-persona smoke subset
#   launch-swarm.sh --up              # `hum bee <bee> enter` for each
#   launch-swarm.sh --up --smoke
#   launch-swarm.sh --down            # `hum bee <bee> exit` for each
#   launch-swarm.sh --down --smoke
#   launch-swarm.sh --restart         # `hum bee <bee> reenter` for each
#   launch-swarm.sh --restart --smoke
#   launch-swarm.sh --uninstall       # tear down service units
#   launch-swarm.sh --uninstall --smoke
#   launch-swarm.sh --status          # `hum bee --list | grep daman-`
#
# Env:
#   DAMAN_PERSONA_KEY_DIR  per-bee keyfile dir (default ~/.config/hum/daman-personas)

set -euo pipefail

MODE="${1:-install}"
SMOKE=0
for arg in "$@"; do
  if [ "$arg" = "--smoke" ]; then SMOKE=1; fi
done

KEY_DIR="${DAMAN_PERSONA_KEY_DIR:-$HOME/.config/hum/daman-personas}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PERSONA_HIVE_DIR="$(cd "$SCRIPT_DIR/../daman-personas" && pwd)"
INSTALL_SCRIPT="$PERSONA_HIVE_DIR/install"
[ -x "$INSTALL_SCRIPT" ] || { echo "error: install script missing or not executable: $INSTALL_SCRIPT" >&2; exit 2; }
command -v hum >/dev/null 2>&1 || { echo "error: 'hum' CLI not on PATH; install hum first (curl -fsSL https://raw.githubusercontent.com/adiled/hum/main/install | bash)" >&2; exit 2; }

bees_smoke=(
  "daman-leader-alpha:leader:alpha"
  "daman-follower-v1-1:follower:v1"
  "daman-watchdog-v1-1:watchdog:v1"
  "daman-arbiter-v1:arbiter:v1"
)

bees_full=(
  "daman-leader-alpha:leader:alpha"
  "daman-leader-bravo:leader:bravo"
  "daman-leader-charlie:leader:charlie"
  "daman-leader-delta:leader:delta"
  "daman-leader-echo:leader:echo"
  "daman-follower-v1-1:follower:v1"
  "daman-follower-v1-2:follower:v1"
  "daman-follower-v1-3:follower:v1"
  "daman-follower-v1-4:follower:v1"
  "daman-follower-v1-5:follower:v1"
  "daman-follower-v2-1:follower:v2"
  "daman-follower-v2-2:follower:v2"
  "daman-follower-v2-3:follower:v2"
  "daman-follower-v2-4:follower:v2"
  "daman-follower-v2-5:follower:v2"
  "daman-follower-v3-1:follower:v3"
  "daman-follower-v3-2:follower:v3"
  "daman-follower-v3-3:follower:v3"
  "daman-follower-v3-4:follower:v3"
  "daman-follower-v3-5:follower:v3"
  "daman-watchdog-v1-1:watchdog:v1"
  "daman-watchdog-v1-2:watchdog:v1"
  "daman-watchdog-v2:watchdog:v2"
  "daman-arbiter-v1:arbiter:v1"
  "daman-arbiter-v2:arbiter:v2"
  "daman-relief-1:relief:mechanical"
  "daman-relief-2:relief:mechanical"
)

bee_list() {
  if [ "$SMOKE" = "1" ]; then
    printf '%s\n' "${bees_smoke[@]}"
  else
    printf '%s\n' "${bees_full[@]}"
  fi
}

install_one() {
  local bee="$1" role="$2" variant="$3"
  local keyfile="$KEY_DIR/${bee}.key"
  [ -r "$keyfile" ] || { echo "error: no keyfile for $bee at $keyfile" >&2; return 1; }
  DAMAN_PERSONA_BEE_NAME="$bee" \
  DAMAN_PERSONA_ROLE="$role" \
  DAMAN_PERSONA_VARIANT="$variant" \
  DAMAN_PERSONA_KEY_PATH="$keyfile" \
    "$INSTALL_SCRIPT"
}

uninstall_one() {
  local bee="$1"
  DAMAN_PERSONA_BEE_NAME="$bee" \
  DAMAN_PERSONA_ROLE="leader" \
  DAMAN_PERSONA_KEY_PATH="$KEY_DIR/${bee}.key" \
    "$INSTALL_SCRIPT" uninstall
}

case "$MODE" in
  install|"")
    bee_list | while IFS=: read -r bee role variant; do
      [ -z "$bee" ] && continue
      echo "==> install $bee ($role/$variant)"
      install_one "$bee" "$role" "$variant" || true
    done
    echo ""
    echo "swarm installed. lifecycle from here: hum bee <bee> enter|exit|reenter"
    ;;
  --up)
    bee_list | while IFS=: read -r bee _role _variant; do
      [ -z "$bee" ] && continue
      echo "==> hum bee $bee enter"
      hum bee "$bee" enter || true
    done
    ;;
  --down)
    bee_list | while IFS=: read -r bee _role _variant; do
      [ -z "$bee" ] && continue
      echo "==> hum bee $bee exit"
      hum bee "$bee" exit || true
    done
    ;;
  --restart)
    bee_list | while IFS=: read -r bee _role _variant; do
      [ -z "$bee" ] && continue
      echo "==> hum bee $bee reenter"
      hum bee "$bee" reenter || true
    done
    ;;
  --uninstall)
    bee_list | while IFS=: read -r bee _role _variant; do
      [ -z "$bee" ] && continue
      echo "==> uninstall $bee"
      uninstall_one "$bee" || true
    done
    ;;
  --status)
    hum bee --list | grep -E "daman-(leader|follower|watchdog|arbiter|relief)" || echo "no daman bees registered"
    ;;
  --smoke)
    # bare --smoke alone defaults to install
    SMOKE=1
    bee_list | while IFS=: read -r bee role variant; do
      [ -z "$bee" ] && continue
      echo "==> install $bee ($role/$variant)"
      install_one "$bee" "$role" "$variant" || true
    done
    ;;
  *)
    echo "usage: $0 [install|--up|--down|--restart|--uninstall|--status] [--smoke]" >&2
    exit 2
    ;;
esac
