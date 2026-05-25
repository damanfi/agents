//! Operator tool builders. Self-contained: does not depend on or modify
//! `daman-arc-fs::factories`. The operator owns its own sol! bindings for the
//! `recordTrade`, `getClaim`, `arbiterRule`, and universe-whitelist surfaces it
//! exercises, because the upstream `daman-arc-fs::contracts::CopyBond` binding
//! does not yet declare `recordTrade` or `getClaim`.
//!
//! Each closure builds a fresh
//! `ProviderBuilder::new().with_recommended_fillers().wallet(...).on_http(url)`
//! per call so concurrent invocations do not clobber each other's nonces.

use std::str::FromStr;
use std::sync::Arc;

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, U256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use daman_arc_fs::DamanAddrs;
use reverb_arc_fs::errors::ForagerError;
use reverb_arc_fs::tools::{Idempotency, Tool, ToolCall, ToolResult};
use serde_json::{json, Value};
use tracing::warn;

sol! {
    /// Local binding of the four `DamanCopyBond` surfaces the operator touches.
    /// Kept separate from `daman-arc-fs::contracts::CopyBond` so the operator
    /// crate is free-standing.
    #[sol(rpc)]
    contract OperatorCopyBond {
        function recordTrade(address leader, address asset, uint256 amount, bool isLong) external;
        function arbiterRule(uint256 claimId, uint256 slashAmount, bool upheld, bytes32 builder, bytes32 traceCid) external;
        function getLeader(address leader) external view returns (address addr, uint8 tier, uint256 bondAmount, uint256 claimedAum, bool active, uint64 registeredAt, uint64 bondLockedUntil);
        function getClaim(uint256 claimId) external view returns (Claim memory);

        struct Claim {
            uint256 id;
            address leader;
            address watchdog;
            bytes32 evidenceHash;
            uint64 filedAt;
            uint64 disputeWindowEnds;
            uint8 status;
            uint256 slashAmount;
            bytes32 builder;
        }
    }

    #[sol(rpc)]
    contract UniverseWhitelist {
        function isEligible(address asset) external view returns (bool);
    }
}

/// Per-bee config the operator tool factories close over. Mirrors `DamanCtx`
/// but lives in this crate so the operator stays independent.
#[derive(Clone)]
pub struct OperatorCtx {
    pub bee_name: Arc<String>,
    pub eoa_addr: Arc<String>,
    pub rpc_url: Arc<String>,
    pub chain_id: u64,
    pub addrs: Arc<DamanAddrs>,
    pub signer: PrivateKeySigner,
}

impl OperatorCtx {
    pub fn new(
        bee_name: impl Into<String>,
        rpc_url: impl Into<String>,
        chain_id: u64,
        addrs: DamanAddrs,
        signer: PrivateKeySigner,
    ) -> Self {
        let addr = format!("{:#x}", signer.address());
        Self {
            bee_name: Arc::new(bee_name.into()),
            eoa_addr: Arc::new(addr),
            rpc_url: Arc::new(rpc_url.into()),
            chain_id,
            addrs: Arc::new(addrs),
            signer,
        }
    }
}

/// Build the operator's four-tool surface, namespaced.
pub fn operator_tools(ctx: OperatorCtx, namespace: &str) -> Vec<Tool> {
    let ns = namespace.to_string();
    let raw = vec![
        tool_record_trade_oracle(&ns, ctx.clone()),
        tool_rule_claim_arbiter(&ns, ctx.clone()),
        tool_read_leader_state(&ns, ctx.clone()),
        tool_read_claim(&ns, ctx),
    ];
    apply_specs(raw, &ns)
}

fn apply_specs(tools: Vec<Tool>, namespace: &str) -> Vec<Tool> {
    use std::collections::HashMap;
    let specs = crate::specs::operator_tool_specs(namespace);
    let by_name: HashMap<String, Value> = specs
        .into_iter()
        .filter_map(|s| {
            s.get("name")
                .and_then(|n| n.as_str())
                .map(|n| (n.to_string(), s.clone()))
        })
        .collect();
    tools
        .into_iter()
        .map(|t| {
            if let Some(spec) = by_name.get(t.name()) {
                let desc = spec
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                let schema = spec
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                t.with_description(desc).with_input_schema(schema)
            } else {
                t
            }
        })
        .collect()
}

// =============================================================================
// helpers
// =============================================================================

fn parse_u256_arg(args: &Value, key: &str) -> Option<U256> {
    args.get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| U256::from_str(s).ok())
        .or_else(|| args.get(key).and_then(|v| v.as_u64()).map(U256::from))
}

fn parse_addr_arg(args: &Value, key: &str) -> Option<Address> {
    args.get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| Address::from_str(s).ok())
}

fn parse_b32_arg(args: &Value, key: &str) -> Option<[u8; 32]> {
    let s = args.get(key).and_then(|v| v.as_str())?;
    let s = s.trim_start_matches("0x");
    if s.is_empty() {
        return Some([0u8; 32]);
    }
    let b = hex::decode(s).ok()?;
    if b.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&b);
    Some(out)
}

fn abi_err(call_id: String, reason: &str) -> ToolResult {
    ToolResult::fail(call_id, ForagerError::AbiValidation { reason: reason.into() })
}

fn send_err(call_id: String, reason: String) -> ToolResult {
    warn!(reason = %reason, "operator tool send failed");
    ToolResult::fail(call_id, ForagerError::SendFailed { reason })
}

fn cfg_err(call_id: String, reason: String) -> ToolResult {
    ToolResult::fail(call_id, ForagerError::ConfigInvalid(reason))
}

fn ok_tx(call_id: String, tx_hash: alloy::primitives::B256, extra: Value) -> ToolResult {
    let mut value = json!({ "txHash": format!("{tx_hash:#x}") });
    if let Value::Object(m) = extra {
        if let Value::Object(out) = &mut value {
            for (k, v) in m {
                out.insert(k, v);
            }
        }
    }
    ToolResult::ok(call_id, value)
}

fn rpc_url(ctx: &OperatorCtx) -> Result<reqwest::Url, String> {
    reqwest::Url::parse(&ctx.rpc_url).map_err(|e| format!("rpc url: {e}"))
}

// =============================================================================
// write tools
// =============================================================================

fn tool_record_trade_oracle(ns: &str, ctx: OperatorCtx) -> Tool {
    let name = format!("{ns}_operator_record_trade");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let leader = match parse_addr_arg(&call.args, "leader") {
                Some(a) => a,
                None => return abi_err(call.call_id, "leader address required"),
            };
            let asset = match parse_addr_arg(&call.args, "asset") {
                Some(a) => a,
                None => return abi_err(call.call_id, "asset address required"),
            };
            let amount = match parse_u256_arg(&call.args, "amount") {
                Some(v) => v,
                None => return abi_err(call.call_id, "amount required (USDC base units, decimal string)"),
            };
            let is_long = match call.args.get("isLong").and_then(|v| v.as_bool()) {
                Some(b) => b,
                None => return abi_err(call.call_id, "isLong (bool) required"),
            };
            if !is_long {
                return abi_err(call.call_id, "contract reverts ShortNotPermitted when isLong=false");
            }

            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };

            // Read-side pre-check on the universe whitelist. Substrate enforces this too,
            // but skipping a doomed submit keeps the audit log clean.
            let read_provider = ProviderBuilder::new().with_recommended_fillers().on_http(url.clone());
            let universe_addr = match Address::from_str(&ctx.addrs.universe_registry) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("universe_registry addr: {e}")),
            };
            let universe = UniverseWhitelist::new(universe_addr, &read_provider);
            match universe.isEligible(asset).call().await {
                Ok(r) if !r._0 => {
                    return ToolResult::fail(
                        call.call_id,
                        ForagerError::AbiValidation {
                            reason: format!("asset {asset:#x} not eligible per UniverseRegistry"),
                        },
                    );
                }
                Ok(_) => {}
                Err(e) => return send_err(call.call_id, format!("universe read: {e}")),
            }

            // Read-side pre-check on leader registration.
            let copy_bond_addr = match Address::from_str(&ctx.addrs.copy_bond) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
            };
            let read_cb = OperatorCopyBond::new(copy_bond_addr, &read_provider);
            match read_cb.getLeader(leader).call().await {
                Ok(r) if !r.active => {
                    return ToolResult::fail(
                        call.call_id,
                        ForagerError::AbiValidation {
                            reason: format!("leader {leader:#x} not active on CopyBond"),
                        },
                    );
                }
                Ok(_) => {}
                Err(e) => return send_err(call.call_id, format!("leader read: {e}")),
            }

            // Wallet-bound provider for the write.
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new().with_recommended_fillers().wallet(wallet).on_http(url);
            let contract = OperatorCopyBond::new(copy_bond_addr, &provider);
            match contract.recordTrade(leader, asset, amount, is_long).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({
                            "leader": format!("{leader:#x}"),
                            "asset":  format!("{asset:#x}"),
                            "amount": amount.to_string(),
                            "isLong": is_long,
                        }),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_rule_claim_arbiter(ns: &str, ctx: OperatorCtx) -> Tool {
    let name = format!("{ns}_operator_rule_claim");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let claim_id = match parse_u256_arg(&call.args, "claimId") {
                Some(v) => v,
                None => return abi_err(call.call_id, "claimId required"),
            };
            let slash_amount = match parse_u256_arg(&call.args, "slashAmount") {
                Some(v) => v,
                None => return abi_err(call.call_id, "slashAmount required"),
            };
            let upheld = match call.args.get("upheld").and_then(|v| v.as_bool()) {
                Some(b) => b,
                None => return abi_err(call.call_id, "upheld (bool) required"),
            };
            let builder = parse_b32_arg(&call.args, "builder").unwrap_or([0u8; 32]);
            let trace_cid = parse_b32_arg(&call.args, "traceCid").unwrap_or([0u8; 32]);

            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };

            let copy_bond_addr = match Address::from_str(&ctx.addrs.copy_bond) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
            };

            // Read-side pre-check on claim status. Status enum order: None=0, Filed=1,
            // Disputed=2, Upheld=3, Rejected=4. Skip already-ruled claims and claims still
            // inside their dispute window.
            let read_provider = ProviderBuilder::new().with_recommended_fillers().on_http(url.clone());
            let read_cb = OperatorCopyBond::new(copy_bond_addr, &read_provider);
            match read_cb.getClaim(claim_id).call().await {
                Ok(r) => {
                    let claim = r._0;
                    if claim.id == U256::ZERO {
                        return abi_err(call.call_id, &format!("claim {claim_id} not found"));
                    }
                    if claim.status == 3 || claim.status == 4 {
                        return ToolResult::fail(
                            call.call_id,
                            ForagerError::AbiValidation {
                                reason: format!(
                                    "claim {claim_id} already ruled (status {})",
                                    claim.status
                                ),
                            },
                        );
                    }
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    if now < claim.disputeWindowEnds as u64 {
                        return ToolResult::fail(
                            call.call_id,
                            ForagerError::AbiValidation {
                                reason: format!(
                                    "dispute window open until {} (now {now})",
                                    claim.disputeWindowEnds
                                ),
                            },
                        );
                    }
                }
                Err(e) => return send_err(call.call_id, format!("claim read: {e}")),
            }

            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new().with_recommended_fillers().wallet(wallet).on_http(url);
            let contract = OperatorCopyBond::new(copy_bond_addr, &provider);
            match contract
                .arbiterRule(claim_id, slash_amount, upheld, builder.into(), trace_cid.into())
                .send()
                .await
            {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({
                            "claimId":     claim_id.to_string(),
                            "slashAmount": slash_amount.to_string(),
                            "upheld":      upheld,
                        }),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

// =============================================================================
// read tools
// =============================================================================

fn tool_read_leader_state(ns: &str, ctx: OperatorCtx) -> Tool {
    let name = format!("{ns}_read_leader_state");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let leader = match parse_addr_arg(&call.args, "leader") {
                Some(a) => a,
                None => return abi_err(call.call_id, "leader address required"),
            };
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let provider = ProviderBuilder::new().with_recommended_fillers().on_http(url);
            let addr = match Address::from_str(&ctx.addrs.copy_bond) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
            };
            let contract = OperatorCopyBond::new(addr, &provider);
            match contract.getLeader(leader).call().await {
                Ok(r) => ToolResult::ok(
                    call.call_id,
                    json!({
                        "addr":             format!("{:#x}", r.addr),
                        "tier":             r.tier,
                        "bondAmount":       r.bondAmount.to_string(),
                        "claimedAum":       r.claimedAum.to_string(),
                        "active":           r.active,
                        "registeredAt":     r.registeredAt,
                        "bondLockedUntil":  r.bondLockedUntil,
                    }),
                ),
                Err(e) => send_err(call.call_id, format!("read: {e}")),
            }
        }
    })
}

fn tool_read_claim(ns: &str, ctx: OperatorCtx) -> Tool {
    let name = format!("{ns}_read_claim");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let claim_id = match parse_u256_arg(&call.args, "claimId") {
                Some(v) => v,
                None => return abi_err(call.call_id, "claimId required"),
            };
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let provider = ProviderBuilder::new().with_recommended_fillers().on_http(url);
            let addr = match Address::from_str(&ctx.addrs.copy_bond) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
            };
            let contract = OperatorCopyBond::new(addr, &provider);
            match contract.getClaim(claim_id).call().await {
                Ok(r) => {
                    let c = r._0;
                    ToolResult::ok(
                        call.call_id,
                        json!({
                            "id":                c.id.to_string(),
                            "leader":            format!("{:#x}", c.leader),
                            "watchdog":          format!("{:#x}", c.watchdog),
                            "evidenceHash":      format!("0x{}", hex::encode(c.evidenceHash)),
                            "filedAt":           c.filedAt,
                            "disputeWindowEnds": c.disputeWindowEnds,
                            "status":            c.status,
                            "statusName":        status_name(c.status),
                            "slashAmount":       c.slashAmount.to_string(),
                            "builder":           format!("0x{}", hex::encode(c.builder)),
                        }),
                    )
                }
                Err(e) => send_err(call.call_id, format!("read: {e}")),
            }
        }
    })
}

fn status_name(s: u8) -> &'static str {
    match s {
        0 => "None",
        1 => "Filed",
        2 => "Disputed",
        3 => "Upheld",
        4 => "Rejected",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> OperatorCtx {
        let signer = PrivateKeySigner::from_str(&"a".repeat(64)).unwrap();
        OperatorCtx::new(
            "daman-operator",
            "https://rpc.testnet.arc.network",
            5042002,
            DamanAddrs::default(),
            signer,
        )
    }

    #[test]
    fn factory_returns_four_tools() {
        let tools = operator_tools(test_ctx(), "op");
        assert_eq!(tools.len(), 4);
    }

    #[test]
    fn all_tools_carry_namespace_prefix() {
        let tools = operator_tools(test_ctx(), "op");
        for t in &tools {
            assert!(t.name().starts_with("op_"), "missing prefix: {}", t.name());
        }
    }

    #[test]
    fn write_tools_are_not_idempotent_reads_are() {
        let tools = operator_tools(test_ctx(), "op");
        for t in &tools {
            let n = t.name();
            if n.contains("_read_") {
                assert_eq!(t.idempotency(), Idempotency::Idempotent, "{n} should be idempotent");
            } else {
                assert_eq!(t.idempotency(), Idempotency::NotIdempotent, "{n} should be not-idempotent");
            }
        }
    }
}
