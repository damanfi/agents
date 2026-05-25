//! # daman-arc-fs
//!
//! Daman's forager extension to `reverb-arc-fs`. Adds 17 high-level tools that compose the
//! base `arc_*` primitives plus the Daman contract surface (DamanCopyBond,
//! DamanBountyAccrual, DamanReputationRegistry, DamanBenevolence, UniverseRegistry, plus
//! the substrate's RefundProtocolFixed for follower refund claims).
//!
//! Persona bees emit `chi:"tool-call"` carrying a `tool_name` from this surface. humd routes
//! by `tool_name` to this forager. The forager runs the six-stage safety pipeline (auth, ABI
//! validation, simulation gate, rate limit, send, receipt cache) inherited from `reverb-arc-fs`,
//! then submits via the keyring's EOA bound to the calling bee, then emits `chi:"tool-result"`
//! back via humd's `tool_routes[callId]` reverse map.
//!
//! No reasoning inside the forager. The forager is the executor. Reasoning lives in the
//! worker bee's cell (claude-cli); the persona is the thin asker.
//!
//! Spec: <https://reverbprotocol.github.io/protocol/OPERATING_MODEL#the-forager-hive-contract>
//! plus the daman-side `damanfi/agents/daman-arc-fs/README.md` for the tool table.

pub mod hello;
pub mod tools;
pub mod tools_defs;
pub mod handler;

pub use hello::build_hello;
pub use handler::Handler;
pub use tools::DamanAddrs;
