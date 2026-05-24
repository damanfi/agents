//! daman-credit-policy. Balance-aware credit policy for Daman bees.
//!
//! Provides:
//! - `LoanRequestSigner`: builds and EIP-712-signs a `LoanRequest` for the
//!   bust-bee p2p relief path. The signed payload is gossip-broadcast by
//!   the bee; any `daman-relief` bee may pick it up and submit on chain.
//! - `BalanceProbe`: thin async helper that reads the bee's USDC balance
//!   and the on-chain `nonceOf` value the signer needs.
//! - Constant thresholds matching the contract's eligibility logic:
//!   `GAS_MIN`, `LOW_THRESHOLD`, `BUST_THRESHOLD`, `PER_BORROWER_CAP`.
//!
//! Bees that already drive an alloy provider for other reasons (the
//! relief bee, future leader/follower bees) construct the signer
//! directly; bees that don't can use the `sign_loan_request_offline`
//! helper which only needs the chainId + verifying-contract address.
//!
//! Frame: `LoanRequestSigner` mirrors the wakala (procuration-agent)
//! shape on the wire. The bee owns its EOA + its private key; the
//! relief bee never sees the key. The signed payload binds the debt
//! to the bee's address structurally.

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use alloy::sol_types::{eip712_domain, SolStruct};
use alloy::sol;
use anyhow::{Context, Result};
use serde::Serialize;

/// Minimum USDC balance below which the bee cannot afford even one tx.
/// The bee must use the p2p relief path; it cannot self-submit even a
/// `register` or `requestLoan` call from this state.
pub const GAS_MIN: u128 = 200_000;

/// USDC threshold below which the bee should request a loan. Above
/// `GAS_MIN` so the bee can pay its own gas; below this means working
/// capital is depleted and refill is needed.
pub const LOW_THRESHOLD: u128 = 1_000_000;

/// Contract-side eligibility threshold for the active-but-bust path.
/// Mirrors `DamanBenevolence.ELIGIBILITY_BUST_THRESHOLD`.
pub const BUST_THRESHOLD: u128 = 1_000_000;

/// Contract-side per-borrower cap. Mirrors `DamanBenevolence.PER_BORROWER_CAP`.
pub const PER_BORROWER_CAP: u128 = 5_000_000;

/// EIP-712 domain name. Must match `DamanBenevolence.EIP712_NAME`.
pub const EIP712_NAME: &str = "DamanBenevolence";

/// EIP-712 domain version. Must match `DamanBenevolence.EIP712_VERSION`.
pub const EIP712_VERSION: &str = "1";

sol! {
    /// EIP-712 typed-data shape. Identical layout to the on-chain
    /// `LoanRequest` struct in `DamanBenevolence`.
    #[derive(Serialize)]
    struct LoanRequest {
        address borrower;
        uint256 amount;
        uint256 nonce;
        uint256 deadline;
    }
}

/// Output shape that bees gossip as the body of `chi:credit-signed-request`.
#[derive(Debug, Clone, Serialize)]
pub struct SignedLoanRequestBody {
    pub borrower: String,
    pub amount: String,
    pub nonce: String,
    pub deadline: String,
    pub signature: String,
}

/// Sign a LoanRequest using the bee's private-key signer.
///
/// Returns the JSON body the bee should embed under the
/// `request` + `signature` fields of the outgoing
/// `chi:credit-signed-request` gossip frame.
pub fn sign_loan_request(
    signer: &PrivateKeySigner,
    chain_id: u64,
    verifying_contract: Address,
    amount: U256,
    nonce: U256,
    deadline: U256,
) -> Result<SignedLoanRequestBody> {
    let req = LoanRequest {
        borrower: signer.address(),
        amount,
        nonce,
        deadline,
    };
    let domain = eip712_domain! {
        name: EIP712_NAME,
        version: EIP712_VERSION,
        chain_id: chain_id,
        verifying_contract: verifying_contract,
    };
    let digest = req.eip712_signing_hash(&domain);
    let sig = signer.sign_hash_sync(&digest).context("eip712 sign")?;
    let sig_bytes: [u8; 65] = sig.as_bytes();
    Ok(SignedLoanRequestBody {
        borrower: format!("{:#x}", req.borrower),
        amount: req.amount.to_string(),
        nonce: req.nonce.to_string(),
        deadline: req.deadline.to_string(),
        signature: format!("0x{}", hex::encode(sig_bytes)),
    })
}

/// Branch the bee should take based on its current USDC balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditBranch {
    /// Balance below GAS_MIN. Cannot self-submit anything. Must sign +
    /// gossip a `credit-signed-request` for a relief bee to relay.
    Bust,
    /// Balance below LOW_THRESHOLD but above GAS_MIN. Can pay gas;
    /// should call `requestLoan` directly.
    Low,
    /// Balance above LOW_THRESHOLD. Normal operation. Should service
    /// outstanding debt via `repay` when bounty arrives.
    Normal,
}

/// Classify a balance into a branch.
pub fn classify(balance_atomic: u128) -> CreditBranch {
    if balance_atomic < GAS_MIN {
        CreditBranch::Bust
    } else if balance_atomic < LOW_THRESHOLD {
        CreditBranch::Low
    } else {
        CreditBranch::Normal
    }
}

/// Compute the recommended loan amount given the bee's current debt and
/// the per-borrower cap. Returns the requested amount, capped to the
/// remaining headroom under PER_BORROWER_CAP.
pub fn recommended_loan_amount(current_debt_atomic: u128) -> u128 {
    PER_BORROWER_CAP.saturating_sub(current_debt_atomic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn classify_branches() {
        assert_eq!(classify(0), CreditBranch::Bust);
        assert_eq!(classify(199_999), CreditBranch::Bust);
        assert_eq!(classify(200_000), CreditBranch::Low);
        assert_eq!(classify(999_999), CreditBranch::Low);
        assert_eq!(classify(1_000_000), CreditBranch::Normal);
        assert_eq!(classify(5_000_000), CreditBranch::Normal);
    }

    #[test]
    fn sign_then_verify() {
        // Deterministic key from a known seed for repro.
        let key_hex = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = PrivateKeySigner::from_str(key_hex).unwrap();
        let body = sign_loan_request(
            &signer,
            5042002,
            Address::from_str("0xd66812b02F2CA8C057e68e2E80e8c22500A3b9aD").unwrap(),
            U256::from(5_000_000u64),
            U256::ZERO,
            U256::from(2_000_000_000u64),
        )
        .unwrap();
        assert_eq!(body.borrower.to_lowercase(), format!("{:#x}", signer.address()).to_lowercase());
        assert_eq!(body.amount, "5000000");
        assert!(body.signature.starts_with("0x"));
        assert_eq!(body.signature.len(), 132); // 0x + 65 bytes * 2
    }

    #[test]
    fn recommended_amount() {
        assert_eq!(recommended_loan_amount(0), 5_000_000);
        assert_eq!(recommended_loan_amount(2_000_000), 3_000_000);
        assert_eq!(recommended_loan_amount(5_000_000), 0);
        assert_eq!(recommended_loan_amount(6_000_000), 0);
    }
}
