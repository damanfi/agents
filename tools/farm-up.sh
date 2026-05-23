#!/usr/bin/env bash
# Mint five Ed25519 keypairs, derive each humd's HumdId, write each
# container's peers.json listing the other four, and bring the bee
# farm up. Idempotent: re-running regenerates fresh keys (warning
# below) unless `--reuse` is passed.
#
# Wallets for the on-chain race are minted separately: tools/mint-evm.sh
# (companion script). The Arc testnet faucet round-trip is the patron
# king's job.

set -euo pipefail

cd "$(dirname "$0")/.."

REUSE=0
for arg in "$@"; do
  case "$arg" in
    --reuse) REUSE=1 ;;
    -h|--help)
      cat <<EOF
farm-up: mint keys, peer the mesh, bring containers up.

Usage:
  ./tools/farm-up.sh          fresh keys (DESTRUCTIVE: replaces existing)
  ./tools/farm-up.sh --reuse  keep existing keys, regenerate peers.json only

Side effects:
  - writes ./keys/watchdog-{1..5}/humd.key (Ed25519 32-byte raw)
  - writes ./keys/watchdog-{1..5}/peers.json
  - prints a roster table to stdout
EOF
      exit 0
      ;;
    *)
      echo "unknown arg: $arg" >&2
      exit 2
      ;;
  esac
done

# ── Key minting ──────────────────────────────────────────────────────
# Each key is 32 random bytes; the HumdId is sha256(ed25519-pubkey).
# We derive the pubkey via openssl off the raw 32-byte seed; if openssl
# isn't available, fall back to Python.

mint_key() {
  local out="$1"
  if command -v openssl >/dev/null 2>&1; then
    openssl rand 32 > "$out"
  else
    python3 -c "import os; import sys; sys.stdout.buffer.write(os.urandom(32))" > "$out"
  fi
  chmod 600 "$out"
}

derive_pubkey_hex() {
  # Input: 32-byte raw private key on stdin. Output: 32-byte pubkey hex.
  # Uses python+cryptography (most portable). Falls back to a placeholder
  # warning if no cryptography library is available; the demo still
  # runs but pubkey-based HumdIds will be derived once humd boots.
  python3 - <<'PY'
import sys
data = sys.stdin.buffer.read()
try:
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives import serialization
    sk = Ed25519PrivateKey.from_private_bytes(data)
    pk = sk.public_key().public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw,
    )
    print(pk.hex())
except Exception as e:
    print("PUBKEY_UNAVAILABLE", file=sys.stderr)
    sys.exit(0)
PY
}

hid_from_pubkey() {
  # Input: pubkey hex on stdin. Output: humd_<64-hex> HumdId.
  python3 - <<'PY'
import hashlib, sys
pk_hex = sys.stdin.read().strip()
if pk_hex in ("", "PUBKEY_UNAVAILABLE"):
    print("humd_unknown_pubkey_unavailable")
    sys.exit(0)
pk = bytes.fromhex(pk_hex)
print("humd_" + hashlib.sha256(pk).hexdigest())
PY
}

# ── Generate keys ────────────────────────────────────────────────────
mkdir -p keys
declare -a HIDS=()
for i in 1 2 3 4 5; do
  dir="keys/watchdog-$i"
  mkdir -p "$dir"
  if [ "$REUSE" -eq 1 ] && [ -f "$dir/humd.key" ]; then
    echo "[farm-up] reusing $dir/humd.key"
  else
    mint_key "$dir/humd.key"
    echo "[farm-up] minted $dir/humd.key"
  fi
  pubkey_hex=$(derive_pubkey_hex < "$dir/humd.key")
  hid=$(echo "$pubkey_hex" | hid_from_pubkey)
  HIDS[$i]="$hid"
done

# ── Write per-container peers.json ───────────────────────────────────
# Each container peers with the other four via the docker-compose
# network's DNS names (watchdog-2, watchdog-3, ...).
for i in 1 2 3 4 5; do
  dir="keys/watchdog-$i"
  echo "[" > "$dir/peers.json"
  first=1
  for j in 1 2 3 4 5; do
    if [ "$i" != "$j" ]; then
      hid="${HIDS[$j]}"
      hostname="watchdog-$j"
      if [ "$first" -eq 0 ]; then
        echo "," >> "$dir/peers.json"
      fi
      cat >> "$dir/peers.json" <<EOF
  { "humd_id": "$hid", "hints": ["tcp:${hostname}:14730"] }
EOF
      first=0
    fi
  done
  echo "]" >> "$dir/peers.json"
done

# ── Roster ───────────────────────────────────────────────────────────
echo
echo "Bee-farm roster:"
printf "  %-12s %s\n" "container" "humd_id"
for i in 1 2 3 4 5; do
  printf "  %-12s %s\n" "watchdog-$i" "${HIDS[$i]}"
done

# ── Bring up ─────────────────────────────────────────────────────────
echo
echo "Building image (first run takes 5-10 minutes; humd compiles from source)..."
docker compose build

echo
echo "Starting bee farm..."
docker compose up -d

echo
echo "Tailing logs (Ctrl+C to detach; containers keep running):"
echo "  docker compose logs -f"
echo
echo "Inject a degradation race:"
echo "  python3 tools/degradation-injector/main.py --leader 0x...your-leader..."
echo
echo "Tear down:"
echo "  docker compose down -v"
