# degradation-injector

Drive the bee-farm race against a live DamanCopyBond deployment.

Publishes a deliberate 6-loss streak through the oracle seam: eight `recordTrade` + `recordSettlement` calls signed by the deployment's oracle wallet, producing on-chain `TradeExecuted` and `SettlementCompleted` events. The bridge bee picks those up, gossips them as `chi:settlement-completed`, and the five containers in the bee-farm race to emit `chi:slash-claim` first.

## Install

```
pip install -r requirements.txt
```

## Run

```
export DAMAN_INJECTOR_KEY=0x...      # oracle wallet private key
export DAMAN_COPY_BOND=0x...         # deployed DamanCopyBond address
python3 main.py --leader 0x...
```

Optional:

```
--asset 0x...           # asset to record trades against (defaults to Arc USDC)
--rpc-url URL           # defaults to https://rpc.testnet.arc.network
--amount N              # atomic units per trade (default 1.0 with 18 decimals)
--loss-amount N         # negative PnL per losing settlement (default 0.05)
--gas-price-gwei N      # default 1
--dry-run               # print plan, do not send transactions
```

## Pattern

The settlement pattern is `[-, -, -, -, -, -, 0, +]`. Six losses in a row crosses every watchdog's threshold (the compose file randomizes between 3 and 7). The two follow-up settlements give judges a clean tail and let the race conclude before the next round.

## After running

Watch the farm logs:

```
docker compose logs -f | grep -E "(slash-claim|emitting)"
```

The first watchdog to file gets the timestamp; if A1 (BountyAccrual) is deployed, the bounty is routed to its address.
