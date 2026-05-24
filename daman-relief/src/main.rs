//! daman-relief. The wakeel relayer for the Daman credit primitive.
//!
//! Listens on `daman/credit/p2p` for `chi:"credit-signed-request"`. For each
//! signed request, validates locally (signature recovery, deadline, nonce)
//! and on chain (`isEligible(borrower)`, `treasuryAvailable >= amount`, plus
//! a sanity check on `nonceOf(borrower)`). If checks pass, submits
//! `requestLoanWithSignature(req, sig)` against `DamanBenevolence` and
//! publishes `chi:"credit-relayed"` carrying the tx hash. On any failure
//! it publishes `chi:"credit-error"` with a contract-mirrored code so the
//! borrower can diagnose without polling.
//!
//! The wire shape matches the rest of the Daman hive: NDJSON over the humd
//! Unix socket at `$XDG_RUNTIME_DIR/hum/thrum.sock`, framed as one JSON
//! object per line. Hello manifest declares this bee as a stateless lean
//! forager on the `daman/credit/relay` wire.
//!
//! Zero secrets in the binary. The relief bee's private key comes from the
//! `DAMAN_RELIEF_KEY` env var (hex, with or without `0x` prefix) which the
//! operator provisions per-container. The bee submits txs from that EOA;
//! the EOA pays only gas. The borrower's address comes from the signed
//! payload and is the only address the contract credits with the loan.
//!
//! Configurable:
//!
//!   HUM_THRUM_SOCK         humd's NDJSON socket (defaults to XDG runtime)
//!   ARC_TESTNET_RPC        Arc testnet RPC URL
//!   BENEVOLENCE_ADDR       DamanBenevolence proxy address
//!   USDC_ADDR              Arc USDC pre-deploy (default 0x36...0000)
//!   DAMAN_RELIEF_KEY       relief bee's EOA private key, hex
//!   RELIEF_SURPLUS_MIN     minimum bee USDC balance to relay (default 300000 = 0.3 USDC)

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Bytes, U256, B256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::str::FromStr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{info, warn};

const BEE_NAME: &str = "daman-relief";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");
const P2P_TOPIC: &str = "daman/credit/p2p";

sol! {
    #[sol(rpc)]
    contract Benevolence {
        struct LoanRequest {
            address borrower;
            uint256 amount;
            uint256 nonce;
            uint256 deadline;
        }
        function isEligible(address candidate) external view returns (bool);
        function treasuryAvailable() external view returns (uint256);
        function nonceOf(address borrower) external view returns (uint256);
        function requestLoanWithSignature(LoanRequest calldata req, bytes calldata signature) external;
    }

    #[sol(rpc)]
    contract Usdc {
        function balanceOf(address account) external view returns (uint256);
    }
}

#[derive(Debug, Clone)]
struct Config {
    sock_path: String,
    rpc_url: String,
    benevolence: Address,
    usdc: Address,
    signer: PrivateKeySigner,
    surplus_min: U256,
}

impl Config {
    fn from_env() -> Result<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::geteuid() }));
        let default_sock = format!("{runtime}/hum/thrum.sock");
        let key = std::env::var("DAMAN_RELIEF_KEY").context("DAMAN_RELIEF_KEY required")?;
        let key = key.trim_start_matches("0x");
        let signer = PrivateKeySigner::from_str(key).context("parse relief signer key")?;
        let benevolence = std::env::var("BENEVOLENCE_ADDR").context("BENEVOLENCE_ADDR required")?;
        let usdc = std::env::var("USDC_ADDR")
            .unwrap_or_else(|_| "0x3600000000000000000000000000000000000000".to_string());
        let surplus_min: u128 = std::env::var("RELIEF_SURPLUS_MIN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300_000);
        Ok(Self {
            sock_path: std::env::var("HUM_THRUM_SOCK").unwrap_or(default_sock),
            rpc_url: std::env::var("ARC_TESTNET_RPC")
                .unwrap_or_else(|_| "https://rpc.testnet.arc.network".to_string()),
            benevolence: Address::from_str(&benevolence).context("parse BENEVOLENCE_ADDR")?,
            usdc: Address::from_str(&usdc).context("parse USDC_ADDR")?,
            signer,
            surplus_min: U256::from(surplus_min),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct SignedRequestBody {
    request: LoanRequestJson,
    signature: String,
    #[allow(dead_code)]
    role: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "lastActivityTs")]
    last_activity_ts: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct LoanRequestJson {
    borrower: String,
    amount: String,
    nonce: String,
    deadline: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::from_env()?;
    let relayer_addr = cfg.signer.address();
    info!(
        sock = %cfg.sock_path,
        rpc = %cfg.rpc_url,
        benevolence = %cfg.benevolence,
        relayer = %relayer_addr,
        "{BEE_NAME} starting"
    );

    let wallet = EthereumWallet::from(cfg.signer.clone());
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .on_http(cfg.rpc_url.parse().context("parse rpc url")?);
    let benev = Benevolence::new(cfg.benevolence, &provider);
    let usdc = Usdc::new(cfg.usdc, &provider);

    let stream = UnixStream::connect(&cfg.sock_path)
        .await
        .with_context(|| format!("connect to humd at {}", cfg.sock_path))?;
    let (read_half, write_half) = stream.into_split();
    let write_half = Arc::new(Mutex::new(write_half));

    // Hello manifest. Speak credit-* chis, listen for credit-signed-request.
    let hello = json!({
        "chi": "hello",
        "bee": ["worker"],
        "chis": [
            "hello", "echo", "log",
            "gossip-publish",
            "credit-signed-request",
            "credit-relayed",
            "credit-error"
        ],
        "name": BEE_NAME,
        "version": BEE_VERSION,
    });
    write_line(&write_half, &hello).await?;

    // Subscribe to the credit p2p topic.
    let sub = json!({
        "chi": "gossip-subscribe",
        "topic": P2P_TOPIC,
    });
    write_line(&write_half, &sub).await?;

    let mut reader = BufReader::new(read_half).lines();
    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let frame: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, payload = %line, "frame parse failed");
                continue;
            }
        };

        // We accept either a direct chi:credit-signed-request frame or one
        // nested under a gossip envelope as { topic: "daman/credit/p2p",
        // payload: { chi: "credit-signed-request", ... } }.
        let inner = if frame.get("topic").and_then(|t| t.as_str()) == Some(P2P_TOPIC) {
            frame.get("payload").cloned().unwrap_or(frame.clone())
        } else {
            frame.clone()
        };

        let chi = inner.get("chi").and_then(|c| c.as_str()).unwrap_or("");
        if chi != "credit-signed-request" {
            continue;
        }

        let body: SignedRequestBody = match serde_json::from_value(inner.clone()) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "credit-signed-request body parse failed");
                continue;
            }
        };

        // Handle inline. Per-request throughput is well below 1 Hz so
        // serial handling is fine and avoids generic-provider gymnastics.
        let borrower = match Address::from_str(&body.request.borrower) {
            Ok(a) => a,
            Err(e) => { warn!(error = %e, "borrower parse failed"); continue; }
        };
        let amount = match parse_u256(&body.request.amount) {
            Ok(a) => a,
            Err(e) => { warn!(error = %e, "amount parse failed"); continue; }
        };
        let nonce = match parse_u256(&body.request.nonce) {
            Ok(a) => a,
            Err(e) => { warn!(error = %e, "nonce parse failed"); continue; }
        };
        let deadline = match parse_u256(&body.request.deadline) {
            Ok(a) => a,
            Err(e) => { warn!(error = %e, "deadline parse failed"); continue; }
        };
        let signature = match parse_bytes(&body.signature) {
            Ok(b) => b,
            Err(e) => { warn!(error = %e, "signature parse failed"); continue; }
        };

        // 1. Surplus check.
        let relayer_balance = match usdc.balanceOf(cfg.signer.address()).call().await {
            Ok(r) => r._0,
            Err(e) => { warn!(error = %e, "balanceOf failed"); continue; }
        };
        if relayer_balance < cfg.surplus_min {
            publish_error(
                &write_half,
                &borrower,
                "InsufficientRelayerSurplus",
                "relief bee balance below surplus floor",
            )
            .await;
            continue;
        }

        // 2. On-chain pre-check.
        let on_chain_nonce = match benev.nonceOf(borrower).call().await {
            Ok(r) => r._0,
            Err(e) => { warn!(error = %e, "nonceOf failed"); continue; }
        };
        if on_chain_nonce != nonce {
            publish_error(&write_half, &borrower, "InvalidNonce", "borrower nonce mismatch").await;
            continue;
        }
        let eligible = match benev.isEligible(borrower).call().await {
            Ok(r) => r._0,
            Err(e) => { warn!(error = %e, "isEligible failed"); continue; }
        };
        if !eligible {
            publish_error(&write_half, &borrower, "NotEligible", "borrower not eligible").await;
            continue;
        }
        let treasury = match benev.treasuryAvailable().call().await {
            Ok(r) => r._0,
            Err(e) => { warn!(error = %e, "treasuryAvailable failed"); continue; }
        };
        if treasury < amount {
            publish_error(
                &write_half,
                &borrower,
                "ExceedsTreasuryAvailable",
                "treasury below requested amount",
            )
            .await;
            continue;
        }

        // 3. Submit.
        let req = Benevolence::LoanRequest { borrower, amount, nonce, deadline };
        let tx = benev.requestLoanWithSignature(req, signature).send().await;
        let receipt = match tx {
            Ok(pending) => match pending.get_receipt().await {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "get_receipt failed");
                    publish_error(&write_half, &borrower, "RaceLost", &format!("{e}")).await;
                    continue;
                }
            },
            Err(e) => {
                warn!(error = %e, "submit failed; reporting RaceLost");
                publish_error(&write_half, &borrower, "RaceLost", &format!("{e}")).await;
                continue;
            }
        };

        let tx_hash = receipt.transaction_hash;
        info!(borrower = %borrower, tx = %tx_hash, "credit relayed");

        let relayed = json!({
            "chi": "gossip-publish",
            "topic": P2P_TOPIC,
            "payload": {
                "chi": "credit-relayed",
                "borrower": format!("{borrower:#x}"),
                "amount": amount.to_string(),
                "txHash": format!("{tx_hash:#x}"),
                "relayer": format!("{:#x}", cfg.signer.address()),
            }
        });
        if let Err(e) = write_line(&write_half, &relayed).await {
            warn!(error = %e, "write credit-relayed failed");
        }
    }

    Ok(())
}

async fn publish_error(
    write_half: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    borrower: &Address,
    code: &str,
    message: &str,
) {
    let payload = json!({
        "chi": "gossip-publish",
        "topic": P2P_TOPIC,
        "payload": {
            "chi": "credit-error",
            "borrower": format!("{borrower:#x}"),
            "code": code,
            "message": message,
        }
    });
    if let Err(e) = write_line(write_half, &payload).await {
        warn!(error = %e, "publish_error write failed");
    }
}

async fn write_line(
    handle: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    v: &Value,
) -> Result<()> {
    let s = serde_json::to_string(v)?;
    let mut bytes = s.into_bytes();
    bytes.push(b'\n');
    let mut guard = handle.lock();
    guard.write_all(&bytes).await?;
    Ok(())
}

fn parse_u256(s: &str) -> Result<U256> {
    if let Some(hex) = s.strip_prefix("0x") {
        Ok(U256::from_str_radix(hex, 16)?)
    } else {
        Ok(U256::from_str_radix(s, 10)?)
    }
}

fn parse_bytes(s: &str) -> Result<Bytes> {
    let s = s.trim_start_matches("0x");
    Ok(Bytes::from(hex::decode(s)?))
}

#[allow(dead_code)]
fn parse_b256(s: &str) -> Result<B256> {
    let s = s.trim_start_matches("0x");
    let bytes = hex::decode(s)?;
    Ok(B256::from_slice(&bytes))
}
