# damanfi/agents

Reference hive definitions for Daman on hum. Three member crates: `daman-watchdog`, `daman-arbiter`, and `daman-farcaster-poster`. All three speak the chi vocabulary documented in `damanfi/protocol::HiveVocabulary.md` plus the social-posting extension below.

## Propensity

| bee | statefulness | richness | wire shape |
|---|---|---|---|
| daman-watchdog | stateful (rolling window per leader) | thick (degradation policy) | listener-mostly, emits `slash-claim` |
| daman-arbiter | stateful (open disputes) | thick (ruling policy) | listener for `dispute-opened`, speaker of `ruling` |
| daman-farcaster-poster | stateless | lean | listener for `cast-publish`, speaker of `cast-published` |

## Wire

`daman-watchdog` listens for:
- `chi:"trade-executed"` (populates activity record)
- `chi:"settlement-completed"` (updates rolling window + loss streak)

Emits `chi:"slash-claim"` when the loss-streak threshold is crossed. The bridge forager (`damanfi/bridge`) reads `slash-claim` chis and dispatches `attestDegradation` on chain.

`daman-arbiter` listens for `chi:"dispute-opened"`. Emits `chi:"ruling"`. The bridge forager dispatches `arbiterRule(claimId, slashAmount, upheld)` on chain.

`daman-farcaster-poster` listens for `chi:"cast-publish"` on the `daman/cast` gossip topic. Wraps the Neynar API to publish a Farcaster cast from the operator-controlled handle and emits `chi:"cast-published"` carrying the cast hash and timestamp. Mirrors the `twilio-sms` outbound-messaging-as-bee pattern from hum: provider credentials, rate limits, and signer custody live in this bee so consumer agents (recruiter, future marketing bees) never touch Neynar directly.

## ADR-001

The watchdog and arbiter source events only from the operator-side oracle's reads of the deployment's own contracts. No off-platform leaderboards, no third-party performance feeds. Hum is the transport; the chain is the truth. The farcaster-poster is an outbound forager: it does not feed the bond state, only carries publication intent off the mesh to the social surface.

## Configure

| env | bee | default | what |
|---|---|---|---|
| `HUM_THRUM_SOCK` | all | `$XDG_RUNTIME_DIR/hum/thrum.sock` | humd's NDJSON socket |
| `DAMAN_WATCHDOG_WINDOW_SIZE` | watchdog | `50` | rolling-window size per leader |
| `DAMAN_WATCHDOG_LOSS_STREAK` | watchdog | `5` | consecutive losses that trigger a slash-claim |
| `NEYNAR_API_KEY` | farcaster-poster | none | Neynar developer-console API key |
| `NEYNAR_SIGNER_UUID` | farcaster-poster | none | uuid of the registered Farcaster signer |
| `DAMANFI_FARCASTER_FID` | farcaster-poster | none | numeric FID for the operator-controlled handle |
| `NEYNAR_API_BASE` | farcaster-poster | `https://api.neynar.com` | base URL override for tests |

## Run

```bash
# From the workspace root.
cargo run -p daman-watchdog
cargo run -p daman-arbiter
cargo run -p daman-farcaster-poster
```

The bees self-announce to the mesh on connection. Other humds discover them via the standard `nestling_discover` flow documented at `github.com/adiled/hum`. Anyone can run a Daman watchdog by following `github.com/damanfi/agents`; the handshake is the registration.

## What these are not

These are reference policy implementations. The watchdog's loss-streak heuristic is intentionally simple; production deployments substitute a richer degradation model (regime shift detection, drawdown analysis, asset-policy violation, tape audit). The arbiter's auto-uphold policy is similarly placeholder; production deployments wait the full dispute window and substitute domain-expert review or evidence-replay simulation.

The point of the reference is to demonstrate the chi vocabulary and the bridge contract. Replace the policy, keep the wire.

## Bee farm

`docker-compose.yml` orchestrates five watchdog containers, each running its own humd plus the `daman-watchdog` binary. Identities are minted as Ed25519 keypairs by `tools/farm-up.sh`; each container peers with the other four via the docker-compose network. Loss-streak thresholds and window sizes are randomized across the five containers (configured via env in the compose file) so the race is non-trivial: a degraded leader trips containers with stricter policies first.

`tools/degradation-injector/main.py` drives the race. It signs eight `recordTrade` + `recordSettlement` calls against the live DamanCopyBond contract on Arc testnet, producing a deliberate 6-loss streak. The bridge bee polls chain, gossips the events as chi tones, the five containers race to emit `chi:slash-claim`, and the bridge dispatches `attestDegradation` on chain. First-to-file is the winner.

Setup:

```
./tools/farm-up.sh                          # mint keys + peers, build, up
python3 tools/degradation-injector/main.py --leader 0x...
docker compose logs -f
```

The injector reads `DAMAN_INJECTOR_KEY` and `DAMAN_COPY_BOND` from env. Faucet round-trip for the six wallets (five watchdog humds + one injector) is the operator's responsibility before the first run.

## License

Apache-2.0.
