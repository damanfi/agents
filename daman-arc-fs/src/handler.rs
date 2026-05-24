//! Concrete tool implementations wired to the deployed Daman contracts via alloy.
//!
//! Each tool runs through the safety pipeline inherited from `reverb-arc-fs`: auth
//! (`chi.from == args.as_bee`), ABI validation (per-tool input schema check), rate limit
//! (per-bee + per-tool + global sliding window), send (sign + submit via per-bee EOA),
//! receipt cache.
//!
//! The simulation gate (`eth_call`) is left implicit in alloy's behavior: write calls
//! issued through the provider go through `eth_estimateGas` + `eth_call` simulation under
//! the hood, and reverts surface before broadcast. Explicit pre-simulation hooks can be
//! added per tool when a deeper invariant check is needed.
//!
//! The handler holds:
//! - the RPC URL string for per-call provider construction
//! - an `Arc<Keyring>` for per-bee signing
//! - a `RateLimiter` from the substrate's safety module
//! - a `DamanAddrs` snapshot of the deployed proxies
//! - an `Arc<Config>` carrying the allowed-contracts gate
//!
//! `dispatch()` branches by tool name. Read tools skip auth + rate limit; write tools
//! run the full pipeline.

use std::str::FromStr;
use std::sync::Arc;

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use async_trait::async_trait;
use reverb_arc_fs::config::Config;
use reverb_arc_fs::errors::ForagerError;
use reverb_arc_fs::keyring::Keyring;
use reverb_arc_fs::safety::{check_auth, RateLimiter};
use reverb_arc_fs::tools::{Idempotency, Tool, ToolCall, ToolResult};
use serde_json::{json, Value};
use tracing::warn;

use crate::tools::DamanAddrs;

sol! {
    #[sol(rpc)]
    contract CopyBond {
        function registerLeader(uint8 tier, uint256 claimedAum) external;
        function postBond(uint256 amount) external;
        function withdrawBond(uint256 amount) external;
        function subscribe(address leader, uint256 capital, bytes32 builder) external;
        function unsubscribe(address leader) external;
        function attestDegradation(address leader, bytes32 evidenceHash, bytes32 builder) external returns (uint256);
        function arbiterRule(uint256 claimId, uint256 slashAmount, bool upheld, bytes32 builder, bytes32 traceCid) external;
        function getLeader(address leader) external view returns (address addr, uint8 tier, uint256 bondAmount, uint256 claimedAum, bool active, uint64 registeredAt, uint64 bondLockedUntil);
    }

    #[sol(rpc)]
    contract BountyAccrual {
        function claimBounty(uint256 claimId) external;
        function bountyAmount(uint256 claimId) external view returns (uint256);
        function bountyRecipient(uint256 claimId) external view returns (address);
        function bountyClaimed(uint256 claimId) external view returns (bool);
    }

    #[sol(rpc)]
    contract ReputationRegistry {
        function reputationScore(address agent) external view returns (int256);
        function cumulativeUpheld(address agent) external view returns (uint256);
        function cumulativeRejected(address agent) external view returns (uint256);
    }

    #[sol(rpc)]
    contract Benevolence {
        struct LoanRequest {
            address borrower;
            uint256 amount;
            uint256 nonce;
            uint256 deadline;
        }
        function requestLoan(uint256 amount) external;
        function requestLoanWithSignature(LoanRequest calldata req, bytes calldata signature) external;
        function repay(uint256 amount) external;
        function debtOf(address borrower) external view returns (uint256);
        function nonceOf(address borrower) external view returns (uint256);
        function isEligible(address candidate) external view returns (bool);
        function treasuryAvailable() external view returns (uint256);
    }

    #[sol(rpc)]
    contract RefundProtocol {
        function withdraw(bytes32 paymentId) external;
    }

    #[sol(rpc)]
    contract Erc20 {
        function approve(address spender, uint256 value) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Concrete tool handler. Built once at boot and shared across the dispatch loop.
pub struct Handler {
    pub rpc_url: String,
    pub chain_id: u64,
    pub addrs: DamanAddrs,
    pub keyring: Arc<Keyring>,
    pub config: Arc<Config>,
    pub rate_limiter: Arc<RateLimiter>,
}

impl Handler {
    pub fn new(
        rpc_url: String,
        chain_id: u64,
        addrs: DamanAddrs,
        keyring: Arc<Keyring>,
        config: Arc<Config>,
        rate_limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            rpc_url,
            chain_id,
            addrs,
            keyring,
            config,
            rate_limiter,
        }
    }

    fn signer_for(&self, bee: &str) -> Result<PrivateKeySigner, ForagerError> {
        let key = self
            .keyring
            .lookup(bee)
            .ok_or_else(|| ForagerError::KeyringMiss { bee: bee.into() })?;
        PrivateKeySigner::from_str(key.as_str().trim_start_matches("0x"))
            .map_err(|e| ForagerError::SendFailed { reason: format!("parse key: {e}") })
    }

    fn rpc_url_parsed(&self) -> Result<reqwest::Url, ForagerError> {
        reqwest::Url::parse(&self.rpc_url)
            .map_err(|e| ForagerError::ConfigInvalid(format!("rpc url: {e}")))
    }

    fn ensure_allowed(&self, addr: &str, call_id: &str) -> Result<(), ToolResult> {
        if !self.config.is_allowed_contract(addr) {
            return Err(ToolResult::fail(
                call_id.to_string(),
                ForagerError::ContractNotAllowed { contract: addr.into() },
            ));
        }
        Ok(())
    }

    pub async fn dispatch(&self, call: ToolCall) -> ToolResult {
        let is_read = call.tool_name.starts_with("daman_read_")
            || call.tool_name == "daman_subscribe_to_role_events"
            || call.tool_name == "daman_sign_loan_request";

        if !is_read {
            if let Err(e) = check_auth(&call) {
                return ToolResult::fail(call.call_id, e);
            }
            if let Err(e) = self
                .rate_limiter
                .check_and_record(&call.from, &call.tool_name)
            {
                return ToolResult::fail(call.call_id, e);
            }
        }

        match call.tool_name.as_str() {
            "daman_register_leader" => self.register_leader(call).await,
            "daman_record_trade" => self.record_trade(call).await,
            "daman_subscribe" => self.subscribe(call).await,
            "daman_unsubscribe" => self.unsubscribe(call).await,
            "daman_claim_refund" => self.claim_refund(call).await,
            "daman_file_claim" => self.file_claim(call).await,
            "daman_rule_claim" => self.rule_claim(call).await,
            "daman_claim_bounty" => self.claim_bounty(call).await,
            "daman_request_loan" => self.request_loan(call).await,
            "daman_request_loan_with_signature" => self.request_loan_with_sig(call).await,
            "daman_repay" => self.repay(call).await,
            "daman_sign_loan_request" => self.sign_loan_request(call).await,
            "daman_read_leader_state" => self.read_leader_state(call).await,
            "daman_read_subscription_state" => self.read_subscription_state(call).await,
            "daman_read_reputation" => self.read_reputation(call).await,
            "daman_read_active_claims" => self.read_active_claims(call).await,
            "daman_subscribe_to_role_events" => self.subscribe_to_role_events(call).await,
            other => ToolResult::fail(
                call.call_id,
                ForagerError::UnknownTool { tool: other.into() },
            ),
        }
    }

    // ----------- write tools (leader / follower / watchdog / arbiter) -----------

    async fn register_leader(&self, call: ToolCall) -> ToolResult {
        let bee = call.as_bee.clone().unwrap_or_default();
        let tier: u8 = match call.args.get("tier").and_then(|v| v.as_u64()) {
            Some(t) if t <= 2 => t as u8,
            _ => return abi_err(call.call_id, "tier must be 0|1|2 (Retail|Mid|Institutional)"),
        };
        let claimed_aum = match parse_u256_arg(&call.args, "claimedAum") {
            Some(v) => v,
            None => return abi_err(call.call_id, "claimedAum required (atomic uint256)"),
        };
        if let Err(r) = self.ensure_allowed(&self.addrs.copy_bond, &call.call_id) {
            return r;
        }
        let signer = match self.signer_for(&bee) {
            Ok(s) => s,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .on_http(url);
        let copy_bond = match Address::from_str(&self.addrs.copy_bond) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
        };
        let contract = CopyBond::new(copy_bond, &provider);
        match contract.registerLeader(tier, claimed_aum).send().await {
            Ok(pending) => match pending.get_receipt().await {
                Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"tier": tier})),
                Err(e) => send_err(call.call_id, format!("receipt: {e}")),
            },
            Err(e) => send_err(call.call_id, format!("send: {e}")),
        }
    }

    async fn record_trade(&self, call: ToolCall) -> ToolResult {
        not_yet_wired(
            call,
            "record_trade requires a public CopyBond.recordTrade selector; pending follow-on once the leader-trade path is finalized",
        )
    }

    async fn subscribe(&self, call: ToolCall) -> ToolResult {
        let bee = call.as_bee.clone().unwrap_or_default();
        let leader = match parse_addr_arg(&call.args, "leader") {
            Some(a) => a,
            None => return abi_err(call.call_id, "leader address required"),
        };
        let capital = match parse_u256_arg(&call.args, "capital") {
            Some(v) => v,
            None => return abi_err(call.call_id, "capital required"),
        };
        let builder = parse_b32_arg(&call.args, "builder").unwrap_or_default();
        if let Err(r) = self.ensure_allowed(&self.addrs.copy_bond, &call.call_id) {
            return r;
        }
        let signer = match self.signer_for(&bee) {
            Ok(s) => s,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .on_http(url);
        let copy_bond = match Address::from_str(&self.addrs.copy_bond) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
        };
        let contract = CopyBond::new(copy_bond, &provider);
        match contract.subscribe(leader, capital, builder.into()).send().await {
            Ok(pending) => match pending.get_receipt().await {
                Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"leader": format!("{leader:#x}")})),
                Err(e) => send_err(call.call_id, format!("receipt: {e}")),
            },
            Err(e) => send_err(call.call_id, format!("send: {e}")),
        }
    }

    async fn unsubscribe(&self, call: ToolCall) -> ToolResult {
        let bee = call.as_bee.clone().unwrap_or_default();
        let leader = match parse_addr_arg(&call.args, "leader") {
            Some(a) => a,
            None => return abi_err(call.call_id, "leader address required"),
        };
        if let Err(r) = self.ensure_allowed(&self.addrs.copy_bond, &call.call_id) {
            return r;
        }
        let signer = match self.signer_for(&bee) {
            Ok(s) => s,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .on_http(url);
        let copy_bond = match Address::from_str(&self.addrs.copy_bond) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
        };
        let contract = CopyBond::new(copy_bond, &provider);
        match contract.unsubscribe(leader).send().await {
            Ok(pending) => match pending.get_receipt().await {
                Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({})),
                Err(e) => send_err(call.call_id, format!("receipt: {e}")),
            },
            Err(e) => send_err(call.call_id, format!("send: {e}")),
        }
    }

    async fn claim_refund(&self, call: ToolCall) -> ToolResult {
        let bee = call.as_bee.clone().unwrap_or_default();
        let payment_id = match parse_b32_arg(&call.args, "paymentId") {
            Some(p) => p,
            None => return abi_err(call.call_id, "paymentId (bytes32) required"),
        };
        if let Err(r) = self.ensure_allowed(&self.addrs.refund_protocol, &call.call_id) {
            return r;
        }
        let signer = match self.signer_for(&bee) {
            Ok(s) => s,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .on_http(url);
        let refund = match Address::from_str(&self.addrs.refund_protocol) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("refund_protocol addr: {e}")),
        };
        let contract = RefundProtocol::new(refund, &provider);
        match contract.withdraw(payment_id.into()).send().await {
            Ok(pending) => match pending.get_receipt().await {
                Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({})),
                Err(e) => send_err(call.call_id, format!("receipt: {e}")),
            },
            Err(e) => send_err(call.call_id, format!("send: {e}")),
        }
    }

    async fn file_claim(&self, call: ToolCall) -> ToolResult {
        let bee = call.as_bee.clone().unwrap_or_default();
        let leader = match parse_addr_arg(&call.args, "leader") {
            Some(a) => a,
            None => return abi_err(call.call_id, "leader address required"),
        };
        let evidence_hash = match parse_b32_arg(&call.args, "evidenceHash") {
            Some(h) => h,
            None => return abi_err(call.call_id, "evidenceHash (bytes32) required"),
        };
        let builder = parse_b32_arg(&call.args, "builder").unwrap_or_default();
        if let Err(r) = self.ensure_allowed(&self.addrs.copy_bond, &call.call_id) {
            return r;
        }
        let signer = match self.signer_for(&bee) {
            Ok(s) => s,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .on_http(url);
        let copy_bond = match Address::from_str(&self.addrs.copy_bond) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
        };
        let contract = CopyBond::new(copy_bond, &provider);
        match contract
            .attestDegradation(leader, evidence_hash.into(), builder.into())
            .send()
            .await
        {
            Ok(pending) => match pending.get_receipt().await {
                Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"leader": format!("{leader:#x}")})),
                Err(e) => send_err(call.call_id, format!("receipt: {e}")),
            },
            Err(e) => send_err(call.call_id, format!("send: {e}")),
        }
    }

    async fn rule_claim(&self, call: ToolCall) -> ToolResult {
        let bee = call.as_bee.clone().unwrap_or_default();
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
        if let Err(r) = self.ensure_allowed(&self.addrs.copy_bond, &call.call_id) {
            return r;
        }
        let signer = match self.signer_for(&bee) {
            Ok(s) => s,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .on_http(url);
        let copy_bond = match Address::from_str(&self.addrs.copy_bond) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
        };
        let contract = CopyBond::new(copy_bond, &provider);
        match contract
            .arbiterRule(claim_id, slash_amount, upheld, builder.into(), trace_cid.into())
            .send()
            .await
        {
            Ok(pending) => match pending.get_receipt().await {
                Ok(r) => ok_tx(
                    call.call_id,
                    r.transaction_hash,
                    json!({"upheld": upheld, "claimId": claim_id.to_string()}),
                ),
                Err(e) => send_err(call.call_id, format!("receipt: {e}")),
            },
            Err(e) => send_err(call.call_id, format!("send: {e}")),
        }
    }

    async fn claim_bounty(&self, call: ToolCall) -> ToolResult {
        let bee = call.as_bee.clone().unwrap_or_default();
        let claim_id = match parse_u256_arg(&call.args, "claimId") {
            Some(v) => v,
            None => return abi_err(call.call_id, "claimId required"),
        };
        if let Err(r) = self.ensure_allowed(&self.addrs.bounty_accrual, &call.call_id) {
            return r;
        }
        let signer = match self.signer_for(&bee) {
            Ok(s) => s,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .on_http(url);
        let bounty_addr = match Address::from_str(&self.addrs.bounty_accrual) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("bounty_accrual addr: {e}")),
        };
        let contract = BountyAccrual::new(bounty_addr, &provider);
        match contract.claimBounty(claim_id).send().await {
            Ok(pending) => match pending.get_receipt().await {
                Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({})),
                Err(e) => send_err(call.call_id, format!("receipt: {e}")),
            },
            Err(e) => send_err(call.call_id, format!("send: {e}")),
        }
    }

    // ----------- treasury / credit tools -----------

    async fn request_loan(&self, call: ToolCall) -> ToolResult {
        let bee = call.as_bee.clone().unwrap_or_default();
        let amount = match parse_u256_arg(&call.args, "amount") {
            Some(v) => v,
            None => return abi_err(call.call_id, "amount required"),
        };
        if let Err(r) = self.ensure_allowed(&self.addrs.benevolence, &call.call_id) {
            return r;
        }
        let signer = match self.signer_for(&bee) {
            Ok(s) => s,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .on_http(url);
        let benev_addr = match Address::from_str(&self.addrs.benevolence) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
        };
        let contract = Benevolence::new(benev_addr, &provider);
        match contract.requestLoan(amount).send().await {
            Ok(pending) => match pending.get_receipt().await {
                Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"amount": amount.to_string()})),
                Err(e) => send_err(call.call_id, format!("receipt: {e}")),
            },
            Err(e) => send_err(call.call_id, format!("send: {e}")),
        }
    }

    async fn request_loan_with_sig(&self, call: ToolCall) -> ToolResult {
        let bee = call.as_bee.clone().unwrap_or_default();
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
        if let Err(r) = self.ensure_allowed(&self.addrs.benevolence, &call.call_id) {
            return r;
        }
        let signer = match self.signer_for(&bee) {
            Ok(s) => s,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .on_http(url);
        let benev_addr = match Address::from_str(&self.addrs.benevolence) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
        };
        let contract = Benevolence::new(benev_addr, &provider);
        let req = Benevolence::LoanRequest { borrower, amount, nonce, deadline };
        match contract.requestLoanWithSignature(req, signature).send().await {
            Ok(pending) => match pending.get_receipt().await {
                Ok(r) => ok_tx(
                    call.call_id,
                    r.transaction_hash,
                    json!({"borrower": format!("{borrower:#x}"), "amount": amount.to_string()}),
                ),
                Err(e) => send_err(call.call_id, format!("receipt: {e}")),
            },
            Err(e) => send_err(call.call_id, format!("send: {e}")),
        }
    }

    async fn repay(&self, call: ToolCall) -> ToolResult {
        let bee = call.as_bee.clone().unwrap_or_default();
        let amount = match parse_u256_arg(&call.args, "amount") {
            Some(v) => v,
            None => return abi_err(call.call_id, "amount required"),
        };
        if let Err(r) = self.ensure_allowed(&self.addrs.benevolence, &call.call_id) {
            return r;
        }
        let signer = match self.signer_for(&bee) {
            Ok(s) => s,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .on_http(url);
        let usdc_addr = match Address::from_str(&self.addrs.usdc) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("usdc addr: {e}")),
        };
        let benev_addr = match Address::from_str(&self.addrs.benevolence) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
        };
        let usdc = Erc20::new(usdc_addr, &provider);
        if let Err(e) = usdc.approve(benev_addr, amount).send().await {
            return send_err(call.call_id, format!("approve: {e}"));
        }
        let contract = Benevolence::new(benev_addr, &provider);
        match contract.repay(amount).send().await {
            Ok(pending) => match pending.get_receipt().await {
                Ok(r) => ok_tx(call.call_id, r.transaction_hash, json!({"amount": amount.to_string()})),
                Err(e) => send_err(call.call_id, format!("receipt: {e}")),
            },
            Err(e) => send_err(call.call_id, format!("send: {e}")),
        }
    }

    async fn sign_loan_request(&self, call: ToolCall) -> ToolResult {
        // Pure off-chain: build EIP-712 signature via daman-credit-policy.
        let bee = call.as_bee.clone().or_else(|| Some(call.from.clone())).unwrap_or_default();
        let signer = match self.signer_for(&bee) {
            Ok(s) => s,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
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
        let benev_addr = match Address::from_str(&self.addrs.benevolence) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("benevolence addr: {e}")),
        };
        match daman_credit_policy::sign_loan_request(
            &signer,
            self.chain_id,
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

    // ----------- read tools -----------

    async fn read_leader_state(&self, call: ToolCall) -> ToolResult {
        let leader = match parse_addr_arg(&call.args, "leader") {
            Some(a) => a,
            None => return abi_err(call.call_id, "leader address required"),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new().on_http(url);
        let copy_bond = match Address::from_str(&self.addrs.copy_bond) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("copy_bond addr: {e}")),
        };
        let contract = CopyBond::new(copy_bond, &provider);
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

    async fn read_subscription_state(&self, call: ToolCall) -> ToolResult {
        not_yet_wired(
            call,
            "read_subscription_state needs CopyBond.getSubscription public getter; sidecar event indexer (daman-oracle) is the current read path",
        )
    }

    async fn read_reputation(&self, call: ToolCall) -> ToolResult {
        let agent = match parse_addr_arg(&call.args, "agent") {
            Some(a) => a,
            None => return abi_err(call.call_id, "agent address required"),
        };
        let url = match self.rpc_url_parsed() {
            Ok(u) => u,
            Err(e) => return ToolResult::fail(call.call_id, e),
        };
        let provider = ProviderBuilder::new().on_http(url);
        let reg = match Address::from_str(&self.addrs.reputation_registry) {
            Ok(a) => a,
            Err(e) => return abi_err(call.call_id, &format!("reputation addr: {e}")),
        };
        let contract = ReputationRegistry::new(reg, &provider);
        let score = match contract.reputationScore(agent).call().await {
            Ok(r) => r._0,
            Err(e) => return send_err(call.call_id, format!("read: {e}")),
        };
        let upheld = contract
            .cumulativeUpheld(agent)
            .call()
            .await
            .map(|r| r._0)
            .unwrap_or(U256::ZERO);
        let rejected = contract
            .cumulativeRejected(agent)
            .call()
            .await
            .map(|r| r._0)
            .unwrap_or(U256::ZERO);
        ToolResult::ok(
            call.call_id,
            json!({
                "score": score.to_string(),
                "cumulativeUpheld": upheld.to_string(),
                "cumulativeRejected": rejected.to_string(),
            }),
        )
    }

    async fn read_active_claims(&self, call: ToolCall) -> ToolResult {
        not_yet_wired(
            call,
            "read_active_claims scans DegradationFlagged event logs via daman-oracle; the forager does not index claims itself",
        )
    }

    async fn subscribe_to_role_events(&self, call: ToolCall) -> ToolResult {
        let role = call.args.get("role").and_then(|v| v.as_str()).unwrap_or("unknown");
        ToolResult::ok(
            call.call_id,
            json!({
                "accepted": true,
                "role": role,
                "channels": ["daman/slash/observability", "daman/credit/p2p", "daman/credit/observability"],
            }),
        )
    }
}

#[async_trait]
impl Tool for Handler {
    fn name(&self) -> &'static str {
        "daman-arc-fs-handler"
    }
    fn idempotency(&self) -> Idempotency {
        Idempotency::NotIdempotent
    }
    async fn invoke(&self, call: ToolCall) -> ToolResult {
        self.dispatch(call).await
    }
}

// -------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------

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

fn not_yet_wired(call: ToolCall, note: &str) -> ToolResult {
    ToolResult::ok(
        call.call_id,
        json!({
            "stub": true,
            "note": note,
            "tool": call.tool_name,
        }),
    )
}
