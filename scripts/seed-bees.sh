#!/usr/bin/env bash
# seed-bees.sh — drip a fixed per-persona gas stipend in USDC from the deployer
# to each Daman persona EOA. Per BRIEF_SWARM_AND_CINEMA, $0.50 covers a bee's
# gas budget for register() + initial requestLoan() + a handful of subsequent
# tool-call submissions. Working capital comes from the benevolence treasury,
# not from this drip.
#
# Reads the deployer key from $DEPLOYER_PRIVATE_KEY if set; otherwise sources
# damanfi/copy-bond/.env (which exports PRIVATE_KEY for forge scripts) and uses
# that value.
#
# Env:
#   KEY_DIR     persona keyfile dir (default ~/.config/hum/daman-personas)
#   USDC        token address (default Arc testnet bridged USDC)
#   RPC         RPC url (default Arc testnet)
#   AMOUNT      drip amount in token base units (default 500000 = $0.50 USDC)
#
# Usage:
#   ./scripts/seed-bees.sh                         # drip all keyfiles in KEY_DIR
#   ./scripts/seed-bees.sh daman-leader-alpha ...  # drip only the named bees
set -euo pipefail

KEY_DIR="${KEY_DIR:-$HOME/.config/hum/daman-personas}"
USDC="${USDC:-0x3600000000000000000000000000000000000000}"
RPC="${RPC:-https://rpc.testnet.arc.network}"
AMOUNT="${AMOUNT:-3000000}"

if [ -z "${DEPLOYER_PRIVATE_KEY:-}" ]; then
  ENVFILE="$HOME/damanfi/copy-bond/.env"
  if [ -r "$ENVFILE" ]; then
    DEPLOYER_PRIVATE_KEY="$(grep -E '^PRIVATE_KEY=' "$ENVFILE" | head -1 | cut -d= -f2-)"
  fi
fi
[ -n "${DEPLOYER_PRIVATE_KEY:-}" ] || { echo "error: DEPLOYER_PRIVATE_KEY unset and copy-bond/.env not readable" >&2; exit 2; }

command -v cast >/dev/null 2>&1 || { echo "error: 'cast' (foundry) not on PATH" >&2; exit 2; }

# Resolve the keyfile list. If positional args given, treat them as bee names;
# otherwise drip every *.key file in KEY_DIR.
files=()
if [ "$#" -gt 0 ]; then
  for bee in "$@"; do
    files+=("$KEY_DIR/${bee}.key")
  done
else
  while IFS= read -r f; do files+=("$f"); done < <(ls "$KEY_DIR"/*.key 2>/dev/null | sort)
fi
[ "${#files[@]}" -gt 0 ] || { echo "error: no keyfiles under $KEY_DIR" >&2; exit 2; }

# Total
human_amount=$(awk -v a="$AMOUNT" 'BEGIN { printf "%.2f", a/1000000 }')
echo "==> dripping \$$human_amount USDC × ${#files[@]} bees from deployer"
echo ""

for keyfile in "${files[@]}"; do
  [ -r "$keyfile" ] || { echo "skip: $keyfile not readable"; continue; }
  bee=$(basename "$keyfile" .key)
  addr=$(cast wallet address --private-key "0x$(cat "$keyfile")")
  printf '%-28s %s ... ' "$bee" "$addr"
  if tx=$(cast send "$USDC" "transfer(address,uint256)" "$addr" "$AMOUNT" \
          --rpc-url "$RPC" --private-key "$DEPLOYER_PRIVATE_KEY" --json 2>&1); then
    hash=$(echo "$tx" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("transactionHash",""))' 2>/dev/null || echo "")
    echo "ok ${hash:0:14}…"
  else
    echo "FAIL"
    echo "$tx" | head -3
  fi
done

echo ""
echo "drip complete."
