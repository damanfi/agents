#!/usr/bin/env bash
# mint-persona-keys.sh — mint EOAs for the 27 personas as per-persona keyfiles.
#
# Per BRIEF_PERSONA_AS_FORAGER, each persona is its own forager process and owns its
# single EOA private key. There is no shared keyring; process boundary = identity
# boundary. Mirrors the humfs `fs.roots` per-instance scoping pattern from
# https://adiled.github.io/hum/hives/humfs : "Each humfs forager owns its fs.roots
# snapshot, read from its local hum.json at boot."
#
# Run once before launch-swarm.sh. Idempotent: re-running keeps existing keyfiles
# untouched; pass --force to regenerate all.
#
# Output: ~/.config/hum/daman-personas/<bee_name>.key — one 64-char hex file per
# persona, no `0x` prefix, no newline, mode 0600. The persona binary reads this at
# boot and uses it as its only on-chain identity.
#
# Each persona's EOA is derived deterministically from BEE_SEED_<bee_name_upper> when
# set, so the operator can pin per-persona identities. When the seed is unset, a
# fresh random key is minted via `cast wallet new`.

set -euo pipefail

FORCE=0
if [[ "${1:-}" == "--force" ]]; then
  FORCE=1
fi

KEY_DIR="$HOME/.config/hum/daman-personas"
mkdir -p "$KEY_DIR"
chmod 700 "$KEY_DIR"

mint_one() {
  local bee="$1"
  local keyfile="$KEY_DIR/${bee}.key"
  if [[ -f "$keyfile" && $FORCE -eq 0 ]]; then
    local addr
    addr=$(cast wallet address --private-key "0x$(cat "$keyfile")")
    echo "$bee: $addr (existing)"
    return
  fi

  local env_var="BEE_SEED_$(echo "$bee" | tr '[:lower:]-' '[:upper:]_')"
  local seed="${!env_var:-}"
  local key
  if [[ -n "$seed" ]]; then
    # Deterministic via keccak256(seed). Operator-supplied seed entropy.
    key=$(printf '%s' "$seed" | cast keccak | sed 's/^0x//')
  else
    key=$(cast wallet new --json | jq -r '.[0].private_key' | sed 's/^0x//')
  fi
  printf '%s' "$key" > "$keyfile"
  chmod 600 "$keyfile"
  local addr
  addr=$(cast wallet address --private-key "0x$key")
  echo "$bee: $addr ($keyfile)"
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
echo "keyfiles at $KEY_DIR (mode 600 each)"
echo "next: scripts/launch-swarm.sh [--smoke|full]"
