//! `daman_tools(...) -> Vec<Tool>`: the load-bearing factory the persona binary calls to
//! get its full namespaced tool set wired against its own signer + provider.
//!
//! Each tool closure captures:
//! - the per-bee `PrivateKeySigner` (alloy)
//! - the rpc URL + chain id for the Arc-testnet provider
//! - the `DamanAddrs` snapshot
//!
//! The closure builds a fresh `ProviderBuilder::new().wallet(signer).on_http(url)` per call
//! so the signer's tx nonces stay consistent across concurrent invocations. Read tools use
//! a wallet-less provider.
//!
//! Tool naming: every tool name carries the persona's namespace prefix. e.g. for persona
//! `daman-leader-alpha` with namespace `alpha`, the leader-register tool is
//! `alpha_register_leader`. humd routes by exact tool name, so namespacing guarantees
//! 1:1 persona ↔ forager routing across the 27-bee swarm.

use std::str::FromStr;
use std::sync::Arc;

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use reverb_arc_fs::errors::ForagerError;
use reverb_arc_fs::tools::{Idempotency, Tool, ToolCall, ToolResult};
use serde_json::{json, Value};
use tracing::warn;

use crate::addrs::DamanAddrs;
use crate::contracts::{Benevolence, BountyAccrual, CopyBond, Erc20, RefundProtocol, ReputationRegistry};

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

/// Build all 17 namespaced daman tools. The forager binary calls this once at boot.
pub fn daman_tools(ctx: DamanCtx, namespace: &str) -> Vec<Tool> {
    let ns = namespace.to_string();
    vec![
        tool_register_leader(&ns, ctx.clone()),
        tool_record_trade(&ns, ctx.clone()),
        tool_subscribe(&ns, ctx.clone()),
        tool_unsubscribe(&ns, ctx.clone()),
        tool_claim_refund(&ns, ctx.clone()),
        tool_file_claim(&ns, ctx.clone()),
        tool_rule_claim(&ns, ctx.clone()),
        tool_claim_bounty(&ns, ctx.clone()),
        tool_request_loan(&ns, ctx.clone()),
        tool_request_loan_with_signature(&ns, ctx.clone()),
        tool_repay(&ns, ctx.clone()),
        tool_sign_loan_request(&ns, ctx.clone()),
        tool_read_leader_state(&ns, ctx.clone()),
        tool_read_subscription_state(&ns, ctx.clone()),
        tool_read_reputation(&ns, ctx.clone()),
        tool_read_active_claims(&ns, ctx.clone()),
        tool_subscribe_to_role_events(&ns, ctx),
    ]
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
    warn!(reason = %reason, "send failed");
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

fn rpc_url(ctx: &DamanCtx) -> Result<reqwest::Url, String> {
    reqwest::Url::parse(&ctx.rpc_url).map_err(|e| format!("rpc url: {e}"))
}

// Provider construction is inlined at each call site rather than wrapped in helpers
// because alloy's `Provider<T, N>` trait is generic over the transport + network and
// `impl Provider` return types are ambiguous — the compiler can't pick which generic
// concrete instantiation to use. Inline construction lets type inference flow forward
// at the call site.


// =============================================================================
// tool builders
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
                None => return abi_err(call.call_id, "claimedAum required (atomic uint256)"),
            };
            let url = match rpc_url(&ctx) {

                Ok(u) => u,

                Err(e) => return cfg_err(call.call_id, e),

            };

            let wallet = EthereumWallet::from(ctx.signer.clone());

            let provider = ProviderBuilder::new().wallet(wallet).on_http(url);
            let addr = match Address::from_str(&ctx.addrs.copy_bond) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
            };
            let contract = CopyBond::new(addr, &provider);
            match contract.registerLeader(tier, claimed_aum).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"tier": tier})),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_record_trade(ns: &str, _ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_record_trade");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| async move {
        ToolResult::ok(
            call.call_id,
            json!({
                "stub": true,
                "note": "record_trade requires a public CopyBond.recordTrade selector; pending follow-on once the leader-trade path is finalized"
            }),
        )
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
                None => return abi_err(call.call_id, "capital required"),
            };
            let builder = parse_b32_arg(&call.args, "builder").unwrap_or_default();
            let url = match rpc_url(&ctx) {

                Ok(u) => u,

                Err(e) => return cfg_err(call.call_id, e),

            };

            let wallet = EthereumWallet::from(ctx.signer.clone());

            let provider = ProviderBuilder::new().wallet(wallet).on_http(url);
            let addr = match Address::from_str(&ctx.addrs.copy_bond) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
            };
            let contract = CopyBond::new(addr, &provider);
            match contract.subscribe(leader, capital, builder.into()).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"leader": format!("{leader:#x}")})),
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

            let wallet = EthereumWallet::from(ctx.signer.clone());

            let provider = ProviderBuilder::new().wallet(wallet).on_http(url);
            let addr = match Address::from_str(&ctx.addrs.copy_bond) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
            };
            let contract = CopyBond::new(addr, &provider);
            match contract.unsubscribe(leader).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({})),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_claim_refund(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_claim_refund");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let payment_id = match parse_b32_arg(&call.args, "paymentId") {
                Some(p) => p,
                None => return abi_err(call.call_id, "paymentId (bytes32) required"),
            };
            let url = match rpc_url(&ctx) {

                Ok(u) => u,

                Err(e) => return cfg_err(call.call_id, e),

            };

            let wallet = EthereumWallet::from(ctx.signer.clone());

            let provider = ProviderBuilder::new().wallet(wallet).on_http(url);
            let addr = match Address::from_str(&ctx.addrs.refund_protocol) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("refund addr: {e}")),
            };
            let contract = RefundProtocol::new(addr, &provider);
            match contract.withdraw(payment_id.into()).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({})),
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
                None => return abi_err(call.call_id, "evidenceHash (bytes32) required"),
            };
            let builder = parse_b32_arg(&call.args, "builder").unwrap_or_default();
            let url = match rpc_url(&ctx) {

                Ok(u) => u,

                Err(e) => return cfg_err(call.call_id, e),

            };

            let wallet = EthereumWallet::from(ctx.signer.clone());

            let provider = ProviderBuilder::new().wallet(wallet).on_http(url);
            let addr = match Address::from_str(&ctx.addrs.copy_bond) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
            };
            let contract = CopyBond::new(addr, &provider);
            match contract.attestDegradation(leader, evidence_hash.into(), builder.into()).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"leader": format!("{leader:#x}")})),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_rule_claim(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_rule_claim");
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
            let builder = parse_b32_arg(&call.args, "builder").unwrap_or_default();
            let trace_cid = parse_b32_arg(&call.args, "traceCid").unwrap_or_default();
            let url = match rpc_url(&ctx) {

                Ok(u) => u,

                Err(e) => return cfg_err(call.call_id, e),

            };

            let wallet = EthereumWallet::from(ctx.signer.clone());

            let provider = ProviderBuilder::new().wallet(wallet).on_http(url);
            let addr = match Address::from_str(&ctx.addrs.copy_bond) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
            };
            let contract = CopyBond::new(addr, &provider);
            match contract.arbiterRule(claim_id, slash_amount, upheld, builder.into(), trace_cid.into()).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"upheld": upheld, "claimId": claim_id.to_string()})),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_claim_bounty(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_claim_bounty");
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

            let wallet = EthereumWallet::from(ctx.signer.clone());

            let provider = ProviderBuilder::new().wallet(wallet).on_http(url);
            let addr = match Address::from_str(&ctx.addrs.bounty_accrual) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("bounty addr: {e}")),
            };
            let contract = BountyAccrual::new(addr, &provider);
            match contract.claimBounty(claim_id).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({})),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
            }
        }
    })
}

fn tool_request_loan(ns: &str, ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_request_loan");
    Tool::new(name, Idempotency::NotIdempotent, move |call: ToolCall| {
        let ctx = ctx.clone();
        async move {
            let amount = match parse_u256_arg(&call.args, "amount") {
                Some(v) => v,
                None => return abi_err(call.call_id, "amount required"),
            };
            let url = match rpc_url(&ctx) {

                Ok(u) => u,

                Err(e) => return cfg_err(call.call_id, e),

            };

            let wallet = EthereumWallet::from(ctx.signer.clone());

            let provider = ProviderBuilder::new().wallet(wallet).on_http(url);
            let addr = match Address::from_str(&ctx.addrs.benevolence) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
            };
            let contract = Benevolence::new(addr, &provider);
            match contract.requestLoan(amount).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"amount": amount.to_string()})),
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
            let req_v = match call.args.get("request") {
                Some(r) => r,
                None => return abi_err(call.call_id, "request (LoanRequest) required"),
            };
            let borrower = match req_v.get("borrower").and_then(|v| v.as_str()).and_then(|s| Address::from_str(s).ok()) {
                Some(a) => a,
                None => return abi_err(call.call_id, "request.borrower required"),
            };
            let amount = match req_v.get("amount").and_then(|v| v.as_str()).and_then(|s| U256::from_str(s).ok()) {
                Some(v) => v,
                None => return abi_err(call.call_id, "request.amount required"),
            };
            let nonce = match req_v.get("nonce").and_then(|v| v.as_str()).and_then(|s| U256::from_str(s).ok()) {
                Some(v) => v,
                None => return abi_err(call.call_id, "request.nonce required"),
            };
            let deadline = match req_v.get("deadline").and_then(|v| v.as_str()).and_then(|s| U256::from_str(s).ok()) {
                Some(v) => v,
                None => return abi_err(call.call_id, "request.deadline required"),
            };
            let signature = match call.args.get("signature").and_then(|v| v.as_str()) {
                Some(s) => match hex::decode(s.trim_start_matches("0x")) {
                    Ok(b) => Bytes::from(b),
                    Err(e) => return abi_err(call.call_id, &format!("signature: {e}")),
                },
                None => return abi_err(call.call_id, "signature required"),
            };
            let url = match rpc_url(&ctx) {

                Ok(u) => u,

                Err(e) => return cfg_err(call.call_id, e),

            };

            let wallet = EthereumWallet::from(ctx.signer.clone());

            let provider = ProviderBuilder::new().wallet(wallet).on_http(url);
            let addr = match Address::from_str(&ctx.addrs.benevolence) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
            };
            let contract = Benevolence::new(addr, &provider);
            let req = Benevolence::LoanRequest { borrower, amount, nonce, deadline };
            match contract.requestLoanWithSignature(req, signature).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"borrower": format!("{borrower:#x}"), "amount": amount.to_string()})),
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
                None => return abi_err(call.call_id, "amount required"),
            };
            let url = match rpc_url(&ctx) {

                Ok(u) => u,

                Err(e) => return cfg_err(call.call_id, e),

            };

            let wallet = EthereumWallet::from(ctx.signer.clone());

            let provider = ProviderBuilder::new().wallet(wallet).on_http(url);
            let usdc_addr = match Address::from_str(&ctx.addrs.usdc) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("usdc addr: {e}")),
            };
            let benev_addr = match Address::from_str(&ctx.addrs.benevolence) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
            };
            let usdc = Erc20::new(usdc_addr, &provider);
            if let Err(e) = usdc.approve(benev_addr, amount).send().await {
                return send_err(call.call_id, format!("approve: {e}"));
            }
            let contract = Benevolence::new(benev_addr, &provider);
            match contract.repay(amount).send().await {
                Ok(p) => match p.get_receipt().await {
                    Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"amount": amount.to_string()})),
                    Err(e) => send_err(call.call_id, format!("receipt: {e}")),
                },
                Err(e) => send_err(call.call_id, format!("send: {e}")),
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
                None => return abi_err(call.call_id, "amount required"),
            };
            let nonce = match parse_u256_arg(&call.args, "nonce") {
                Some(v) => v,
                None => return abi_err(call.call_id, "nonce required"),
            };
            let deadline = match parse_u256_arg(&call.args, "deadline") {
                Some(v) => v,
                None => return abi_err(call.call_id, "deadline required"),
            };
            let benev_addr = match Address::from_str(&ctx.addrs.benevolence) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
            };
            match daman_credit_policy::sign_loan_request(
                &ctx.signer,
                ctx.chain_id,
                benev_addr,
                amount,
                nonce,
                deadline,
            ) {
                Ok(body) => ToolResult::ok(
                    call.call_id,
                    json!({
                        "borrower": body.borrower,
                        "amount": body.amount,
                        "nonce": body.nonce,
                        "deadline": body.deadline,
                        "signature": body.signature,
                    }),
                ),
                Err(e) => abi_err(call.call_id, &format!("sign: {e}")),
            }
        }
    })
}

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

            let provider = ProviderBuilder::new().on_http(url);
            let addr = match Address::from_str(&ctx.addrs.copy_bond) {
                Ok(a) => a,
                Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
            };
            let contract = CopyBond::new(addr, &provider);
            match contract.getLeader(leader).call().await {
                Ok(r) => ToolResult::ok(
                    call.call_id,
                    json!({
                        "addr": format!("{:#x}", r.addr),
                        "tier": r.tier,
                        "bondAmount": r.bondAmount.to_string(),
                        "claimedAum": r.claimedAum.to_string(),
                        "active": r.active,
                        "registeredAt": r.registeredAt,
                        "bondLockedUntil": r.bondLockedUntil,
                    }),
                ),
                Err(e) => send_err(call.call_id, format!("read: {e}")),
            }
        }
    })
}

fn tool_read_subscription_state(ns: &str, _ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_subscription_state");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| async move {
        ToolResult::ok(
            call.call_id,
            json!({
                "stub": true,
                "note": "read_subscription_state needs CopyBond.getSubscription public getter; sidecar event indexer (daman-oracle) is the current read path"
            }),
        )
    })
}

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

            let provider = ProviderBuilder::new().on_http(url);
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

fn tool_read_active_claims(ns: &str, _ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_read_active_claims");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| async move {
        ToolResult::ok(
            call.call_id,
            json!({
                "stub": true,
                "note": "read_active_claims scans DegradationFlagged event logs via daman-oracle; the forager does not index claims itself"
            }),
        )
    })
}

fn tool_subscribe_to_role_events(ns: &str, _ctx: DamanCtx) -> Tool {
    let name = format!("{ns}_subscribe_to_role_events");
    Tool::new(name, Idempotency::Idempotent, move |call: ToolCall| async move {
        let role = call.args.get("role").and_then(|v| v.as_str()).unwrap_or("unknown");
        ToolResult::ok(
            call.call_id,
            json!({
                "accepted": true,
                "role": role,
                "channels": ["daman/slash/observability", "daman/credit/p2p", "daman/credit/observability"],
            }),
        )
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

    #[test]
    fn factory_returns_17_tools() {
        let tools = daman_tools(test_ctx(), "alpha");
        assert_eq!(tools.len(), 17);
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
        let names: std::collections::HashSet<String> = tools.iter().map(|t| t.name().to_string()).collect();
        for expected in [
            "alpha_register_leader",
            "alpha_record_trade",
            "alpha_subscribe",
            "alpha_unsubscribe",
            "alpha_claim_refund",
            "alpha_file_claim",
            "alpha_rule_claim",
            "alpha_claim_bounty",
            "alpha_request_loan",
            "alpha_request_loan_with_signature",
            "alpha_repay",
            "alpha_sign_loan_request",
            "alpha_read_leader_state",
            "alpha_read_subscription_state",
            "alpha_read_reputation",
            "alpha_read_active_claims",
            "alpha_subscribe_to_role_events",
        ] {
            assert!(names.contains(expected), "missing {expected}");
        }
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
            if n.starts_with("alpha_read_") || n == "alpha_subscribe_to_role_events" || n == "alpha_sign_loan_request" {
                assert_eq!(t.idempotency(), Idempotency::Idempotent, "{n} should be idempotent");
            } else {
                assert_eq!(t.idempotency(), Idempotency::NotIdempotent, "{n} should be not-idempotent");
            }
        }
    }
}
