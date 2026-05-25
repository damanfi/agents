//! # daman-arc-fs (library)
//!
//! Library crate that consumer-product persona binaries import to compose their own
//! per-persona forager. Per BRIEF_PERSONA_AS_FORAGER + the substrate's
//! `reverb-arc-fs::PersonaForagerBuilder`, each persona binary builds one
//! `PersonaForager` holding exactly one EOA private key, one stable ed25519 hid,
//! one namespaced tool surface, one humd connection. Process boundary IS identity
//! boundary, matching humfs's per-instance `fs.roots` pattern.
//!
//! Public surface:
//! - [`DamanAddrs`] — the deployed proxy address book on Arc testnet.
//! - [`DamanCtx`] — per-bee config (signer + addrs + rpc) the tool factories close over.
//! - [`daman_tools`] — factory returning `Vec<Tool>` for one persona, all tools
//!   prefixed by the persona's namespace. See `factories::daman_tools` for the
//!   current count; tests assert it.
//! - [`namespace_for_bee`] — canonical bee_name → namespace mapping per the brief's
//!   convention (alpha, fol_v1_1, wd_v1_1, arb_v1, relief1, etc.).
//!
//! Typical persona-binary composition:
//!
//! ```ignore
//! use reverb_arc_fs::{PersonaForagerBuilder, BeeIdentity, BeeRole, PrivateKey};
//! use alloy::signers::local::PrivateKeySigner;
//! use std::str::FromStr;
//! use daman_arc_fs::{daman_tools, namespace_for_bee, DamanAddrs, DamanCtx};
//!
//! let bee_name = "daman-leader-alpha";
//! let ns = namespace_for_bee(bee_name);
//! let key_bytes = std::fs::read_to_string("/path/to/key").unwrap();
//! let signer = PrivateKeySigner::from_str(key_bytes.trim()).unwrap();
//! let pk = PrivateKey::new(format!("0x{}", key_bytes.trim())).unwrap();
//! let identity = BeeIdentity::load_or_mint_with_role(bee_name, BeeRole::Forager).unwrap();
//! let addrs = DamanAddrs::default();
//!
//! let ctx = DamanCtx::new(
//!     bee_name,
//!     "https://rpc.testnet.arc.network",
//!     5042002,
//!     addrs.clone(),
//!     signer,
//! );
//! let tools = daman_tools(ctx, &ns);
//!
//! let forager = PersonaForagerBuilder::default()
//!     .bee_name(bee_name)
//!     .namespace(ns)
//!     .identity(identity)
//!     .private_key(pk)
//!     .with_tools(tools)
//!     .allowed_contracts(addrs.allowlist())
//!     .wire("daman/arc-fs")
//!     .build()
//!     .unwrap();
//! ```
//!
//! Spec: <https://reverbprotocol.github.io/protocol/OPERATING_MODEL>
//! Hum hives contract: <https://adiled.github.io/hum/hives/>

pub mod addrs;
pub mod contracts;
pub mod credit_inbox;
pub mod factories;
pub mod namespacing;
pub mod register;
pub mod specs;
pub mod state_snapshot;

pub use addrs::DamanAddrs;
pub use credit_inbox::{
    inbox_dir, list_pending, mark_submitted, publish_request, PendingRequest, SignedLoanRequest,
};
pub use factories::{daman_tools, topics_for_role, DamanCtx};
pub use namespacing::namespace_for_bee;
pub use register::{register_agent, RegisterOutcome};
pub use specs::daman_tool_specs;
pub use state_snapshot::{fetch_bee_state, render_state_block, BeeState};
