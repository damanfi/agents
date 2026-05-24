#!/usr/bin/env bash
# mint-persona-keys.sh — mint EOAs for the 27 personas and write daman-arc-fs's keyring.
#
# Run once before launch-swarm.sh. Idempotent: re-running regenerates fresh keys for any
# persona not already in the keyring; pass --force to regenerate all.
#
# Output: ~/.config/hum/daman-arc-fs/keyring.json with 0600 permissions.
#
# Each persona's EOA is derived deterministically from BEE_SEED env values when set, so
# the operator can pin per-persona identities. When BEE_SEED_<bee_name_upper> is unset,
# a fresh random key is minted via `cast wallet new`.

set -euo pipefail

FORCE=0
if [[ "${1:-}" == "--force" ]]; then
  FORCE=1
fi

KEYRING_DIR="$HOME/.config/hum/daman-arc-fs"
KEYRING="$KEYRING_DIR/keyring.json"
mkdir -p "$KEYRING_DIR"
chmod 700 "$KEYRING_DIR"

if [[ ! -f "$KEYRING" ]]; then
  echo "{}" > "$KEYRING"
fi
chmod 600 "$KEYRING"

mint_one() {
  local bee="$1"
  local existing
  existing=$(jq -r --arg b "$bee" '.[$b] // empty' "$KEYRING")
  if [[ -n "$existing" && $FORCE -eq 0 ]]; then
    echo "$bee: existing key kept"
    return
  fi

  local env_var="BEE_SEED_$(echo "$bee" | tr '[:lower:]-' '[:upper:]_')"
  local seed="${!env_var:-}"
  local key
  if [[ -n "$seed" ]]; then
    # Deterministic via keccak256(seed). Operator-supplied seed entropy.
    key="0x$(printf '%s' "$seed" | cast keccak | sed 's/^0x//')"
  else
    key=$(cast wallet new --json | jq -r '.[0].private_key')
  fi
  local addr
  addr=$(cast wallet address --private-key "$key")
  echo "$bee: $addr"
  local tmp
  tmp=$(mktemp)
  jq --arg b "$bee" --arg k "$key" '. + {($b): $k}' "$KEYRING" > "$tmp"
  mv "$tmp" "$KEYRING"
  chmod 600 "$KEYRING"
}

bees=(
  "daman-leader-alpha"
  "daman-leader-bravo"
  "daman-leader-charlie"
  "daman-leader-delta"
  "daman-leader-echo"
)
for v in 1 2 3; do
  for i in 1 2 3 4 5; do
    bees+=("daman-follower-v${v}-${i}")
  done
done
bees+=(
  "daman-watchdog-v1-1"
  "daman-watchdog-v1-2"
  "daman-watchdog-v2"
  "daman-arbiter-v1"
  "daman-arbiter-v2"
  "daman-relief-1"
  "daman-relief-2"
)

for bee in "${bees[@]}"; do
  mint_one "$bee"
done

echo ""
echo "keyring at $KEYRING (mode 600)"
echo "next: scripts/launch-swarm.sh [--smoke|full]"
