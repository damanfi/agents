//! `daman_tools(...) -> Vec<Tool>`: the load-bearing factory the persona binary calls to
//! get its full namespaced tool set wired against its own signer + provider.
//!
//! Each tool closure captures:
//! - the per-bee `PrivateKeySigner` (alloy)
//! - the rpc URL + chain id for the Arc-testnet provider
//! - the `DamanAddrs` snapshot
//!
//! The closure builds a fresh `ProviderBuilder::new().with_recommended_fillers().wallet(signer).on_http(url)`
//! per call so the signer's tx nonces stay consistent across concurrent invocations. Read
//! tools use a wallet-less provider.
//!
//! Tool naming: every tool name carries the persona's namespace prefix. e.g. for persona
//! `daman-leader-alpha` with namespace `alpha`, the leader-register tool is
//! `alpha_register_leader`. humd routes by exact tool name, so namespacing guarantees
//! 1:1 persona ↔ forager routing across the 27-bee swarm.

use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolEvent;
use reverb_arc_fs::errors::ForagerError;
use reverb_arc_fs::tools::{Idempotency, Tool, ToolCall, ToolResult};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::addrs::DamanAddrs;
use crate::contracts::{
    Benevolence, BountyAccrual, CopyBond, Erc20, RefundProtocol, ReputationRegistry,
    UniverseRegistry,
};
use crate::credit_inbox::{self, SignedLoanRequest};

/// Per-bee configuration the tool factories close over.
#[derive(Clone)]
pub struct DamanCtx {
    pub bee_name: Arc<String>,
    pub eoa_addr: Arc<String>,
    pub rpc_url: Arc<String>,
    pub chain_id: u64,
    pub addrs: Arc<DamanAddrs>,
    pub signer: PrivateKeySigner,
}

impl DamanCtx {
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

/// Build the namespaced daman tools for one persona bee, each enriched with the
/// description + JSON schema from [`crate::specs::daman_tool_specs`] so humd's
/// prompt-forward path can inject the canonical `{name, description, inputSchema}`
/// shape into every chi:"prompt" the worker sees. The forager binary calls this once
/// at boot.
///
/// Excluded: `record_trade` and `rule_claim`. Those are oracle / arbiter-gated on
/// `DamanCopyBond` and the bee's signer is not the configured oracle / arbiter
/// EOA. They live on a sibling operator binary that loads the privileged key.
pub fn daman_tools(ctx: DamanCtx, namespace: &str) -> Vec<Tool> {
    let ns = namespace.to_string();
    let raw = vec![
        // copybond writes
        tool_register_leader(&ns, ctx.clone()),
        tool_post_bond(&ns, ctx.clone()),
        tool_withdraw_bond(&ns, ctx.clone()),
        tool_subscribe(&ns, ctx.clone()),
        tool_unsubscribe(&ns, ctx.clone()),
        tool_file_claim(&ns, ctx.clone()),
        tool_dispute_claim(&ns, ctx.clone()),
        // refund
        tool_claim_refund(&ns, ctx.clone()),
        // bounty
        tool_claim_bounty(&ns, ctx.clone()),
        // benevolence loan cycle
        tool_request_loan(&ns, ctx.clone()),
        tool_request_loan_with_signature(&ns, ctx.clone()),
        tool_repay(&ns, ctx.clone()),
        tool_sign_loan_request(&ns, ctx.clone()),
        // credit-mutual-aid inbox transport (filesystem; chi:gossip not wired yet)
        tool_publish_signed_request(&ns, ctx.clone()),
        tool_read_credit_inbox(&ns, ctx.clone()),
        tool_mark_credit_processed(&ns, ctx.clone()),
        // usdc
        tool_approve_usdc(&ns, ctx.clone()),
        // copybond reads
        tool_read_leader_state(&ns, ctx.clone()),
        tool_read_subscription_state(&ns, ctx.clone()),
        tool_read_claim(&ns, ctx.clone()),
        tool_read_bond_balance(&ns, ctx.clone()),
        tool_read_active_claims(&ns, ctx.clone()),
        // refund / bounty reads
        tool_read_payment(&ns, ctx.clone()),
        tool_read_bounty_amount(&ns, ctx.clone()),
        tool_read_bounty_recipient(&ns, ctx.clone()),
        tool_read_bounty_claimed(&ns, ctx.clone()),
        // reputation
        tool_read_reputation(&ns, ctx.clone()),
        // universe
        tool_universe_check(&ns, ctx.clone()),
        tool_universe_list_eligible(&ns, ctx.clone()),
        // usdc reads
        tool_read_usdc_balance(&ns, ctx.clone()),
        tool_read_usdc_allowance(&ns, ctx.clone()),
        // gossip
        tool_subscribe_to_role_events(&ns, ctx),
    ];
    apply_specs(raw, &ns)
}

/// Apply the description + inputSchema from `daman_tool_specs` onto each
/// tool by name match. Tools without a matching spec pass through with
/// the default empty description + `{"type":"object","properties":{}}`
/// schema from `Tool::new`.
fn apply_specs(tools: Vec<Tool>, namespace: &str) -> Vec<Tool> {
    use std::collections::HashMap;
    let specs = crate::specs::daman_tool_specs(namespace);
    let by_name: HashMap<String, &Value> = specs
        .iter()
        .filter_map(|s| {
            s.get("name")
                .and_then(|n| n.as_str())
                .map(|n| (n.to_string(), s))
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
// helpers (pub(crate) so the audit-supplied factories that live in this module
// can use them; also re-exported for future sibling modules)
// =============================================================================

pub(crate) fn parse_u256_arg(args: &Value, key: &str) -> Option<U256> {
    args.get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| U256::from_str(s).ok())
        .or_else(|| args.get(key).and_then(|v| v.as_u64()).map(U256::from))
}

pub(crate) fn parse_addr_arg(args: &Value, key: &str) -> Option<Address> {
    args.get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| Address::from_str(s).ok())
}

pub(crate) fn parse_b32_arg(args: &Value, key: &str) -> Option<[u8; 32]> {
    let s = args.get(key).and_then(|v| v.as_str())?;
    let s = s.trim_start_matches("0x");
    let b = hex::decode(s).ok()?;
    if b.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&b);
    Some(out)
}

pub(crate) fn abi_err(call_id: String, reason: &str) -> ToolResult {
    ToolResult::fail(call_id, ForagerError::AbiValidation { reason: reason.into() })
}

pub(crate) fn send_err(call_id: String, reason: String) -> ToolResult {
    warn!(reason = %reason, "send failed");
    ToolResult::fail(call_id, ForagerError::SendFailed { reason })
}

pub(crate) fn cfg_err(call_id: String, reason: String) -> ToolResult {
    ToolResult::fail(call_id, ForagerError::ConfigInvalid(reason))
}

pub(crate) fn ok_tx(call_id: String, tx_hash: alloy::primitives::B256, extra: Value) -> ToolResult {
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

pub(crate) fn rpc_url(ctx: &DamanCtx) -> Result<reqwest::Url, String> {
    reqwest::Url::parse(&ctx.rpc_url).map_err(|e| format!("rpc url: {e}"))
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn copy_bond_addr(ctx: &DamanCtx, call_id: &str) -> Result<Address, ToolResult> {
    Address::from_str(&ctx.addrs.copy_bond)
        .map_err(|e| abi_err(call_id.to_string(), &format!("copy_bond addr: {e}")))
}

fn usdc_addr_resolve(ctx: &DamanCtx, call_id: &str) -> Result<Address, ToolResult> {
    Address::from_str(&ctx.addrs.usdc)
        .map_err(|e| abi_err(call_id.to_string(), &format!("usdc addr: {e}")))
}

/// Render a bytes32 source tag as ASCII when it is a zero-padded printable
/// string (the convention the seed script uses, e.g. `"HLAL_2026Q2"` packed
/// into a bytes32), otherwise as 0x-hex.
fn bytes32_to_display(tag: &FixedBytes<32>) -> String {
    let bytes = tag.as_slice();
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let head = &bytes[..end];
    if !head.is_empty() && head.iter().all(|b| b.is_ascii_graphic() || *b == b'_') {
        String::from_utf8(head.to_vec()).unwrap_or_else(|_| format!("0x{}", hex::encode(bytes)))
    } else {
        format!("0x{}", hex::encode(bytes))
    }
}

// Provider construction is inlined at each call site rather than wrapped in helpers
// because alloy's `Provider<T, N>` trait is generic over the transport + network and
// `impl Provider` return types are ambiguous. Inline construction lets type inference
// flow forward at the call site.

// =============================================================================
// CopyBond writes
// =============================================================================

fn tool_register_leader(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_register_leader");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let tier: u8 = match call.args.get("tier").and_then(|v| v.as_u64()) {
                Some(t) if t <= 2 => t as u8,
                _ => return abi_err(call.call_id, "tier must be 0|1|2 (Retail|Mid|Institutional)"),
            };
            let claimed_aum = match parse_u256_arg(&call.args, "claimedAum") {
                Some(v) => v,
                None => return abi_err(call.call_id, "claimedAum required (atomic uint256, USDC base units)"),
            };
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let cb = match copy_bond_addr(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);
            let contract = CopyBond::new(cb, &provider);
            match contract.registerLeader(tier, claimed_aum).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({"tier": tier, "claimedAum": claimed_aum.to_string()}),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_post_bond(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_post_bond");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let amount = match parse_u256_arg(&call.args, "amount") {
                Some(v) => v,
                None => return abi_err(call.call_id, "amount required (USDC base units)"),
            };
            if amount == U256::ZERO {
                return abi_err(call.call_id, "amount must be > 0");
            }
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let cb = match copy_bond_addr(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let usdc = match usdc_addr_resolve(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);
            let token = Erc20::new(usdc, &provider);
            if let Err(e) = token.approve(cb, amount).send().await {
                return send_err(call.call_id, format!("approve: {e}"));
            }
            let contract = CopyBond::new(cb, &provider);
            match contract.postBond(amount).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({"amount": amount.to_string()}),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_withdraw_bond(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_withdraw_bond");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let amount = match parse_u256_arg(&call.args, "amount") {
                Some(v) => v,
                None => return abi_err(call.call_id, "amount required (USDC base units)"),
            };
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let cb = match copy_bond_addr(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);
            let contract = CopyBond::new(cb, &provider);
            match contract.withdrawBond(amount).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({"amount": amount.to_string()}),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_subscribe(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_subscribe");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let leader = match parse_addr_arg(&call.args, "leader") {
                Some(a) => a,
                None => return abi_err(call.call_id, "leader address required"),
            };
            let capital = match parse_u256_arg(&call.args, "capital") {
                Some(v) => v,
                None => return abi_err(call.call_id, "capital required (USDC base units)"),
            };
            if capital == U256::ZERO {
                return abi_err(call.call_id, "capital must be > 0");
            }
            let builder = parse_b32_arg(&call.args, "builder").unwrap_or_default();
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let cb = match copy_bond_addr(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let usdc = match usdc_addr_resolve(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);
            let token = Erc20::new(usdc, &provider);
            if let Err(e) = token.approve(cb, capital).send().await {
                return send_err(call.call_id, format!("approve: {e}"));
            }
            let contract = CopyBond::new(cb, &provider);
            match contract.subscribe(leader, capital, builder.into()).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({
                            "leader": format!("{leader:#x}"),
                            "capital": capital.to_string(),
                        }),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_unsubscribe(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_unsubscribe");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
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
            let cb = match copy_bond_addr(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);
            let contract = CopyBond::new(cb, &provider);
            match contract.unsubscribe(leader).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({"leader": format!("{leader:#x}")}),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_file_claim(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_file_claim");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let leader = match parse_addr_arg(&call.args, "leader") {
                Some(a) => a,
                None => return abi_err(call.call_id, "leader address required"),
            };
            let evidence_hash = match parse_b32_arg(&call.args, "evidenceHash") {
                Some(h) => h,
                None => return abi_err(call.call_id, "evidenceHash (bytes32 hex) required"),
            };
            let builder = parse_b32_arg(&call.args, "builder").unwrap_or_default();
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let cb = match copy_bond_addr(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);
            let contract = CopyBond::new(cb, &provider);
            match contract
                .attestDegradation(leader, evidence_hash.into(), builder.into())
                .send()
                .await
            {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => {
                        // The DegradationFlagged event's first indexed topic is claimId.
                        // Pull it out of the logs for the caller; fall back to null if
                        // the event is missing (should not happen on a successful call).
                        let claim_id = r
                            .inner
                            .logs()
                            .iter()
                            .find(|log| log.address() == cb)
                            .and_then(|log| {
                                CopyBond::DegradationFlagged::decode_log_data(
                                    log.data(),
                                    true,
                                )
                                .ok()
                                .map(|d| d.claimId)
                            })
                            .map(|id| id.to_string());
                        ok_tx(
                            call.call_id,
                            r.transaction_hash,
                            json!({
                                "leader": format!("{leader:#x}"),
                                "claimId": claim_id,
                            }),
                        )
                    }
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_dispute_claim(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_dispute_claim");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
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
            let cb = match copy_bond_addr(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);
            let contract = CopyBond::new(cb, &provider);
            match contract.disputeAttestation(claim_id).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({"claimId": claim_id.to_string()}),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

// =============================================================================
// Refund
// =============================================================================

/// Parse `paymentIds` (preferred), `paymentId` (single uint256), or legacy
/// `claimId` (single uint256, kept for spec back-compat) into a non-empty
/// Vec<U256>.
fn parse_payment_ids(args: &Value) -> Result<Vec<U256>, &'static str> {
    if let Some(arr) = args.get("paymentIds").and_then(|v| v.as_array()) {
        if arr.is_empty() {
            return Err("paymentIds must be non-empty");
        }
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            let id = v
                .as_str()
                .and_then(|s| U256::from_str(s).ok())
                .or_else(|| v.as_u64().map(U256::from))
                .ok_or("paymentIds entries must be uint256 decimal strings")?;
            out.push(id);
        }
        return Ok(out);
    }
    if let Some(id) = parse_u256_arg(args, "paymentId") {
        return Ok(vec![id]);
    }
    if let Some(id) = parse_u256_arg(args, "claimId") {
        return Ok(vec![id]);
    }
    Err("paymentIds (uint256[]) or paymentId (uint256) required")
}

fn tool_claim_refund(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_claim_refund");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let ids = match parse_payment_ids(&call.args) {
                Ok(v) => v,
                Err(reason) => return abi_err(call.call_id, reason),
            };

            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };

            let refund_addr = match Address::from_str(&ctx.addrs.refund_protocol) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("refund addr: {e}")),
            };

            let caller = match Address::from_str(&ctx.eoa_addr) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("signer addr: {e}")),
            };

            // Read-only preflight on a wallet-less provider so the signer's nonce
            // isn't bumped by view calls.
            let read_provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .on_http(url.clone());
            let read_contract = RefundProtocol::new(refund_addr, &read_provider);

            // Cheap pause check before per-id reads.
            match read_contract.paused().call().await {
                Ok(r) if r._0 => return abi_err(call.call_id, "refund protocol is paused"),
                Ok(_) => {}
                Err(e) => return send_err(call.call_id, format!("paused read: {e}")),
            }

            let now = now_unix_secs();
            let mut total_remaining = U256::ZERO;
            let mut summaries = Vec::with_capacity(ids.len());
            for id in &ids {
                let p = match read_contract.payments(*id).call().await {
                    Ok(r) => r,
                    Err(e) => return send_err(call.call_id, format!("payments({id}): {e}")),
                };
                if p.to != caller {
                    return abi_err(
                        call.call_id,
                        &format!(
                            "payment {id} belongs to {:#x}, not the bee signer {:#x}",
                            p.to, caller
                        ),
                    );
                }
                if p.refunded {
                    return abi_err(call.call_id, &format!("payment {id} already refunded"));
                }
                let release = p.releaseTimestamp;
                if release > U256::from(now) {
                    return abi_err(
                        call.call_id,
                        &format!(
                            "payment {id} still locked until unix {release} (now {now})"
                        ),
                    );
                }
                let remaining = p.amount.saturating_sub(p.withdrawnAmount);
                total_remaining += remaining;
                summaries.push(json!({
                    "paymentId": id.to_string(),
                    "remaining": remaining.to_string(),
                    "releaseTimestamp": release.to_string(),
                }));
            }
            if total_remaining.is_zero() {
                return abi_err(
                    call.call_id,
                    "no remaining principal across requested paymentIds; nothing to withdraw",
                );
            }

            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);
            let contract = RefundProtocol::new(refund_addr, &provider);

            match contract.withdraw(ids.clone()).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({
                            "paymentIds": ids.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                            "totalWithdrawn": total_remaining.to_string(),
                            "payments": summaries,
                        }),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_read_payment(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_payment");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let payment_id = match parse_u256_arg(&call.args, "paymentId") {
                Some(v) => v,
                None => return abi_err(call.call_id, "paymentId (uint256) required"),
            };
            let recipient = match parse_addr_arg(&call.args, "recipient") {
                Some(a) => a,
                None => match Address::from_str(&ctx.eoa_addr) {
                    Ok(a) => a,
                    Err(e) => return abi_err(call.call_id, &format!("signer addr: {e}")),
                },
            };

            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .on_http(url);
            let refund_addr = match Address::from_str(&ctx.addrs.refund_protocol) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("refund addr: {e}")),
            };
            let contract = RefundProtocol::new(refund_addr, &provider);

            let p = match contract.payments(payment_id).call().await {
                Ok(r) => r,
                Err(e) => return send_err(call.call_id, format!("payments: {e}")),
            };
            let balance = contract
                .balances(recipient)
                .call()
                .await
                .map(|r| r._0)
                .unwrap_or(U256::ZERO);
            let debt = contract
                .debts(recipient)
                .call()
                .await
                .map(|r| r._0)
                .unwrap_or(U256::ZERO);
            let paused = contract.paused().call().await.map(|r| r._0).unwrap_or(false);

            let now = U256::from(now_unix_secs());
            let remaining = p.amount.saturating_sub(p.withdrawnAmount);
            let withdrawable = !paused
                && !p.refunded
                && p.to == recipient
                && p.releaseTimestamp <= now
                && !remaining.is_zero()
                && balance >= remaining;

            ToolResult::ok(
                call.call_id,
                json!({
                    "paymentId": payment_id.to_string(),
                    "to": format!("{:#x}", p.to),
                    "amount": p.amount.to_string(),
                    "releaseTimestamp": p.releaseTimestamp.to_string(),
                    "refundTo": format!("{:#x}", p.refundTo),
                    "withdrawnAmount": p.withdrawnAmount.to_string(),
                    "refunded": p.refunded,
                    "remaining": remaining.to_string(),
                    "recipient": format!("{:#x}", recipient),
                    "recipientBalance": balance.to_string(),
                    "recipientDebt": debt.to_string(),
                    "paused": paused,
                    "withdrawable": withdrawable,
                    "nowUnix": now.to_string(),
                }),
            )
        }
    })
}

// =============================================================================
// Bounty
// =============================================================================

fn tool_claim_bounty(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_claim_bounty");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let claim_id = match parse_u256_arg(&call.args, "claimId") {
                Some(v) => v,
                None => return abi_err(call.call_id, "claimId required (atomic uint256 string or u64)"),
            };
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let bounty_addr = match Address::from_str(&ctx.addrs.bounty_accrual) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("bounty_accrual addr: {e}")),
            };
            let caller = ctx.signer.address();

            // Pre-flight reads to surface clean error messages without paying for a revert.
            let read_provider = ProviderBuilder::new().with_recommended_fillers().on_http(url.clone());
            let read_contract = BountyAccrual::new(bounty_addr, &read_provider);

            let recipient = match read_contract.bountyRecipient(claim_id).call().await {
                Ok(r) => r._0,
                Err(e) => return send_err(call.call_id, format!("preflight bountyRecipient: {e}")),
            };
            if recipient == Address::ZERO {
                return abi_err(call.call_id, &format!("claimId {claim_id} not found on bounty_accrual"));
            }
            if recipient != caller {
                return abi_err(
                    call.call_id,
                    &format!(
                        "caller {caller:#x} is not bounty recipient (recipient is {recipient:#x})"
                    ),
                );
            }
            match read_contract.bountyClaimed(claim_id).call().await {
                Ok(r) if r._0 => {
                    return abi_err(call.call_id, &format!("claimId {claim_id} already claimed"));
                }
                Ok(_) => {}
                Err(e) => return send_err(call.call_id, format!("preflight bountyClaimed: {e}")),
            }
            let amount = match read_contract.bountyAmount(claim_id).call().await {
                Ok(r) => r._0,
                Err(e) => return send_err(call.call_id, format!("preflight bountyAmount: {e}")),
            };
            if amount.is_zero() {
                return abi_err(call.call_id, &format!("claimId {claim_id} has zero amount"));
            }

            // Write path
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new().with_recommended_fillers().wallet(wallet).on_http(url);
            let contract = BountyAccrual::new(bounty_addr, &provider);
            match contract.claimBounty(claim_id).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({
                            "claimId": claim_id.to_string(),
                            "amount": amount.to_string(),
                            "recipient": format!("{recipient:#x}"),
                        }),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send claimBounty: {e}")),
            }
        }
    })
}

fn tool_read_bounty_amount(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_bounty_amount");
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
            let addr = match Address::from_str(&ctx.addrs.bounty_accrual) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("bounty_accrual addr: {e}")),
            };
            let contract = BountyAccrual::new(addr, &provider);
            match contract.bountyAmount(claim_id).call().await {
                Ok(r) => ToolResult::ok(
                    call.call_id,
                    json!({
                        "claimId": claim_id.to_string(),
                        "amount": r._0.to_string(),
                    }),
                ),
                Err(e) => send_err(call.call_id, format!("read bountyAmount: {e}")),
            }
        }
    })
}

fn tool_read_bounty_recipient(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_bounty_recipient");
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
            let addr = match Address::from_str(&ctx.addrs.bounty_accrual) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("bounty_accrual addr: {e}")),
            };
            let contract = BountyAccrual::new(addr, &provider);
            match contract.bountyRecipient(claim_id).call().await {
                Ok(r) => {
                    let exists = r._0 != Address::ZERO;
                    ToolResult::ok(
                        call.call_id,
                        json!({
                            "claimId": claim_id.to_string(),
                            "recipient": format!("{:#x}", r._0),
                            "exists": exists,
                        }),
                    )
                }
                Err(e) => send_err(call.call_id, format!("read bountyRecipient: {e}")),
            }
        }
    })
}

fn tool_read_bounty_claimed(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_bounty_claimed");
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
            let addr = match Address::from_str(&ctx.addrs.bounty_accrual) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("bounty_accrual addr: {e}")),
            };
            let contract = BountyAccrual::new(addr, &provider);
            match contract.bountyClaimed(claim_id).call().await {
                Ok(r) => ToolResult::ok(
                    call.call_id,
                    json!({
                        "claimId": claim_id.to_string(),
                        "claimed": r._0,
                    }),
                ),
                Err(e) => send_err(call.call_id, format!("read bountyClaimed: {e}")),
            }
        }
    })
}

// =============================================================================
// Benevolence (loan cycle)
// =============================================================================

fn tool_request_loan(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_request_loan");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let amount = match parse_u256_arg(&call.args, "amount") {
                Some(v) => v,
                None => return abi_err(call.call_id, "amount required (USDC base units, decimal string)"),
            };
            if amount.is_zero() {
                return abi_err(call.call_id, "amount must be > 0");
            }

            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);

            let benev_addr = match Address::from_str(&ctx.addrs.benevolence) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
            };
            let contract = Benevolence::new(benev_addr, &provider);
            match contract.requestLoan(amount).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({
                            "borrower": ctx.eoa_addr.as_str(),
                            "amount": amount.to_string(),
                            "path": "direct",
                        }),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_request_loan_with_signature(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_request_loan_with_signature");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            // Args may arrive top-level or nested under "request". Accept either;
            // prefer top-level.
            let req_obj = call.args.get("request");

            let pick_str = |k: &str| -> Option<String> {
                call.args.get(k).and_then(|v| v.as_str()).map(String::from)
                    .or_else(|| req_obj.and_then(|r| r.get(k)).and_then(|v| v.as_str()).map(String::from))
            };
            let pick_u256 = |k: &str| -> Option<U256> {
                pick_str(k).and_then(|s| U256::from_str(&s).ok())
            };

            let borrower = match pick_str("borrower").and_then(|s| Address::from_str(&s).ok()) {
                Some(a) => a,
                None => return abi_err(call.call_id, "borrower (0x-address) required"),
            };
            let amount = match pick_u256("amount") {
                Some(v) => v,
                None => return abi_err(call.call_id, "amount required (USDC base units, decimal string)"),
            };
            if amount.is_zero() {
                return abi_err(call.call_id, "amount must be > 0");
            }
            let nonce = match pick_u256("nonce") {
                Some(v) => v,
                None => return abi_err(call.call_id, "nonce required (read benevolence.nonceOf before signing)"),
            };
            let deadline = match pick_u256("deadline") {
                Some(v) => v,
                None => return abi_err(call.call_id, "deadline required (unix seconds)"),
            };
            let sig_hex = match call.args.get("signature").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return abi_err(call.call_id, "signature required (65-byte 0x-hex)"),
            };
            let signature = match hex::decode(sig_hex.trim_start_matches("0x")) {
                Ok(b) if b.len() == 65 => Bytes::from(b),
                Ok(b) => return abi_err(call.call_id, &format!("signature must be 65 bytes, got {}", b.len())),
                Err(e) => return abi_err(call.call_id, &format!("signature hex: {e}")),
            };

            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);

            let benev_addr = match Address::from_str(&ctx.addrs.benevolence) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
            };
            let contract = Benevolence::new(benev_addr, &provider);
            let req = Benevolence::LoanRequest { borrower, amount, nonce, deadline };

            match contract.requestLoanWithSignature(req, signature).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({
                            "borrower": format!("{borrower:#x}"),
                            "relayer": ctx.eoa_addr.as_str(),
                            "amount": amount.to_string(),
                            "nonce": nonce.to_string(),
                            "deadline": deadline.to_string(),
                            "path": "relief-relay",
                        }),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_repay(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_repay");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let amount = match parse_u256_arg(&call.args, "amount") {
                Some(v) => v,
                None => return abi_err(call.call_id, "amount required (USDC base units, decimal string)"),
            };
            if amount.is_zero() {
                return abi_err(call.call_id, "amount must be > 0");
            }

            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);

            let usdc_addr = match Address::from_str(&ctx.addrs.usdc) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("usdc addr: {e}")),
            };
            let benev_addr = match Address::from_str(&ctx.addrs.benevolence) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
            };

            // 1) USDC approve(benevolence, amount). Await receipt so the
            //    repay tx is guaranteed to see the allowance in state.
            let usdc = Erc20::new(usdc_addr, &provider);
            let approve_tx_hash = match usdc.approve(benev_addr, amount).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => r.transaction_hash,
                    Err(e) => return send_err(call.call_id, format!("approve receipt: {e}")),
                },
                Err(e) => return send_err(call.call_id, format!("approve send: {e}")),
            };

            // 2) benevolence.repay(amount). Pulls USDC via safeTransferFrom.
            let contract = Benevolence::new(benev_addr, &provider);
            match contract.repay(amount).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({
                            "borrower": ctx.eoa_addr.as_str(),
                            "amount": amount.to_string(),
                            "approveTxHash": format!("{approve_tx_hash:#x}"),
                        }),
                    ),
                    Err(e) => send_err(call.call_id, format!("repay receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("repay send: {e}")),
            }
        }
    })
}

fn tool_sign_loan_request(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_sign_loan_request");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let amount = match parse_u256_arg(&call.args, "amount") {
                Some(v) => v,
                None => return abi_err(call.call_id, "amount required (USDC base units, decimal string)"),
            };
            if amount.is_zero() {
                return abi_err(call.call_id, "amount must be > 0");
            }
            let nonce = match parse_u256_arg(&call.args, "nonce") {
                Some(v) => v,
                None => return abi_err(call.call_id, "nonce required (read benevolence.nonceOf(borrower); pass that exact value)"),
            };
            let deadline = match parse_u256_arg(&call.args, "deadline") {
                Some(v) => v,
                None => return abi_err(call.call_id, "deadline required (unix seconds; recommend now() + 3600)"),
            };
            let reason = call
                .args
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("gas top-up")
                .to_string();

            let benev_addr = match Address::from_str(&ctx.addrs.benevolence) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
            };

            let body = match daman_credit_policy::sign_loan_request(
                &ctx.signer,
                ctx.chain_id,
                benev_addr,
                amount,
                nonce,
                deadline,
            ) {
                Ok(b) => b,
                Err(e) => return abi_err(call.call_id, &format!("eip712 sign: {e}")),
            };

            // Auto-publish to the credit-mutual-aid inbox so a relief peer
            // can pick it up on its next tick without the bust bee needing
            // a second tool call (which it likely cannot afford to make).
            let signed = SignedLoanRequest {
                borrower: body.borrower.clone(),
                amount: body.amount.clone(),
                nonce: body.nonce.clone(),
                deadline: body.deadline.clone(),
                signature: body.signature.clone(),
                signed_at_ts: now_unix_secs(),
                by_bee: ctx.bee_name.as_ref().clone(),
                reason: reason.clone(),
            };

            let (inbox_path, publish_warning) = match credit_inbox::publish_request(&signed) {
                Ok(p) => (Some(p.to_string_lossy().to_string()), None),
                Err(e) => {
                    warn!(error = %e, "credit_inbox publish failed");
                    (None, Some(e))
                }
            };

            ToolResult::ok(
                call.call_id,
                json!({
                    "borrower": body.borrower,
                    "amount": body.amount,
                    "nonce": body.nonce,
                    "deadline": body.deadline,
                    "signature": body.signature,
                    "reason": reason,
                    "domain": {
                        "name": daman_credit_policy::EIP712_NAME,
                        "version": daman_credit_policy::EIP712_VERSION,
                        "chainId": ctx.chain_id,
                        "verifyingContract": format!("{benev_addr:#x}"),
                    },
                    "inbox_path": inbox_path,
                    "publish_warning": publish_warning,
                    "next_step": "wait. a relief peer polls the inbox each tick and will submit on your behalf via request_loan_with_signature; the borrowed USDC lands directly in your EOA.",
                }),
            )
        }
    })
}

fn tool_publish_signed_request(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_publish_signed_request");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            // Accept the same JSON shape sign_loan_request returns, either
            // top-level or nested under `signed`/`request`.
            let body = call
                .args
                .get("signed")
                .or_else(|| call.args.get("request"))
                .unwrap_or(&call.args);

            let pick = |k: &str| -> Option<String> {
                body.get(k).and_then(|v| v.as_str()).map(String::from)
            };
            let borrower = match pick("borrower") {
                Some(s) => s,
                None => return abi_err(call.call_id, "borrower (0x-address) required"),
            };
            let amount = match pick("amount") {
                Some(s) => s,
                None => return abi_err(call.call_id, "amount (decimal string, USDC base units) required"),
            };
            let nonce = match pick("nonce") {
                Some(s) => s,
                None => return abi_err(call.call_id, "nonce (uint256 decimal string) required"),
            };
            let deadline = match pick("deadline") {
                Some(s) => s,
                None => return abi_err(call.call_id, "deadline (unix seconds) required"),
            };
            let signature = match pick("signature") {
                Some(s) => s,
                None => return abi_err(call.call_id, "signature (65-byte 0x-hex) required"),
            };
            let reason = body
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("gas top-up")
                .to_string();

            let signed = SignedLoanRequest {
                borrower,
                amount,
                nonce,
                deadline,
                signature,
                signed_at_ts: now_unix_secs(),
                by_bee: ctx.bee_name.as_ref().clone(),
                reason,
            };

            match credit_inbox::publish_request(&signed) {
                Ok(p) => ToolResult::ok(
                    call.call_id,
                    json!({
                        "inbox_path": p.to_string_lossy(),
                        "by_bee": signed.by_bee,
                        "borrower": signed.borrower,
                        "amount": signed.amount,
                        "nonce": signed.nonce,
                    }),
                ),
                Err(e) => cfg_err(call.call_id, format!("inbox publish: {e}")),
            }
        }
    })
}

fn tool_read_credit_inbox(ns: &str, _ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_credit_inbox");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| async move {
        match credit_inbox::list_pending() {
            Ok(pending) => {
                let entries: Vec<Value> = pending
                    .into_iter()
                    .map(|p| {
                        json!({
                            "filename": p.filename,
                            "borrower": p.request.borrower,
                            "amount": p.request.amount,
                            "nonce": p.request.nonce,
                            "deadline": p.request.deadline,
                            "signature": p.request.signature,
                            "by_bee": p.request.by_bee,
                            "reason": p.request.reason,
                            "signed_at_ts": p.request.signed_at_ts,
                            "age_seconds": p.age_seconds,
                        })
                    })
                    .collect();
                ToolResult::ok(
                    call.call_id,
                    json!({
                        "count": entries.len(),
                        "entries": entries,
                        "inbox_dir": credit_inbox::inbox_dir().to_string_lossy(),
                    }),
                )
            }
            Err(e) => cfg_err(call.call_id, format!("inbox read: {e}")),
        }
    })
}

fn tool_mark_credit_processed(ns: &str, _ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_mark_credit_processed");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| async move {
        let filename = match call.args.get("filename").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return abi_err(call.call_id, "filename required (basename of the *.signed.json file)"),
        };
        let tx_hash = match call.args.get("tx_hash").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return abi_err(call.call_id, "tx_hash required (0x-prefixed hex)"),
        };
        match credit_inbox::mark_submitted(&filename, &tx_hash) {
            Ok(()) => ToolResult::ok(
                call.call_id,
                json!({
                    "filename": filename,
                    "tx_hash": tx_hash,
                    "status": "submitted",
                }),
            ),
            Err(e) => cfg_err(call.call_id, format!("mark_submitted: {e}")),
        }
    })
}

// =============================================================================
// USDC
// =============================================================================

fn parse_amount_or_max(args: &Value, key: &str) -> Option<U256> {
    if let Some(s) = args.get(key).and_then(|v| v.as_str()) {
        if s.eq_ignore_ascii_case("max") {
            return Some(U256::MAX);
        }
    }
    parse_u256_arg(args, key)
}

fn tool_approve_usdc(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_approve_usdc");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let spender = match parse_addr_arg(&call.args, "spender") {
                Some(a) => a,
                None => return abi_err(call.call_id, "spender address required (0x-prefixed)"),
            };
            let amount = match parse_amount_or_max(&call.args, "amount") {
                Some(v) => v,
                None => return abi_err(call.call_id, "amount required (uint256 base-units string, or \"max\")"),
            };

            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let wallet = EthereumWallet::from(ctx.signer.clone());
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(wallet)
                .on_http(url);

            let usdc_addr = match Address::from_str(&ctx.addrs.usdc) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("usdc addr: {e}")),
            };
            let usdc = Erc20::new(usdc_addr, &provider);
            match usdc.approve(spender, amount).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(
                        call.call_id,
                        r.transaction_hash,
                        json!({
                            "spender": format!("{spender:#x}"),
                            "amount":  amount.to_string(),
                            "unlimited": amount == U256::MAX,
                        }),
                    ),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_read_usdc_balance(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_usdc_balance");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let addr = match parse_addr_arg(&call.args, "addr") {
                Some(a) => a,
                None => match Address::from_str(&ctx.eoa_addr) {
                    Ok(a) => a,
                    Err(e) => return abi_err(call.call_id, &format!("ctx eoa: {e}")),
                },
            };
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let provider = ProviderBuilder::new().with_recommended_fillers().on_http(url);
            let usdc_addr = match Address::from_str(&ctx.addrs.usdc) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("usdc addr: {e}")),
            };
            let usdc = Erc20::new(usdc_addr, &provider);
            match usdc.balanceOf(addr).call().await {
                Ok(r) => ToolResult::ok(
                    call.call_id,
                    json!({
                        "addr":    format!("{addr:#x}"),
                        "balance": r._0.to_string(),
                    }),
                ),
                Err(e) => send_err(call.call_id, format!("read: {e}")),
            }
        }
    })
}

fn tool_read_usdc_allowance(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_usdc_allowance");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let spender = match parse_addr_arg(&call.args, "spender") {
                Some(a) => a,
                None => return abi_err(call.call_id, "spender address required"),
            };
            let owner = match Address::from_str(&ctx.eoa_addr) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("ctx eoa: {e}")),
            };
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let provider = ProviderBuilder::new().with_recommended_fillers().on_http(url);
            let usdc_addr = match Address::from_str(&ctx.addrs.usdc) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("usdc addr: {e}")),
            };
            let usdc = Erc20::new(usdc_addr, &provider);
            match usdc.allowance(owner, spender).call().await {
                Ok(r) => ToolResult::ok(
                    call.call_id,
                    json!({
                        "owner":     format!("{owner:#x}"),
                        "spender":   format!("{spender:#x}"),
                        "allowance": r._0.to_string(),
                        "unlimited": r._0 == U256::MAX,
                    }),
                ),
                Err(e) => send_err(call.call_id, format!("read: {e}")),
            }
        }
    })
}

// =============================================================================
// CopyBond reads
// =============================================================================

fn tool_read_leader_state(ns: &str, ctx: DamanCtx) -> Tool {
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
            let cb = match copy_bond_addr(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .on_http(url);
            let contract = CopyBond::new(cb, &provider);
            match contract.getLeader(leader).call().await {
                Ok(r) => {
                    // Derive requiredBond inline so claude does not have to
                    // re-encode the BondEconomics math. bps: retail 1000,
                    // mid 500, institutional 250 (floor).
                    let bps: u16 = match r.tier {
                        0 => 1000,
                        1 => 500,
                        2 => 250,
                        _ => 0,
                    };
                    let required = if bps == 0 {
                        U256::ZERO
                    } else {
                        r.claimedAum.saturating_mul(U256::from(bps)) / U256::from(10_000u64)
                    };
                    let tier_label = match r.tier {
                        0 => "Retail",
                        1 => "Mid",
                        2 => "Institutional",
                        _ => "Unknown",
                    };
                    ToolResult::ok(
                        call.call_id,
                        json!({
                            "addr": format!("{:#x}", r.addr),
                            "tier": r.tier,
                            "tierLabel": tier_label,
                            "bondAmount": r.bondAmount.to_string(),
                            "claimedAum": r.claimedAum.to_string(),
                            "requiredBond": required.to_string(),
                            "registeredAt": r.registeredAt,
                            "bondLockedUntil": r.bondLockedUntil,
                            "active": r.active,
                        }),
                    )
                }
                Err(e) => send_err(call.call_id, format!("read: {e}")),
            }
        }
    })
}

fn tool_read_subscription_state(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_subscription_state");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let follower = match parse_addr_arg(&call.args, "follower") {
                Some(a) => a,
                None => return abi_err(call.call_id, "follower address required"),
            };
            let leader = match parse_addr_arg(&call.args, "leader") {
                Some(a) => a,
                None => return abi_err(call.call_id, "leader address required"),
            };
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let cb = match copy_bond_addr(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .on_http(url);
            let contract = CopyBond::new(cb, &provider);
            match contract.getSubscription(follower, leader).call().await {
                Ok(r) => ToolResult::ok(
                    call.call_id,
                    json!({
                        "follower": format!("{:#x}", r.follower_),
                        "leader": format!("{:#x}", r.leader_),
                        "capital": r.capital.to_string(),
                        "since": r.since,
                        "builder": format!("0x{}", hex::encode(r.builder.as_slice())),
                    }),
                ),
                Err(e) => send_err(call.call_id, format!("read: {e}")),
            }
        }
    })
}

fn tool_read_claim(ns: &str, ctx: DamanCtx) -> Tool {
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
            let cb = match copy_bond_addr(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .on_http(url);
            let contract = CopyBond::new(cb, &provider);
            match contract.getClaim(claim_id).call().await {
                Ok(r) => {
                    let status_label = match r.status {
                        0 => "None",
                        1 => "Filed",
                        2 => "Disputed",
                        3 => "Upheld",
                        4 => "Rejected",
                        _ => "Unknown",
                    };
                    ToolResult::ok(
                        call.call_id,
                        json!({
                            "id": r.id.to_string(),
                            "leader": format!("{:#x}", r.leader),
                            "watchdog": format!("{:#x}", r.watchdog),
                            "evidenceHash": format!("0x{}", hex::encode(r.evidenceHash.as_slice())),
                            "filedAt": r.filedAt,
                            "disputeWindowEnds": r.disputeWindowEnds,
                            "status": r.status,
                            "statusLabel": status_label,
                            "slashAmount": r.slashAmount.to_string(),
                            "builder": format!("0x{}", hex::encode(r.builder.as_slice())),
                        }),
                    )
                }
                Err(e) => send_err(call.call_id, format!("read: {e}")),
            }
        }
    })
}

fn tool_read_bond_balance(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_bond_balance");
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
            let cb = match copy_bond_addr(&ctx, &call.call_id) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let provider = ProviderBuilder::new()
                .with_recommended_fillers()
                .on_http(url);
            let contract = CopyBond::new(cb, &provider);
            match contract.bondBalance(leader).call().await {
                Ok(r) => ToolResult::ok(
                    call.call_id,
                    json!({
                        "leader": format!("{leader:#x}"),
                        "bondBalance": r._0.to_string(),
                    }),
                ),
                Err(e) => send_err(call.call_id, format!("read: {e}")),
            }
        }
    })
}

fn tool_read_active_claims(ns: &str, _ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_active_claims");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| async move {
        ToolResult::fail(
            call.call_id,
            ForagerError::ConfigInvalid(
                "DamanCopyBond does not expose enumerable claim view; query the daman-oracle \
                 event index (subscribes to DegradationFlagged) for the full active-claims list. \
                 To make this read-on-chain real, add nextClaimId() and getClaims(cursor, limit) \
                 to the contract."
                    .into(),
            ),
        )
    })
}

// =============================================================================
// Reputation
// =============================================================================

fn tool_read_reputation(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_reputation");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let agent = match parse_addr_arg(&call.args, "agent") {
                Some(a) => a,
                None => return abi_err(call.call_id, "agent address required"),
            };
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let provider = ProviderBuilder::new().with_recommended_fillers().on_http(url);
            let reg = match Address::from_str(&ctx.addrs.reputation_registry) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("reputation addr: {e}")),
            };
            let contract = ReputationRegistry::new(reg, &provider);
            let score = match contract.reputationScore(agent).call().await {
                Ok(r) => r._0,
                Err(e) => return send_err(call.call_id, format!("read: {e}")),
            };
            let upheld = contract.cumulativeUpheld(agent).call().await.map(|r| r._0).unwrap_or(U256::ZERO);
            let rejected = contract.cumulativeRejected(agent).call().await.map(|r| r._0).unwrap_or(U256::ZERO);
            ToolResult::ok(
                call.call_id,
                json!({
                    "score": score.to_string(),
                    "cumulativeUpheld": upheld.to_string(),
                    "cumulativeRejected": rejected.to_string(),
                }),
            )
        }
    })
}

// =============================================================================
// Universe (read-only)
// =============================================================================

fn tool_universe_check(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_universe_check");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let asset = match parse_addr_arg(&call.args, "asset") {
                Some(a) => a,
                None => return abi_err(call.call_id, "asset address required"),
            };
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let provider = ProviderBuilder::new().with_recommended_fillers().on_http(url);
            let reg = match Address::from_str(&ctx.addrs.universe_registry) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("universe_registry addr: {e}")),
            };
            let contract = UniverseRegistry::new(reg, &provider);
            let eligible = match contract.isEligible(asset).call().await {
                Ok(r) => r._0,
                Err(e) => return send_err(call.call_id, format!("isEligible: {e}")),
            };
            let tag = contract.sourceTag().call().await.map(|r| r._0).ok();
            let updated = contract.lastUpdatedAt().call().await.map(|r| r._0).ok();
            ToolResult::ok(
                call.call_id,
                json!({
                    "asset": format!("{:#x}", asset),
                    "eligible": eligible,
                    "sourceTag": tag.as_ref().map(bytes32_to_display),
                    "lastUpdatedAt": updated,
                }),
            )
        }
    })
}

fn tool_universe_list_eligible(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_universe_list_eligible");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let url = match rpc_url(&ctx) {
                Ok(u) => u,
                Err(e) => return cfg_err(call.call_id, e),
            };
            let provider = ProviderBuilder::new().with_recommended_fillers().on_http(url);
            let reg = match Address::from_str(&ctx.addrs.universe_registry) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("universe_registry addr: {e}")),
            };
            let contract = UniverseRegistry::new(reg, &provider);
            let assets = match contract.listAssets().call().await {
                Ok(r) => r._0,
                Err(e) => return send_err(call.call_id, format!("listAssets: {e}")),
            };
            let tag = contract.sourceTag().call().await.map(|r| r._0).ok();
            let updated = contract.lastUpdatedAt().call().await.map(|r| r._0).ok();
            let hex_assets: Vec<String> = assets.iter().map(|a| format!("{:#x}", a)).collect();
            ToolResult::ok(
                call.call_id,
                json!({
                    "sourceTag": tag.as_ref().map(bytes32_to_display),
                    "lastUpdatedAt": updated,
                    "count": hex_assets.len(),
                    "assets": hex_assets,
                }),
            )
        }
    })
}

// =============================================================================
// Gossip (no-op for now; intent-recording)
// =============================================================================

/// Topics each role declares interest in. Returned by the tool so the
/// persona log shows the canonical strings, and exported so other call
/// sites (future humd bridge, observability dashboards) can agree on the
/// same names.
pub fn topics_for_role(role: &str, leader_addr: Option<&str>) -> Vec<String> {
    match role {
        "leader" => Vec::new(),
        "follower" => match leader_addr {
            Some(addr) => vec![format!("daman/trade-claims/{addr}")],
            None => vec!["daman/trade-claims/*".to_string()],
        },
        "watchdog" => vec!["daman/trade-claims/*".to_string()],
        "arbiter" => vec!["daman/claims/pending/*".to_string()],
        "relief" => vec!["daman/credit/p2p".to_string()],
        _ => Vec::new(),
    }
}

fn tool_subscribe_to_role_events(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_subscribe_to_role_events");
    let bee_name = ctx.bee_name.clone();
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| {
        let bee_name = bee_name.clone();
        async move {
            let role = call
                .args
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let leader_addr = call
                .args
                .get("leaderAddr")
                .and_then(|v| v.as_str());

            let known = matches!(
                role,
                "leader" | "follower" | "watchdog" | "arbiter" | "relief"
            );
            if !known {
                return abi_err(
                    call.call_id,
                    &format!(
                        "role must be one of leader|follower|watchdog|arbiter|relief, got {role:?}"
                    ),
                );
            }

            let topics = topics_for_role(role, leader_addr);

            info!(
                bee = %bee_name,
                role = role,
                topics = ?topics,
                "role-events.intent-recorded"
            );

            ToolResult::ok(
                call.call_id,
                json!({
                    "role": role,
                    "topics": topics,
                    "status": "intent-recorded",
                    "delivery": "pending-hum-gossip-bridge",
                    "note": "gossip-subscribe chi is not implemented in hum yet; humd does not forward gossip-publish frames to bee sockets. Intent is recorded locally and returned for observability."
                }),
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> DamanCtx {
        let signer = PrivateKeySigner::from_str(&"a".repeat(64)).unwrap();
        DamanCtx::new(
            "daman-leader-alpha",
            "https://rpc.testnet.arc.network",
            5042002,
            DamanAddrs::default(),
            signer,
        )
    }

    /// Expected tool count after the credit-mutual-aid inbox transport
    /// merge. Bumped from 29 to 32: added 3 (publish_signed_request,
    /// read_credit_inbox, mark_credit_processed). sign_loan_request was
    /// enhanced to auto-publish after signing but stayed in the count.
    const EXPECTED_TOOL_COUNT: usize = 32;

    #[test]
    fn factory_returns_expected_tool_count() {
        let tools = daman_tools(test_ctx(), "alpha");
        assert_eq!(tools.len(), EXPECTED_TOOL_COUNT, "tool count drift");
    }

    #[test]
    fn all_tools_carry_namespace_prefix() {
        let tools = daman_tools(test_ctx(), "alpha");
        for t in &tools {
            assert!(
                t.name().starts_with("alpha_"),
                "tool `{}` missing namespace prefix",
                t.name()
            );
        }
    }

    #[test]
    fn tool_names_match_expected_set() {
        let tools = daman_tools(test_ctx(), "alpha");
        let names: std::collections::HashSet<String> =
            tools.iter().map(|t| t.name().to_string()).collect();
        for expected in [
            "alpha_register_leader",
            "alpha_post_bond",
            "alpha_withdraw_bond",
            "alpha_subscribe",
            "alpha_unsubscribe",
            "alpha_file_claim",
            "alpha_dispute_claim",
            "alpha_claim_refund",
            "alpha_claim_bounty",
            "alpha_request_loan",
            "alpha_request_loan_with_signature",
            "alpha_repay",
            "alpha_sign_loan_request",
            "alpha_publish_signed_request",
            "alpha_read_credit_inbox",
            "alpha_mark_credit_processed",
            "alpha_approve_usdc",
            "alpha_read_leader_state",
            "alpha_read_subscription_state",
            "alpha_read_claim",
            "alpha_read_bond_balance",
            "alpha_read_active_claims",
            "alpha_read_payment",
            "alpha_read_bounty_amount",
            "alpha_read_bounty_recipient",
            "alpha_read_bounty_claimed",
            "alpha_read_reputation",
            "alpha_universe_check",
            "alpha_universe_list_eligible",
            "alpha_read_usdc_balance",
            "alpha_read_usdc_allowance",
            "alpha_subscribe_to_role_events",
        ] {
            assert!(names.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn removed_tools_are_not_in_set() {
        let tools = daman_tools(test_ctx(), "alpha");
        let names: std::collections::HashSet<String> =
            tools.iter().map(|t| t.name().to_string()).collect();
        // record_trade and rule_claim moved to operator binary; verify the bee
        // surface does not advertise them.
        assert!(!names.contains("alpha_record_trade"));
        assert!(!names.contains("alpha_rule_claim"));
    }

    #[test]
    fn namespacing_does_not_collide_across_personas() {
        let alpha_tools = daman_tools(test_ctx(), "alpha");
        let bravo_tools = daman_tools(test_ctx(), "bravo");
        let alpha_names: std::collections::HashSet<String> = alpha_tools.iter().map(|t| t.name().to_string()).collect();
        let bravo_names: std::collections::HashSet<String> = bravo_tools.iter().map(|t| t.name().to_string()).collect();
        let collisions: Vec<&String> = alpha_names.intersection(&bravo_names).collect();
        assert!(
            collisions.is_empty(),
            "namespace collision: {:?}",
            collisions
        );
    }

    #[test]
    fn read_tools_are_idempotent_write_tools_are_not() {
        let tools = daman_tools(test_ctx(), "alpha");
        for t in &tools {
            let n = t.name();
            let should_be_idem = n.starts_with("alpha_read_")
                || n.starts_with("alpha_universe_")
                || n == "alpha_subscribe_to_role_events"
                || n == "alpha_sign_loan_request";
            if should_be_idem {
                assert_eq!(t.idempotency(), Idempotency::Idempotent, "{n} should be idempotent");
            } else {
                assert_eq!(t.idempotency(), Idempotency::NotIdempotent, "{n} should be not-idempotent");
            }
        }
    }

    #[test]
    fn topics_leader_is_empty() {
        assert!(topics_for_role("leader", None).is_empty());
    }

    #[test]
    fn topics_follower_with_addr() {
        let t = topics_for_role("follower", Some("0xabc"));
        assert_eq!(t, vec!["daman/trade-claims/0xabc".to_string()]);
    }

    #[test]
    fn topics_follower_without_addr_wildcards() {
        let t = topics_for_role("follower", None);
        assert_eq!(t, vec!["daman/trade-claims/*".to_string()]);
    }

    #[test]
    fn topics_watchdog_arbiter_relief() {
        assert_eq!(
            topics_for_role("watchdog", None),
            vec!["daman/trade-claims/*".to_string()]
        );
        assert_eq!(
            topics_for_role("arbiter", None),
            vec!["daman/claims/pending/*".to_string()]
        );
        assert_eq!(
            topics_for_role("relief", None),
            vec!["daman/credit/p2p".to_string()]
        );
    }

    #[test]
    fn topics_unknown_role_is_empty() {
        assert!(topics_for_role("nope", None).is_empty());
    }
}
