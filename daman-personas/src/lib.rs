//! # daman-personas
//!
//! Five persona role implementations: leader, follower, watchdog, arbiter, relief. Each
//! conforms to the `PersonaBee` trait from `persona-base` (the substrate scaffolding).
//!
//! A persona is a thin asker. Per the operating-model standard, persona logic contains no
//! decision-making: the persona assembles world state from gossip + chain events and emits
//! a prompt on its sid; the local `claude-cli` worker reasons; the local `daman-arc-fs`
//! forager executes the worker's tool calls; the persona logs the cycle.
//!
//! Variant overlays parametrize the system prompt per persona instance (alpha vs bravo,
//! v1 vs v2, etc.). The brief specifies overlays as one-line strings; this crate stores
//! them as data not code so adding a new variant doesn't require a new struct.
//!
//! See <https://reverbprotocol.github.io/protocol/OPERATING_MODEL#the-persona-bee-contract>.

pub mod variant;
pub mod personas;

pub use personas::{
    ArbiterPersona, FollowerPersona, LeaderPersona, ReliefPersona, WatchdogPersona,
};
pub use variant::{role_base_prompt, variant_overlay, Role};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
