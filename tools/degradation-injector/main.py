"""
degradation-injector. Drive the bee-farm race.

Publishes a deliberate 6-loss streak through the operator-side oracle
seam against the live Arc testnet DamanCopyBond. Eight events total:
six losing settlements followed by two neutral ones so the watchdogs
observe the streak crossing every container's threshold (which is
randomized between 3 and 7 settlements per docker-compose).

Each event is a transaction signed by the configured oracle account.
The contract's recordTrade + recordSettlement gate requires msg.sender
to equal the deployment's oracle address; the injector wallet IS that
oracle for the bee-farm demo deployment.

Wire:

  injector ──recordTrade(leader, asset, amount, true)──► DamanCopyBond
  injector ──recordSettlement(leader, tradeId, pnl)────► DamanCopyBond
                  │
                  └─emit TradeExecuted / SettlementCompleted
                  │
  bridge ◄── eth_getLogs ── chain
  bridge ──gossip-publish chi:trade-executed / chi:settlement-completed ─► mesh
  watchdogs ◄────────────── gossip ──────────────────────────────────────── mesh
  watchdogs ──chi:slash-claim──► bridge ──attestDegradation──► chain

Usage:
  python3 main.py --leader 0xABCDEF... [--asset 0x...] [--rpc-url ...]

Env:
  DAMAN_INJECTOR_KEY   hex private key of the oracle account (no 0x prefix or with)
  DAMAN_COPY_BOND      copy-bond contract address (0x prefixed)
  DAMAN_RPC_URL        defaults to https://rpc.testnet.arc.network
"""
import argparse
import os
import sys
import time
from typing import Optional

try:
    from web3 import Web3
    from eth_account import Account
except ImportError:
    print("missing deps. install: pip install -r requirements.txt", file=sys.stderr)
    sys.exit(1)


COPY_BOND_ABI = [
    {
        "type": "function",
        "name": "recordTrade",
        "stateMutability": "nonpayable",
        "inputs": [
            {"name": "leader", "type": "address"},
            {"name": "asset", "type": "address"},
            {"name": "amount", "type": "uint256"},
            {"name": "isLong", "type": "bool"},
        ],
        "outputs": [],
    },
    {
        "type": "function",
        "name": "recordSettlement",
        "stateMutability": "nonpayable",
        "inputs": [
            {"name": "leader", "type": "address"},
            {"name": "tradeId", "type": "uint256"},
            {"name": "pnl", "type": "int256"},
        ],
        "outputs": [],
    },
]


def env_or_die(key: str) -> str:
    val = os.environ.get(key)
    if not val:
        print(f"{key} is required", file=sys.stderr)
        sys.exit(2)
    return val


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--leader", required=True, help="leader EVM address to degrade")
    parser.add_argument(
        "--asset",
        default="0x3600000000000000000000000000000000000000",
        help="asset address to trade against (defaults to Arc USDC)",
    )
    parser.add_argument("--rpc-url", default=os.environ.get("DAMAN_RPC_URL", "https://rpc.testnet.arc.network"))
    parser.add_argument("--copy-bond", default=os.environ.get("DAMAN_COPY_BOND"))
    parser.add_argument(
        "--amount",
        type=int,
        default=1_000_000_000_000_000_000,
        help="trade amount in atomic units (default 1.0 with 18 decimals)",
    )
    parser.add_argument(
        "--loss-amount",
        type=int,
        default=50_000_000_000_000_000,
        help="negative PnL per losing settlement in atomic units (default -0.05)",
    )
    parser.add_argument("--gas-price-gwei", type=int, default=1)
    parser.add_argument("--dry-run", action="store_true", help="print plan, do not send transactions")
    args = parser.parse_args()

    if not args.copy_bond:
        print("--copy-bond or DAMAN_COPY_BOND is required", file=sys.stderr)
        sys.exit(2)

    private_key = env_or_die("DAMAN_INJECTOR_KEY")
    if not private_key.startswith("0x"):
        private_key = "0x" + private_key

    w3 = Web3(Web3.HTTPProvider(args.rpc_url))
    if not w3.is_connected():
        print(f"rpc not reachable: {args.rpc_url}", file=sys.stderr)
        sys.exit(3)

    account = Account.from_key(private_key)
    print(f"injector: {account.address}")
    print(f"copy-bond: {args.copy_bond}")
    print(f"leader: {args.leader}")
    print(f"asset: {args.asset}")
    print(f"chain id: {w3.eth.chain_id}")

    contract = w3.eth.contract(address=Web3.to_checksum_address(args.copy_bond), abi=COPY_BOND_ABI)

    # 8 events: trade + settlement pairs. PnL pattern: [-, -, -, -, -, -, 0, +].
    # Six losses in a row crosses every watchdog's threshold (3 to 8 across
    # the farm, mid is ~5). Two follow-up neutral/positive events give
    # judges a clean event tail and let the race conclude.
    settlements = [
        ("loss", -args.loss_amount),
        ("loss", -args.loss_amount),
        ("loss", -args.loss_amount),
        ("loss", -args.loss_amount),
        ("loss", -args.loss_amount),
        ("loss", -args.loss_amount),
        ("flat", 0),
        ("win", args.loss_amount),
    ]

    def send(tx_name: str, fn) -> Optional[str]:
        if args.dry_run:
            print(f"[dry-run] {tx_name}")
            return None
        nonce = w3.eth.get_transaction_count(account.address)
        tx = fn.build_transaction({
            "from": account.address,
            "nonce": nonce,
            "chainId": w3.eth.chain_id,
            "gas": 250_000,
            "gasPrice": w3.to_wei(args.gas_price_gwei, "gwei"),
        })
        signed = w3.eth.account.sign_transaction(tx, private_key=private_key)
        tx_hash = w3.eth.send_raw_transaction(signed.raw_transaction)
        receipt = w3.eth.wait_for_transaction_receipt(tx_hash, timeout=60)
        status = "ok" if receipt.status == 1 else "FAIL"
        print(f"  {tx_name}: tx={tx_hash.hex()} block={receipt.blockNumber} {status}")
        return tx_hash.hex()

    leader_cs = Web3.to_checksum_address(args.leader)
    asset_cs = Web3.to_checksum_address(args.asset)

    for i, (kind, pnl) in enumerate(settlements, start=1):
        trade_id = int(time.time() * 1000) + i
        print(f"\nevent {i}/{len(settlements)} ({kind}, pnl={pnl}):")
        send("recordTrade", contract.functions.recordTrade(leader_cs, asset_cs, args.amount, True))
        # Brief pause so block ordering is clear in the bridge poll cycle.
        time.sleep(1.0)
        send("recordSettlement", contract.functions.recordSettlement(leader_cs, trade_id, pnl))
        time.sleep(1.0)

    print("\ninjection complete. watchdogs should race to file slash-claim in <60s.")
    print("watch: docker compose logs -f | grep -E '(slash-claim|emitting)'")


if __name__ == "__main__":
    main()
