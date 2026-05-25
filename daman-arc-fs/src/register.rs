//! Boot-time on-chain identity anchor for a persona forager.
//!
//! Every persona binary calls [`register_agent`] before opening its humd
//! handshake. The call ensures the bee's EOA is anchored on
//! `DamanAgentRegistry` with its role bytes32, so downstream consumers
//! (cinematic ParticipantsLens, reputation accumulator, reviewer tooling)
//! can read the role-of-record without depending on whichever claude turn
//! happened to fire the equivalent tool call. Deterministic, not
//! discretionary.
//!
//! Treats `AlreadyRegistered` as success — the registry's role anchor is
//! immutable per the contract, and a second call from the same address
//! reverts; that's the expected steady-state.

use std::str::FromStr;

use alloy::network::EthereumWallet;
use alloy::primitives::{keccak256, Address, B256};
use alloy::providers::ProviderBuilder;

use crate::contracts::AgentRegistry;
use crate::factories::DamanCtx;

/// Outcome of a boot-time on-chain registration attempt.
#[derive(Debug)]
pub enum RegisterOutcome {
    /// First-time registration; tx landed at the returned hash.
    Registered(B256),
    /// The EOA was already anchored under some role from a prior boot;
    /// the registry rejected the duplicate write. Treated as success.
    AlreadyRegistered,
}

/// Anchor the persona's EOA on `DamanAgentRegistry.register(role)` where
/// `role` is keccak256 of the role-name string. Pre-checks `isRegistered`
/// to skip a doomed submit when the EOA is already on file.
///
/// Returns a string error if either the contract reach or the tx submit
/// itself fails for non-AlreadyRegistered reasons. Persona binaries log
/// the error and proceed; the network-level identity is best-effort at
/// boot, not a hard gate (a bee can still execute tool-calls; the gap is
/// only that its role anchor on chain may lag a turn).
pub async fn register_agent(ctx: &DamanCtx, role: &str) -> Result<RegisterOutcome, String> {
    let role_hash: B256 = keccak256(role.as_bytes());
    let url = reqwest::Url::parse(&ctx.rpc_url).map_err(|e| format!("rpc url: {e}"))?;
    let wallet = EthereumWallet::from(ctx.signer.clone());
    let provider = ProviderBuilder::new().with_recommended_fillers().wallet(wallet).on_http(url);

    let addr = Address::from_str(&ctx.addrs.agent_registry)
        .map_err(|e| format!("agent_registry addr: {e}"))?;
    let contract = AgentRegistry::new(addr, &provider);

    let me = ctx.signer.address();
    if let Ok(reg) = contract.isRegistered(me).call().await {
        if reg._0 {
            return Ok(RegisterOutcome::AlreadyRegistered);
        }
    }

    match contract.register(role_hash).send().await {
        Ok(p) => match p.get_receipt().await {
            Ok(r) => Ok(RegisterOutcome::Registered(r.transaction_hash)),
            Err(e) => Err(format!("receipt: {e}")),
        },
        Err(e) => {
            // Some RPCs return the revert as a wrapped error; treat
            // AlreadyRegistered as success so a race between pre-check
            // and submit doesn't fail the boot.
            let s = e.to_string();
            if s.contains("AlreadyRegistered") {
                Ok(RegisterOutcome::AlreadyRegistered)
            } else {
                Err(format!("send: {s}"))
            }
        }
    }
}
