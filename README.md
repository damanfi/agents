# damanfi/agents

Reference hive definitions for Daman on hum. Two member crates: `daman-watchdog` and `daman-arbiter`. Both speak the chi vocabulary documented in `damanfi/protocol::HiveVocabulary.md`.

## Propensity

| bee | statefulness | richness | wire shape |
|---|---|---|---|
| daman-watchdog | stateful (rolling window per leader) | thick (degradation policy) | listener-mostly, emits `slash-claim` |
| daman-arbiter | stateful (open disputes) | thick (ruling policy) | listener for `dispute-opened`, speaker of `ruling` |

## Wire

`daman-watchdog` listens for:
- `chi:"trade-executed"` (populates activity record)
- `chi:"settlement-completed"` (updates rolling window + loss streak)

Emits `chi:"slash-claim"` when the loss-streak threshold is crossed. The bridge forager (`damanfi/bridge`) reads `slash-claim` chis and dispatches `attestDegradation` on chain.

`daman-arbiter` listens for `chi:"dispute-opened"`. Emits `chi:"ruling"`. The bridge forager dispatches `arbiterRule(claimId, slashAmount, upheld)` on chain.

## ADR-001

Both bees source events only from the operator-side oracle's reads of the deployment's own contracts. No off-platform leaderboards, no third-party performance feeds. Hum is the transport; the chain is the truth.

## Configure

| env | bee | default | what |
|---|---|---|---|
| `HUM_THRUM_SOCK` | both | `$XDG_RUNTIME_DIR/hum/thrum.sock` | humd's NDJSON socket |
| `DAMAN_WATCHDOG_WINDOW_SIZE` | watchdog | `50` | rolling-window size per leader |
| `DAMAN_WATCHDOG_LOSS_STREAK` | watchdog | `5` | consecutive losses that trigger a slash-claim |

## Run

```bash
# From the workspace root.
cargo run -p daman-watchdog
cargo run -p daman-arbiter
```

The bees self-announce to the mesh on connection. Other humds discover them via the standard `nestling_discover` flow documented at `github.com/adiled/hum`.

## What these are not

These are reference policy implementations. The watchdog's loss-streak heuristic is intentionally simple; production deployments substitute a richer degradation model (regime shift detection, drawdown analysis, asset-policy violation, tape audit). The arbiter's auto-uphold policy is similarly placeholder; production deployments wait the full dispute window and substitute domain-expert review or evidence-replay simulation.

The point of the reference is to demonstrate the chi vocabulary and the bridge contract. Replace the policy, keep the wire.

## License

Apache-2.0.
