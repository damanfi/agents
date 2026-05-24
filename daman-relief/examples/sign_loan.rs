//! End-to-end relay test: signs a LoanRequest using daman-credit-policy
//! then prints the body the relief bee would receive. Run with:
//!
//!   cargo run -p daman-relief --example sign_loan -- \
//!     <borrower_key> <chain_id> <verifying_contract> <amount> <nonce> <deadline>

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use daman_credit_policy::sign_loan_request;
use std::str::FromStr;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 7 {
        eprintln!(
            "usage: sign_loan <borrower_key> <chain_id> <verifying_contract> <amount> <nonce> <deadline>"
        );
        std::process::exit(2);
    }
    let key = args[1].trim_start_matches("0x");
    let signer = PrivateKeySigner::from_str(key)?;
    let chain_id: u64 = args[2].parse()?;
    let verifying = Address::from_str(&args[3])?;
    let amount = U256::from_str_radix(&args[4], 10)?;
    let nonce = U256::from_str_radix(&args[5], 10)?;
    let deadline = U256::from_str_radix(&args[6], 10)?;

    let body = sign_loan_request(&signer, chain_id, verifying, amount, nonce, deadline)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "borrower": body.borrower,
            "amount": body.amount,
            "nonce": body.nonce,
            "deadline": body.deadline,
            "signature": body.signature,
        }))?
    );
    Ok(())
}
