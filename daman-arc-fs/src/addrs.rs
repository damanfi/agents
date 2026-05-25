//! Address book for the Daman + substrate contract surface on Arc testnet.
//! Mirrors `damanfi/copy-bond/.deployments/arc-testnet.json`. Operator may override
//! at boot via env when running against a fork or a future re-deploy.

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

impl DamanAddrs {
    /// All contracts the forager will write to. Mirrors the allowed_contracts gate
    /// in `reverb_arc_fs::config`.
    pub fn allowlist(&self) -> Vec<String> {
        vec![
            self.copy_bond.clone(),
            self.bounty_accrual.clone(),
            self.reputation_registry.clone(),
            self.bond_yield_vault.clone(),
            self.universe_registry.clone(),
            self.benevolence.clone(),
            self.agent_registry.clone(),
            self.refund_protocol.clone(),
        ]
    }
}
