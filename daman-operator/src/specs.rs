//! Tool spec table for the narrow operator surface.
//!
//! Each entry returns `{name, description, inputSchema}` for one of the four operator
//! tools. The shape matches `daman-arc-fs::specs::daman_tool_specs` so humd's
//! prompt-forward path injects them into every chi:"prompt" the worker sees.
//!
//! The operator does NOT advertise the broad daman tool surface. It only holds the two
//! privileged write entry points plus two narrow read helpers that let it pick a slash
//! amount and confirm leader state before submitting.

use serde_json::{json, Value};

/// Build the operator's namespaced tool specs. `ns` is the per-bee prefix from
/// `namespace_for_bee` (for `daman-operator` the fallback path returns `b_daman_operator`).
pub fn operator_tool_specs(ns: &str) -> Vec<Value> {
    vec![
        json!({
            "name": format!("{ns}_operator_record_trade"),
            "description": "Submit DamanCopyBond.recordTrade(leader, asset, amount, isLong) as the on-chain oracle. The signer is the deployer EOA; the call clears the msg.sender == oracle gate. Pre-validates UniverseRegistry.isEligible(asset) and CopyBond.getLeader(leader).active before submit and returns a structured error on either miss.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "leader": { "type": "string", "description": "Leader EOA, 0x-prefixed hex" },
                    "asset":  { "type": "string", "description": "Asset address in the on-chain universe whitelist, 0x-prefixed hex" },
                    "amount": { "type": "string", "description": "Notional in USDC base units (6 decimals), decimal string" },
                    "isLong": { "type": "boolean", "description": "True for a long trade; the contract reverts on shorts (ShortNotPermitted)" }
                },
                "required": ["leader", "asset", "amount", "isLong"]
            }
        }),
        json!({
            "name": format!("{ns}_operator_rule_claim"),
            "description": "Submit DamanCopyBond.arbiterRule(claimId, slashAmount, upheld, builder, traceCid) as the on-chain arbiter. The signer is the deployer EOA; the call clears the msg.sender == arbiterAddr gate. Reads getClaim(claimId) first; rejects when the claim is already ruled (Upheld or Rejected) or when the dispute window has not closed yet.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "claimId":     { "type": "string", "description": "Dispute claim id (uint256 as decimal string)" },
                    "slashAmount": { "type": "string", "description": "Slash amount in USDC base units, capped at BondEconomics.maxSlashAmount(bondAmount); the contract reverts on overflow" },
                    "upheld":      { "type": "boolean", "description": "True to uphold (slash fires + bounty accrues), false to reject (no slash)" },
                    "builder":     { "type": "string", "description": "32-byte attribution tag, 0x-prefixed hex; empty means inherit from claim" },
                    "traceCid":    { "type": "string", "description": "32-byte trace CID for the ruling, 0x-prefixed hex; empty allowed" }
                },
                "required": ["claimId", "slashAmount", "upheld"]
            }
        }),
        json!({
            "name": format!("{ns}_read_leader_state"),
            "description": "Read DamanCopyBond.getLeader(leader) for a candidate address. Returns tier, bondAmount, claimedAum, active, registeredAt, bondLockedUntil. Used by the operator to size slash amounts and confirm the leader is registered before recording a trade against the bond.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "leader": { "type": "string", "description": "Leader EOA, 0x-prefixed hex" }
                },
                "required": ["leader"]
            }
        }),
        json!({
            "name": format!("{ns}_read_claim"),
            "description": "Read DamanCopyBond.getClaim(claimId) for a specific claim id. Returns leader, watchdog, evidenceHash, filedAt, disputeWindowEnds, status, slashAmount, builder. Used by the operator to skip already-ruled claims and to honor the dispute window before submitting arbiterRule.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "claimId": { "type": "string", "description": "Dispute claim id (uint256 as decimal string)" }
                },
                "required": ["claimId"]
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_specs_with_ns_prefix() {
        let specs = operator_tool_specs("op");
        assert_eq!(specs.len(), 4);
        for s in &specs {
            let name = s.get("name").and_then(|v| v.as_str()).unwrap();
            assert!(name.starts_with("op_"), "missing prefix: {name}");
        }
    }

    #[test]
    fn each_spec_has_description_and_schema() {
        for s in operator_tool_specs("op") {
            assert!(s.get("description").and_then(|v| v.as_str()).map(|d| !d.is_empty()).unwrap_or(false));
            assert!(s.get("inputSchema").is_some());
        }
    }
}
