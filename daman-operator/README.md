# daman-operator

Single-process persona binary that holds the deployer EOA private key and submits the two `msg.sender ==` gated entry points on `DamanCopyBond`: `recordTrade` (as the on-chain `oracle`) and `arbiterRule` (as the on-chain `arbiterAddr`). The proxy's `initialize` write set both slots to the deployer address and the implementation exposes no setters, so until a V2 implementation with setters lands via the Safe + Timelock ceremony, this daemon is the only path the swarm has to clear those gates. See `/tmp/audit/auth_ops.md` for the rotation analysis and `/tmp/audit/operator_persona.md` for the design.

## Install

Place the deployer private key at `$HOME/.config/hum/daman-operator/operator.key` (64-char hex, no `0x` prefix, no trailing newline, mode 0600), then run:

```
hum hive /Users/adil/damanfi/agents/daman-operator install
```

The installer hard-fails if the key file is missing or has wider permissions than 0600. It does not copy from `damanfi/copy-bond/.env` automatically; copy the value manually so a stray chmod or commit on the repo can never leak it.
