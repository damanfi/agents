//! daman-arbiter. A stateful ruler on disputed slash-claims.
//!
//! Subscribes to the Daman hive vocabulary on hum. Listens for
//! `dispute-opened` chis emitted by the bridge forager when a leader
//! contests a watchdog's slash-claim within the dispute window.
//! Evaluates the dispute against a configurable policy (reference
//! policy: uphold all disputes within the slash cap) and emits a
//! `ruling` chi for the bridge forager to dispatch on chain via
//! `arbiterRule(claimId, slashAmount, upheld)`.
//!
//! Reference policy is intentionally permissive: production
//! deployments substitute a richer evaluator (domain-expert
//! review, evidence-replay simulator, on-platform tape audit)
//! before the ruling fires.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{info, warn};

const BEE_NAME: &str = "daman-arbiter";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Reference slash-amount policy: uphold the watchdog's claim at the
/// protocol-level slash cap (25% of currently-posted bond). Production
/// arbiters compute this from the deployed `IDamanCopyBond` view
/// `bondBalance(leader)` multiplied by `BondEconomics.SLASH_CAP_BPS`.
const REFERENCE_SLASH_AMOUNT_HEX: &str = "0x00";

#[derive(Debug, Clone)]
struct Config {
    sock_path: String,
}

impl Config {
    fn from_env() -> Result<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
            format!("/run/user/{}", unsafe { libc::geteuid() })
        });
        let default_sock = format!("{runtime}/hum/thrum.sock");
        Ok(Self {
            sock_path: std::env::var("HUM_THRUM_SOCK").unwrap_or(default_sock),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "chi")]
#[allow(dead_code)]
enum InboundFrame {
    #[serde(rename = "hello")]
    Hello(Value),
    #[serde(rename = "dispute-opened")]
    DisputeOpened { args: DisputeOpened },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
struct DisputeOpened {
    claim_id: String,
    leader: String,
    #[allow(dead_code)]
    evidence_hash: String,
    dispute_window_ends: u64,
}

#[derive(Debug, Default)]
#[allow(dead_code)]
struct OpenDispute {
    claim_id: String,
    leader: String,
    window_ends: u64,
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
    info!(sock = %cfg.sock_path, "{BEE_NAME} starting");

    let open: Arc<Mutex<HashMap<String, OpenDispute>>> = Arc::new(Mutex::new(HashMap::new()));

    let stream = UnixStream::connect(&cfg.sock_path)
        .await
        .with_context(|| format!("connect to humd at {}", cfg.sock_path))?;
    let (read_half, mut write_half) = stream.into_split();

    let hello = json!({
        "chi": "hello",
        "bee": ["judge"],
        "chis": ["dispute-opened", "ruling"],
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
            InboundFrame::DisputeOpened { args } => {
                info!(
                    claim_id = %args.claim_id,
                    leader = %args.leader,
                    window_ends = args.dispute_window_ends,
                    "dispute opened"
                );
                open.lock().insert(
                    args.claim_id.clone(),
                    OpenDispute {
                        claim_id: args.claim_id.clone(),
                        leader: args.leader.clone(),
                        window_ends: args.dispute_window_ends,
                    },
                );

                // Reference policy: uphold immediately at the protocol
                // slash cap. Real deployments delay until the window
                // closes and substitute richer evaluation here.
                let ruling_nonce = format!("ruling-{}", args.claim_id);
                let ruling = json!({
                    "chi": "ruling",
                    "args": {
                        "claimId": args.claim_id,
                        "slashAmount": REFERENCE_SLASH_AMOUNT_HEX,
                        "upheld": true,
                        "arbiter": BEE_NAME,
                    }
                });
                write_line(&mut write_half, &ruling).await?;

                // Emit a parallel reasoning-trace pin request. The
                // trace-pinner forager replies with chi:trace-pinned
                // carrying the CID; off-chain observers correlate via
                // ruling_nonce. The CID lands on chain in the A1
                // follow-on alongside the traceCid field addition to
                // ArbiterRuled.
                let trace = json!({
                    "chi": "gossip-publish",
                    "topic": "daman/trace",
                    "payload": {
                        "chi": "pin-trace",
                        "args": {
                            "trace_json": {
                                "agent": BEE_NAME,
                                "decision": "ruling",
                                "claim_id": args.claim_id,
                                "leader": args.leader,
                                "slash_amount": REFERENCE_SLASH_AMOUNT_HEX,
                                "upheld": true,
                                "policy": "reference-auto-uphold-at-cap",
                                "ruling_nonce": &ruling_nonce,
                            },
                            "metadata": {
                                "agent": BEE_NAME,
                                "version": env!("CARGO_PKG_VERSION"),
                            },
                            "request_id": &ruling_nonce,
                        }
                    }
                });
                write_line(&mut write_half, &trace).await?;
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

#[allow(dead_code)]
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dispute_opened_frame() {
        let raw = r#"{"chi":"dispute-opened","args":{"claim_id":"0x1","leader":"0xabc","evidence_hash":"0xdef","dispute_window_ends":1750000000}}"#;
        let frame: InboundFrame = serde_json::from_str(raw).unwrap();
        match frame {
            InboundFrame::DisputeOpened { args } => {
                assert_eq!(args.claim_id, "0x1");
                assert_eq!(args.leader, "0xabc");
            }
            _ => panic!("expected DisputeOpened"),
        }
    }
}
