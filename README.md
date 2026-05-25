# damanfi/agents

Reference hive definitions for Daman on hum. Ten member crates: `daman-watchdog`, `daman-arbiter`, `daman-farcaster-poster`, `daman-recruiter`, `daman-chain-reader`, `daman-trace-pinner`, `daman-universe-keeper`, `daman-underwriter`, `daman-relief`, plus the shared `daman-credit-policy` library. All speak the chi vocabulary documented in `damanfi/protocol::HiveVocabulary.md` plus the social-posting, recruitment, history, trace, universe-rebalance, underwriting, and credit extensions below.

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

## Credit policy: borrowing from DamanBenevolence

Bees that hold their own EOA can participate in the permissionless agent-credit primitive at `DamanBenevolence` (Arc testnet proxy `0xd66812b02F2CA8C057e68e2E80e8c22500A3b9aD`). Two entry paths:

- **Direct:** bee calls `requestLoan(amount)` from its own wallet. Requires gas. Suitable when the bee balance is between `GAS_MIN` (0.20 USDC) and `LOW_THRESHOLD` (1.00 USDC).
- **Peer-to-peer relief:** bee signs an EIP-712 `LoanRequest` payload (free, no gas) and gossips `chi:credit-signed-request` on `daman/credit/p2p`. A `daman-relief` bee picks it up and submits on chain. Suitable when the bee is fully bust (balance below `GAS_MIN`).

The shared `daman-credit-policy` crate provides:

- `classify(balance_atomic) -> CreditBranch::{Bust, Low, Normal}`: branch the bee should take based on its current USDC balance.
- `sign_loan_request(signer, chain_id, verifying_contract, amount, nonce, deadline) -> SignedLoanRequestBody`: EIP-712 sign a LoanRequest. The returned body slots into the `request` + `signature` fields of the outgoing gossip frame.
- `recommended_loan_amount(current_debt_atomic) -> u128`: amount to ask for, capped to the per-borrower headroom under `PER_BORROWER_CAP` (5 USDC).

Bees that need the credit primitive add `daman-credit-policy = { path = "../daman-credit-policy" }` to their `Cargo.toml`, hold an EVM private key (via `DAMAN_BEE_KEY` env or per-bee config), and periodically check balance against the branch table. On `Bust`, sign + gossip; on `Low`, submit `requestLoan` directly; on `Normal`, optionally call `repay` when bounty earnings arrive.

Repayments are 1:1 (zero interest). The contract binds debt to the signer's address regardless of who submits on chain, so the relief-bee relay path carries no on-chain liability for the relayer. The `daman-relief` crate carries the relayer implementation; spawn 2-3 pre-funded relief instances ($1 USDC each, enough for ~1000 relay submissions at Arc gas prices) at swarm start.

## Security model: process boundary = identity boundary

Each persona is its own self-contained forager process. Each persona owns one EOA private key. There is no shared multi-tenant signer; the `daman-arc-fs` crate is a library that the persona binary composes against `persona-base::AskerLoop` to form a single forager process per bee.

The pattern mirrors humfs from the hum docs: "each humfs forager owns its `fs.roots` snapshot, read from its local hum.json at boot." For us the analogue is: each persona forager owns its single EOA, read from its local keyfile at boot. Tool calls that would transact on behalf of any other address are structurally impossible because the forager only holds one key.

Key storage:

- `~/.config/hum/daman-personas/<bee_name>.key` — one 64-char hex file per persona, no `0x` prefix, no trailing newline, mode 0600.
- The directory is mode 0700.
- The persona binary reads its own keyfile at boot (path via `--key-path`).
- `scripts/mint-persona-keys.sh` provisions one keyfile per bee, optionally deterministic via `BEE_SEED_<bee_upper>` env values.
- `scripts/launch-swarm.sh` derives the address per bee with `cast wallet address` and passes both `--eoa-addr` and `--key-path` to the spawned process.

Tool namespacing:

- Each persona advertises only its own tools, namespaced by a short form of its bee_name (`alpha_register_leader`, `wd_v1_1_file_claim`, etc.).
- humd routes each `chi:tool-call` uniquely to the forager that advertised it. claude in each persona's sid sees only its own namespaced tools.
- No `as_bee` argument. No multi-tenant auth check. The forager signs with the only key it holds.

Blast radius: compromise of a single keyfile compromises exactly one bee's wallet. Compromise of the whole `daman-personas/` directory compromises 27 wallets, the same as the old single-keyring approach, but the per-file model lets the operator rotate keys per-bee without touching others, and per-process isolation prevents a memory leak in one persona from leaking another's key.

## License

Apache-2.0.
