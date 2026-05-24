//! Tool metadata catalog for `daman-arc-fs`.
//!
//! The 17 daman-specific tools are dispatched by name through the unified [`Handler`] in
//! [`crate::handler`]. This module exports the canonical metadata (name, idempotency,
//! target contract) used by the hello manifest builder and by the `allowed_contracts` gate.
//!
//! [`Handler`]: crate::handler::Handler

use reverb_arc_fs::tools::Idempotency;

/// Daman contract addresses on Arc testnet. Mirrors
/// `damanfi/copy-bond/.deployments/arc-testnet.json`. Operator may override at boot via
/// env if running against a fork or a future re-deploy.
#[derive(Debug, Clone)]
pub struct DamanAddrs {
    pub copy_bond: String,
    pub bounty_accrual: String,
    pub reputation_registry: String,
    pub bond_yield_vault: String,
    pub universe_registry: String,
    pub benevolence: String,
    pub agent_registry: String,
    pub refund_protocol: String,
    pub usdc: String,
}

impl Default for DamanAddrs {
    fn default() -> Self {
        Self {
            copy_bond: "0x493085c71f3CaceB8373db6e6ffeF43EacbC3e02".into(),
            bounty_accrual: "0xF0Dc40875f56D0703B4C9e3823ACa5d9d9E73F16".into(),
            reputation_registry: "0xAA1a021215322FbB775c6Cc08d81347864a7Ac94".into(),
            bond_yield_vault: "0xe98b4695753D03B644c063C0bb3A3bdd01Cc50dD".into(),
            universe_registry: "0xfea80c061a9ed8a25b33e0b6b9f1490bdb10d270".into(),
            benevolence: "0xd66812b02F2CA8C057e68e2E80e8c22500A3b9aD".into(),
            agent_registry: "0x4b214C6CDCcE4b00e692BE44AD19d652C7F9FB6a".into(),
            refund_protocol: "0xc8bF99c55703bc682a3Efd5c8A728EaEda3E121F".into(),
            usdc: "0x3600000000000000000000000000000000000000".into(),
        }
    }
}

/// Tool metadata. Used by the hello manifest + the allow-list gate.
#[derive(Debug, Clone, Copy)]
pub struct ToolMeta {
    pub name: &'static str,
    pub idempotency: Idempotency,
    pub target: TargetContract,
}

/// Which contract a tool ultimately writes to. Read-only tools that traverse multiple
/// contracts are `AnyRead`.
#[derive(Debug, Clone, Copy)]
pub enum TargetContract {
    CopyBond,
    BountyAccrual,
    Benevolence,
    RefundProtocol,
    AnyRead,
}

/// The full 17-tool catalog.
pub fn catalog() -> Vec<ToolMeta> {
    vec![
        ToolMeta { name: "daman_register_leader", idempotency: Idempotency::NotIdempotent, target: TargetContract::CopyBond },
        ToolMeta { name: "daman_record_trade", idempotency: Idempotency::NotIdempotent, target: TargetContract::CopyBond },
        ToolMeta { name: "daman_subscribe", idempotency: Idempotency::NotIdempotent, target: TargetContract::CopyBond },
        ToolMeta { name: "daman_unsubscribe", idempotency: Idempotency::NotIdempotent, target: TargetContract::CopyBond },
        ToolMeta { name: "daman_claim_refund", idempotency: Idempotency::NotIdempotent, target: TargetContract::RefundProtocol },
        ToolMeta { name: "daman_file_claim", idempotency: Idempotency::NotIdempotent, target: TargetContract::CopyBond },
        ToolMeta { name: "daman_rule_claim", idempotency: Idempotency::NotIdempotent, target: TargetContract::CopyBond },
        ToolMeta { name: "daman_claim_bounty", idempotency: Idempotency::NotIdempotent, target: TargetContract::BountyAccrual },
        ToolMeta { name: "daman_request_loan", idempotency: Idempotency::NotIdempotent, target: TargetContract::Benevolence },
        ToolMeta { name: "daman_request_loan_with_signature", idempotency: Idempotency::NotIdempotent, target: TargetContract::Benevolence },
        ToolMeta { name: "daman_repay", idempotency: Idempotency::NotIdempotent, target: TargetContract::Benevolence },
        // Pure off-chain sign; idempotent.
        ToolMeta { name: "daman_sign_loan_request", idempotency: Idempotency::Idempotent, target: TargetContract::Benevolence },
        ToolMeta { name: "daman_read_leader_state", idempotency: Idempotency::Idempotent, target: TargetContract::AnyRead },
        ToolMeta { name: "daman_read_subscription_state", idempotency: Idempotency::Idempotent, target: TargetContract::AnyRead },
        ToolMeta { name: "daman_read_reputation", idempotency: Idempotency::Idempotent, target: TargetContract::AnyRead },
        ToolMeta { name: "daman_read_active_claims", idempotency: Idempotency::Idempotent, target: TargetContract::AnyRead },
        ToolMeta { name: "daman_subscribe_to_role_events", idempotency: Idempotency::Idempotent, target: TargetContract::AnyRead },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_17_tools() {
        assert_eq!(catalog().len(), 17);
    }

    #[test]
    fn catalog_covers_every_brief_tool() {
        let names: Vec<&str> = catalog().iter().map(|t| t.name).collect();
        for expected in [
            "daman_register_leader",
            "daman_record_trade",
            "daman_subscribe",
            "daman_unsubscribe",
            "daman_claim_refund",
            "daman_file_claim",
            "daman_rule_claim",
            "daman_claim_bounty",
            "daman_request_loan",
            "daman_request_loan_with_signature",
            "daman_repay",
            "daman_sign_loan_request",
            "daman_read_leader_state",
            "daman_read_subscription_state",
            "daman_read_reputation",
            "daman_read_active_claims",
            "daman_subscribe_to_role_events",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
    }

    #[test]
    fn default_addrs_match_deployed_proxies() {
        let a = DamanAddrs::default();
        assert_eq!(a.benevolence, "0xd66812b02F2CA8C057e68e2E80e8c22500A3b9aD");
        assert_eq!(a.agent_registry, "0x4b214C6CDCcE4b00e692BE44AD19d652C7F9FB6a");
        assert_eq!(a.copy_bond, "0x493085c71f3CaceB8373db6e6ffeF43EacbC3e02");
    }

    #[test]
    fn write_tools_are_not_idempotent_except_signing() {
        for t in catalog() {
            if matches!(t.target, TargetContract::AnyRead) {
                continue;
            }
            if t.name == "daman_sign_loan_request" {
                assert_eq!(t.idempotency, Idempotency::Idempotent);
                continue;
            }
            assert_eq!(t.idempotency, Idempotency::NotIdempotent, "{}", t.name);
        }
    }
}
