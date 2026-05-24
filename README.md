# damanfi/agents

Reference hive definitions for Daman on hum. Eight member crates: `daman-watchdog`, `daman-arbiter`, `daman-farcaster-poster`, `daman-recruiter`, `daman-chain-reader`, `daman-trace-pinner`, `daman-universe-keeper`, and `daman-underwriter`. All speak the chi vocabulary documented in `damanfi/protocol::HiveVocabulary.md` plus the social-posting, recruitment, history, trace, universe-rebalance, and underwriting extensions below.

## Propensity

| bee | statefulness | richness | wire shape |
|---|---|---|---|
| daman-watchdog | stateful (rolling window per leader) | thick (degradation policy) | listener-mostly, emits `slash-claim` + `pin-trace` |
| daman-arbiter | stateful (open disputes) | thick (ruling policy) | listener for `dispute-opened`, speaker of `ruling` + `pin-trace` |
| daman-farcaster-poster | stateless | lean | listener for `cast-publish`, speaker of `cast-published` |
| daman-recruiter | stateful (invited-roster) | medium | speaker of `query-history`, `cast-publish`, `attest-recruitment`; listener for `history-result`, `cast-published` |
| daman-chain-reader | stateless | lean | listener for `query-history` and `query-balances`; speaker of `history-result` and `balances-result` |
| daman-trace-pinner | stateless | lean | listener for `pin-trace`, speaker of `trace-pinned` |
| daman-universe-keeper | stateful (last-seen snapshot) | lean | speaker of `universe-rebalance` |
| daman-underwriter | stateful (pending rounds) | medium | listener for `register-leader-request` and `history-result`; speaker of `query-history` and `underwriter-decision` |

## Wire

`daman-watchdog` listens for:
- `chi:"trade-executed"` (populates activity record)
- `chi:"settlement-completed"` (updates rolling window + loss streak)

Emits `chi:"slash-claim"` when the loss-streak threshold is crossed. The bridge forager (`damanfi/bridge`) reads `slash-claim` chis and dispatches `attestDegradation` on chain.

`daman-arbiter` listens for `chi:"dispute-opened"`. Emits `chi:"ruling"`. The bridge forager dispatches `arbiterRule(claimId, slashAmount, upheld)` on chain.

`daman-farcaster-poster` listens for `chi:"cast-publish"` on the `daman/cast` gossip topic. Wraps the Neynar API to publish a Farcaster cast from the operator-controlled handle and emits `chi:"cast-published"` carrying the cast hash and timestamp. Mirrors the `twilio-sms` outbound-messaging-as-bee pattern from hum: provider credentials, rate limits, and signer custody live in this bee so consumer agents (recruiter, future marketing bees) never touch Neynar directly.

`daman-recruiter` is mesh-native by construction: it never imports an Alchemy, Helius, or Neynar client. On a configurable cadence the recruiter publishes `chi:"query-history"` on `daman/history` (eight queries per round: four chains times two filters), consumes `chi:"history-result"` from the chain-reader forager, intersects spot-only addresses with addresses that touched perpetuals to identify candidates, and for each candidate emits two artifacts: a `chi:"cast-publish"` to the farcaster-poster carrying the templated invitation and a deterministic rationale hash, and a `chi:"attest-recruitment"` to the bridge for on-chain dispatch. The bee holds no credentials.

## ADR-001

The watchdog and arbiter source events only from the operator-side oracle's reads of the deployment's own contracts. No off-platform leaderboards, no third-party performance feeds. Hum is the transport; the chain is the truth. The farcaster-poster is an outbound forager: it does not feed the bond state, only carries publication intent off the mesh to the social surface.

## Configure

Zero API keys end-to-end. Every forager dials a protocol endpoint directly with a public default URL the operator can override; no SaaS deplatform vector at the bee boundary.

| env | bee | default | what |
|---|---|---|---|
| `HUM_THRUM_SOCK` | all | `$XDG_RUNTIME_DIR/hum/thrum.sock` | humd's NDJSON socket |
| `DAMAN_WATCHDOG_WINDOW_SIZE` | watchdog | `50` | rolling-window size per leader |
| `DAMAN_WATCHDOG_LOSS_STREAK` | watchdog | `5` | consecutive losses that trigger a slash-claim |
| `FARCASTER_HUB_URL` | farcaster-poster | `https://nemes.farcaster.xyz` | Farcaster Hub HTTP endpoint |
| `FARCASTER_SIGNER_KEY_PATH` | farcaster-poster | none | path to 32-byte ed25519 signer registered on the keystone contract |
| `DAMANFI_FARCASTER_FID` | farcaster-poster | none | numeric FID for the operator-controlled handle |
| `ARC_RPC_URL` | chain-reader | `https://rpc.testnet.arc.network` | Arc JSON-RPC |
| `POLYGON_RPC_URL` | chain-reader | `https://polygon-rpc.com` | Polygon JSON-RPC |
| `ETHEREUM_RPC_URL` | chain-reader | `https://eth.llamarpc.com` | Ethereum JSON-RPC |
| `SOLANA_RPC_URL` | chain-reader | `https://api.mainnet-beta.solana.com` | Solana RPC |
| `KUBO_API_URL` | trace-pinner | `http://localhost:5001` | local kubo HTTP API |
| `DAMAN_RECRUITER_SCAN_INTERVAL_SECS` | recruiter | `3600` | seconds between scan rounds |
| `DAMAN_RECRUITER_LOOKBACK_DAYS` | recruiter | `90` | history depth per query |
| `DAMAN_RECRUITER_CHAINS` | recruiter | `arc,polygon,ethereum,solana` | comma-separated chain list |

## Run

```bash
# From the workspace root.
cargo run -p daman-watchdog
cargo run -p daman-arbiter
cargo run -p daman-farcaster-poster
cargo run -p daman-recruiter
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
