//! daman-watchdog. A stateful degradation detector for slash-bonded copy-trading.
//!
//! Subscribes to the Daman hive vocabulary on hum. Listens for
//! `trade-executed` and `settlement-completed` chis (sourced only
//! from the operator-side oracle per ADR-001). Maintains a per-leader
//! rolling window of trades and settlements; flags degradation when
//! the configured policy threshold is crossed; emits `slash-claim`
//! chis for the bridge forager to dispatch on chain.
//!
//! Wire shape compatible with thrum-core. NDJSON over a UnixStream
//! socket served by humd. The serde structs below are the local
//! mirror of the documented chi schema in `damanfi/protocol::HiveVocabulary.md`.
//!
//! Stateless across restarts in this reference: rolling windows live
//! in memory only. Production deployments should persist the window
//! to disk so that a restart does not lose evidence accumulation.

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{info, warn};
use uuid::Uuid;

const BEE_NAME: &str = "daman-watchdog";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default rolling-window size in number of settlements per leader.
const DEFAULT_WINDOW_SIZE: usize = 50;
/// Default loss-streak threshold: this many consecutive losing settlements
/// trigger a degradation claim. Reference policy only.
const DEFAULT_LOSS_STREAK_THRESHOLD: u32 = 5;

#[derive(Debug, Clone)]
struct Config {
    sock_path: String,
    window_size: usize,
    loss_streak_threshold: u32,
}

impl Config {
    fn from_env() -> Result<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
            format!("/run/user/{}", unsafe { libc::geteuid() })
        });
        let default_sock = format!("{runtime}/hum/thrum.sock");
        Ok(Self {
            sock_path: std::env::var("HUM_THRUM_SOCK").unwrap_or(default_sock),
            window_size: std::env::var("DAMAN_WATCHDOG_WINDOW_SIZE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_WINDOW_SIZE),
            loss_streak_threshold: std::env::var("DAMAN_WATCHDOG_LOSS_STREAK")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_LOSS_STREAK_THRESHOLD),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "chi")]
#[allow(dead_code, clippy::large_enum_variant)]
enum InboundFrame {
    #[serde(rename = "hello")]
    Hello(Value),
    #[serde(rename = "trade-executed")]
    TradeExecuted { args: TradeExecuted },
    #[serde(rename = "settlement-completed")]
    SettlementCompleted { args: SettlementCompleted },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
struct TradeExecuted {
    leader: String,
    #[allow(dead_code)]
    asset: String,
    #[allow(dead_code)]
    amount: String,
    #[allow(dead_code)]
    is_long: bool,
    timestamp: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct SettlementCompleted {
    leader: String,
    trade_id: String,
    /// Signed integer encoded as a hex string ("0x..." big-endian, twos-complement).
    pnl: String,
    #[allow(dead_code)]
    timestamp: u64,
}

#[derive(Debug, Default)]
struct LeaderState {
    settlements: VecDeque<i128>,
    loss_streak: u32,
}

type StateMap = Arc<Mutex<HashMap<String, LeaderState>>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::from_env()?;
    info!(
        sock = %cfg.sock_path,
        window = cfg.window_size,
        loss_streak = cfg.loss_streak_threshold,
        "{BEE_NAME} starting"
    );

    let state: StateMap = Arc::new(Mutex::new(HashMap::new()));

    let stream = UnixStream::connect(&cfg.sock_path)
        .await
        .with_context(|| format!("connect to humd at {}", cfg.sock_path))?;
    let (read_half, mut write_half) = stream.into_split();

    let hello = json!({
        "chi": "hello",
        "bee": ["worker"],
        "chis": [
            "trade-executed",
            "settlement-completed",
            "slash-claim",
            "degradation-detected"
        ],
        "name": BEE_NAME,
        "version": BEE_VERSION,
    });
    write_line(&mut write_half, &hello).await?;

    let mut reader = BufReader::new(read_half).lines();
    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let frame: InboundFrame = match serde_json::from_str(&line) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, payload = %line, "frame parse failed");
                continue;
            }
        };
        match frame {
            InboundFrame::TradeExecuted { args } => {
                // Trades populate the activity record but do not change
                // the loss-streak; only settlement PnL does.
                tracing::debug!(leader = %args.leader, ts = args.timestamp, "trade observed");
            }
            InboundFrame::SettlementCompleted { args } => {
                let pnl = parse_signed_hex(&args.pnl).unwrap_or(0);
                let mut s = state.lock();
                let entry = s.entry(args.leader.clone()).or_default();
                entry.settlements.push_back(pnl);
                if entry.settlements.len() > cfg.window_size {
                    entry.settlements.pop_front();
                }
                if pnl < 0 {
                    entry.loss_streak = entry.loss_streak.saturating_add(1);
                } else {
                    entry.loss_streak = 0;
                }
                let streak = entry.loss_streak;
                drop(s);

                if streak >= cfg.loss_streak_threshold {
                    let evidence_hash = synthesize_evidence_hash(&args.leader, &args.trade_id);
                    let policy = format!("loss-streak >= {}", cfg.loss_streak_threshold);
                    let claim_nonce = Uuid::new_v4().to_string();

                    // Emit the slash-claim immediately (on-chain critical path).
                    let claim = json!({
                        "chi": "slash-claim",
                        "args": {
                            "leader": args.leader,
                            "evidenceHash": evidence_hash,
                            "policy": &policy,
                            "watchdog": BEE_NAME,
                            "claimNonce": &claim_nonce,
                        }
                    });
                    info!(leader = %args.leader, streak, "emitting slash-claim");
                    write_line(&mut write_half, &claim).await?;

                    // Emit a parallel reasoning-trace pin request (off-chain
                    // audit surface). The trace-pinner forager picks this up
                    // and replies with chi:trace-pinned carrying the CID.
                    // The CID lands on chain in the A1 follow-on alongside
                    // BountyAccrual + ReputationRegistry.
                    let trace = json!({
                        "chi": "gossip-publish",
                        "topic": "daman/trace",
                        "payload": {
                            "chi": "pin-trace",
                            "args": {
                                "trace_json": {
                                    "agent": BEE_NAME,
                                    "decision": "slash-claim",
                                    "leader": args.leader,
                                    "policy": &policy,
                                    "loss_streak": streak,
                                    "settlement_window": cfg.window_size,
                                    "evidence_hash": &evidence_hash,
                                    "claim_nonce": &claim_nonce,
                                },
                                "metadata": {
                                    "agent": BEE_NAME,
                                    "version": env!("CARGO_PKG_VERSION"),
                                },
                                "request_id": &claim_nonce,
                            }
                        }
                    });
                    write_line(&mut write_half, &trace).await?;

                    // Reset the streak so we do not retrigger immediately.
                    state.lock().entry(args.leader).or_default().loss_streak = 0;
                }
            }
            InboundFrame::Hello(_) | InboundFrame::Other => {}
        }
    }

    Ok(())
}

async fn write_line<W: AsyncWriteExt + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    let s = serde_json::to_string(v)?;
    w.write_all(s.as_bytes()).await?;
    w.write_all(b"\n").await?;
    Ok(())
}

fn parse_signed_hex(s: &str) -> Result<i128> {
    let stripped = s.trim_start_matches("0x");
    if stripped.is_empty() {
        return Err(anyhow!("empty hex"));
    }
    let unsigned = u128::from_str_radix(stripped, 16).context("parse hex")?;
    // Twos-complement interpretation across the full 128-bit range.
    Ok(unsigned as i128)
}

fn synthesize_evidence_hash(leader: &str, trade_id: &str) -> String {
    // Reference policy only: hash the concatenation. Production
    // watchdogs assemble structured evidence and hash that.
    let mut bytes = Vec::with_capacity(leader.len() + trade_id.len());
    bytes.extend_from_slice(leader.as_bytes());
    bytes.extend_from_slice(trade_id.as_bytes());
    format!("0x{}", hex_encode_32(&bytes))
}

fn hex_encode_32(input: &[u8]) -> String {
    // Cheap, dependency-free 32-byte synthesizer (truncates or pads).
    let mut out = [0u8; 32];
    let take = input.len().min(32);
    out[..take].copy_from_slice(&input[..take]);
    let mut s = String::with_capacity(64);
    for b in out {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_signed_hex_handles_positive() {
        assert_eq!(parse_signed_hex("0x10").unwrap(), 16);
    }

    #[test]
    fn evidence_hash_is_32_bytes() {
        let h = synthesize_evidence_hash("0xabc", "0xdef");
        assert_eq!(h.len(), 2 + 64);
        assert!(h.starts_with("0x"));
    }
}
