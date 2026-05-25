//! # daman-operator
//!
//! Single-purpose persona binary that holds the deployer EOA private key and submits the
//! two `msg.sender ==` gated entry points on `DamanCopyBond`:
//!
//! - `recordTrade(leader, asset, amount, isLong)` must come from `oracle`
//! - `arbiterRule(claimId, slashAmount, upheld, builder, traceCid)` must come from `arbiterAddr`
//!
//! Both slots are currently set to the deployer EOA per the proxy's `initialize` write,
//! and the implementation exposes no setters. Path B from `/tmp/audit/auth_ops.md`: run a
//! dedicated operator daemon that plays both roles in-process. No on-chain rotation.
//!
//! See `/tmp/audit/operator_persona.md` for the design.

pub mod specs;
pub mod tools;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
