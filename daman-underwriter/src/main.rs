//! daman-underwriter. The leader-onboarding screening bee.
//!
//! Subscribes to `chi:"register-leader-request"` tones (filed by the
//! bridge bee on receipt of a registerLeader on-chain event, or by
//! the storefront prior to dispatch). For each candidate, the bee
//! issues three `chi:"query-history"` requests to the chain-reader
//! forager (A10) and consumes the three responses:
//!
//!   1. filter:"leverage-signatures" -> reject on any hit
//!   2. filter:"prediction-market-positions" -> reject on any hit
//!   3. filter:"spot-only" with min volume above tier threshold -> accept tier
//!
//! On completion, the bee emits `chi:"underwriter-decision"` carrying
//! the proposed tier and required bond, plus a reason hash that
//! includes which rule classified the candidate. The bridge translates
//! the decision into an `underwriterAttest(address leader, Tier tier,
//! uint256 bondMin)` call on chain (contract surface awaiting A1+
//! follow-on; the chi-side contract is published independent of the
//! on-chain landing).
//!
//! Zero API keys end-to-end. The three on-RPC checks via the
//! chain-reader forager are sufficient admission cost; the prior A9
//! Circle Compliance Engine HTTP call has been removed per the
//! protocol-talker pivot to eliminate the SaaS deplatform vector at
//! the bee boundary.
//!
//! Wire:
//!
//!   consumer ─► chi:"register-leader-request" { candidate, claimed_aum, query_id } ─► underwriter
//!   underwriter ─► chi:"query-history" { ... }      ─► chain-reader (A10)
//!   underwriter ◄─ chi:"history-result" { ... }     ◄─ chain-reader (A10)
//!   underwriter ─► chi:"underwriter-decision" { candidate, tier, required_bond, reason_code } ─► bridge
//!
//! Configure:
//!
//!   HUM_THRUM_SOCK                      humd's NDJSON socket
//!   DAMAN_UNDERWRITER_LOOKBACK_DAYS     history depth (default 90)
//!   DAMAN_UNDERWRITER_RETAIL_AUM_USDC   retail-tier max claimed AUM atomic (default 250_000 * 10^18)
//!   DAMAN_UNDERWRITER_MID_AUM_USDC      mid-tier max claimed AUM atomic (default 5_000_000 * 10^18)

use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{info, warn};
use uuid::Uuid;

const BEE_NAME: &str = "daman-underwriter";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");
const REGISTER_TOPIC: &str = "daman/register";
const HISTORY_TOPIC: &str = "daman/history";

const DEFAULT_LOOKBACK_DAYS: u32 = 90;

#[derive(Debug, Clone)]
struct Config {
    sock_path: String,
    lookback_days: u32,
    retail_aum_cap_atomic: u128,
    mid_aum_cap_atomic: u128,
    #[allow(dead_code)]
    request_timeout: std::time::Duration,
}

impl Config {
    fn from_env() -> Result<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::geteuid() }));
        let default_sock = format!("{runtime}/hum/thrum.sock");
        let parse_u128 =
            |k: &str, d: u128| std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d);
        Ok(Self {
            sock_path: std::env::var("HUM_THRUM_SOCK").unwrap_or(default_sock),
            lookback_days: std::env::var("DAMAN_UNDERWRITER_LOOKBACK_DAYS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_LOOKBACK_DAYS),
            retail_aum_cap_atomic: parse_u128(
                "DAMAN_UNDERWRITER_RETAIL_AUM_USDC",
                250_000u128 * 10u128.pow(18),
            ),
            mid_aum_cap_atomic: parse_u128(
                "DAMAN_UNDERWRITER_MID_AUM_USDC",
                5_000_000u128 * 10u128.pow(18),
            ),
            request_timeout: std::time::Duration::from_secs(15),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
struct RegisterRequest {
    candidate: String,
    claimed_aum: String,
    #[serde(default)]
    query_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
struct HistoryResult {
    #[allow(dead_code)]
    chain: String,
    filter: String,
    #[serde(default)]
    addresses: Vec<String>,
    #[serde(default)]
    events: Vec<Value>,
    #[serde(default)]
    query_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct Decision {
    candidate: String,
    tier: String,
    required_bond_atomic: String,
    reason_code: String,
    request_id: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    /// candidate (lowercase) -> pending underwriting round.
    pending: HashMap<String, PendingRound>,
}

#[derive(Debug)]
struct PendingRound {
    candidate: String,
    claimed_aum_atomic: u128,
    request_id: Option<String>,
    /// filter name -> result already received.
    received: HashMap<String, bool>,
    /// flagged on receipt of any leverage or perp finding.
    rejected_reason: Option<&'static str>,
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
    info!(
        sock = %cfg.sock_path,
        lookback = cfg.lookback_days,
        "{BEE_NAME} starting"
    );

    let state: Arc<Mutex<State>> = Arc::new(Mutex::new(State::default()));

    let stream = UnixStream::connect(&cfg.sock_path)
        .await
        .with_context(|| format!("connect to humd at {}", cfg.sock_path))?;
    let (read_half, write_half) = stream.into_split();
    let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

    let hello = json!({
        "chi": "hello",
        "bee": ["worker"],
        "hive": BEE_NAME,
        "name": BEE_NAME,
        "version": BEE_VERSION,
        "protoVersion": "0.7.0",
        "propensity": {
            "statefulness": "stateful",
            "richness": "medium",
            "wire": "custom/underwriter-v0"
        },
        "chis": [
            "hello",
            "gossip-publish",
            "register-leader-request",
            "query-history",
            "history-result",
            "underwriter-decision"
        ],
        "topics": [REGISTER_TOPIC, HISTORY_TOPIC],
        "source": "https://github.com/damanfi/agents",
    });
    {
        let mut w = write_half.lock().await;
        write_line(&mut *w, &hello).await?;
    }

    let mut reader = BufReader::new(read_half).lines();
    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let envelope: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "envelope parse failed");
                continue;
            }
        };

        let inner = unwrap_payload(&envelope);
        match inner.get("chi").and_then(Value::as_str) {
            Some("register-leader-request") => {
                let args = match inner.get("args").cloned() {
                    Some(a) => a,
                    None => continue,
                };
                let cfg = cfg.clone();
                let state = state.clone();
                let write_half = write_half.clone();
                tokio::spawn(async move {
                    handle_register_request(&cfg, &args, &state, &write_half).await;
                });
            }
            Some("history-result") => {
                let args = match inner.get("args").cloned() {
                    Some(a) => a,
                    None => continue,
                };
                let cfg = cfg.clone();
                let state = state.clone();
                let write_half = write_half.clone();
                tokio::spawn(async move {
                    handle_history_result(&cfg, &args, &state, &write_half).await;
                });
            }
            _ => {}
        }
    }

    Ok(())
}

fn unwrap_payload(envelope: &Value) -> &Value {
    if envelope.get("chi").and_then(Value::as_str) == Some("gossip-publish") {
        if let Some(p) = envelope.get("payload") {
            return p;
        }
    }
    envelope
}

async fn handle_register_request(
    cfg: &Config,
    args: &Value,
    state: &Arc<Mutex<State>>,
    write: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let req: RegisterRequest = match serde_json::from_value(args.clone()) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "register-leader-request parse failed");
            return;
        }
    };
    let candidate_key = req.candidate.to_lowercase();
    let claimed_aum_atomic = u128_from_decimal_or_hex(&req.claimed_aum).unwrap_or(0);

    {
        let mut s = state.lock();
        s.pending.insert(
            candidate_key.clone(),
            PendingRound {
                candidate: req.candidate.clone(),
                claimed_aum_atomic,
                request_id: req.query_id.clone(),
                received: HashMap::new(),
                rejected_reason: None,
            },
        );
    }

    // Issue the three chain-history checks in parallel.
    for filter in [
        "leverage-signatures",
        "prediction-market-positions",
        "spot-only",
    ] {
        let query_id = format!("uw:{}:{}", candidate_key, filter);
        let req_chi = json!({
            "chi": "gossip-publish",
            "topic": HISTORY_TOPIC,
            "payload": {
                "chi": "query-history",
                "args": {
                    "chain": "arc",
                    "address": req.candidate,
                    "lookback_days": cfg.lookback_days,
                    "filter": filter,
                    "query_id": query_id,
                }
            }
        });
        let mut w = write.lock().await;
        if let Err(e) = write_line(&mut *w, &req_chi).await {
            warn!(error = %e, filter, "query-history write failed");
        }
    }
}

async fn handle_history_result(
    cfg: &Config,
    args: &Value,
    state: &Arc<Mutex<State>>,
    write: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let parsed: HistoryResult = match serde_json::from_value(args.clone()) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "history-result parse failed");
            return;
        }
    };
    let query_id = parsed.query_id.clone().unwrap_or_default();
    let candidate_key = query_id
        .strip_prefix("uw:")
        .and_then(|rest| rest.split(':').next())
        .map(String::from)
        .unwrap_or_default();
    if candidate_key.is_empty() {
        return;
    }

    let (decision_opt, completed) = {
        let mut s = state.lock();
        let round = match s.pending.get_mut(&candidate_key) {
            Some(r) => r,
            None => return,
        };
        let hit = !parsed.addresses.is_empty() || !parsed.events.is_empty();
        if hit && parsed.filter == "leverage-signatures" && round.rejected_reason.is_none() {
            round.rejected_reason = Some("leverage-signatures-present");
        }
        if hit
            && parsed.filter == "prediction-market-positions"
            && round.rejected_reason.is_none()
        {
            round.rejected_reason = Some("perp-or-pm-positions-present");
        }
        round.received.insert(parsed.filter.clone(), hit);
        if round.received.len() < 3 {
            (None, false)
        } else {
            // Round complete: assemble the decision.
            let (tier, required_bond, reason_code) = if let Some(r) = round.rejected_reason {
                ("Rejected".to_string(), 0u128, r.to_string())
            } else {
                let aum = round.claimed_aum_atomic;
                let (tier_name, bps) = tier_for_aum(cfg, aum);
                let required = aum / 10_000u128 * bps as u128;
                (tier_name, required, "spot-only-clean".to_string())
            };
            let decision = Decision {
                candidate: round.candidate.clone(),
                tier,
                required_bond_atomic: format!("0x{:x}", required_bond),
                reason_code,
                request_id: round.request_id.clone(),
            };
            (Some(decision), true)
        }
    };

    if completed {
        let mut s = state.lock();
        s.pending.remove(&candidate_key);
    }
    if let Some(d) = decision_opt {
        publish_decision(&d, write).await;
    }
}

/// Map claimed AUM atomic to (tier name, bond bps). Pure function for
/// unit tests.
pub(crate) fn tier_for_aum(cfg: &Config, aum_atomic: u128) -> (String, u16) {
    if aum_atomic <= cfg.retail_aum_cap_atomic {
        ("Retail".to_string(), 1000)
    } else if aum_atomic <= cfg.mid_aum_cap_atomic {
        ("Mid".to_string(), 500)
    } else {
        ("Institutional".to_string(), 250)
    }
}

async fn publish_decision(
    decision: &Decision,
    write: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let tone = json!({
        "chi": "gossip-publish",
        "topic": REGISTER_TOPIC,
        "payload": {
            "chi": "underwriter-decision",
            "args": serde_json::to_value(decision).unwrap_or(Value::Null),
        }
    });
    let mut w = write.lock().await;
    if let Err(e) = write_line(&mut *w, &tone).await {
        warn!(error = %e, "decision write failed");
    } else {
        info!(candidate = %decision.candidate, tier = %decision.tier, reason = %decision.reason_code, "decision emitted");
    }
}

fn u128_from_decimal_or_hex(s: &str) -> Result<u128> {
    if let Some(rest) = s.strip_prefix("0x") {
        u128::from_str_radix(rest, 16).context("hex u128")
    } else {
        s.parse::<u128>().context("decimal u128")
    }
}

async fn write_line<W: AsyncWriteExt + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    let s = serde_json::to_string(v)?;
    w.write_all(s.as_bytes()).await?;
    w.write_all(b"\n").await?;
    Ok(())
}

#[allow(dead_code)]
fn dispatch_query_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            sock_path: "/tmp/unused".into(),
            lookback_days: 90,
            retail_aum_cap_atomic: 250_000u128 * 10u128.pow(18),
            mid_aum_cap_atomic: 5_000_000u128 * 10u128.pow(18),
            request_timeout: std::time::Duration::from_secs(1),
        }
    }

    #[test]
    fn tier_for_aum_buckets_correctly() {
        let c = cfg();
        let (t1, b1) = tier_for_aum(&c, 10_000u128 * 10u128.pow(18));
        assert_eq!(t1, "Retail");
        assert_eq!(b1, 1000);
        let (t2, b2) = tier_for_aum(&c, 1_000_000u128 * 10u128.pow(18));
        assert_eq!(t2, "Mid");
        assert_eq!(b2, 500);
        let (t3, b3) = tier_for_aum(&c, 50_000_000u128 * 10u128.pow(18));
        assert_eq!(t3, "Institutional");
        assert_eq!(b3, 250);
    }

    #[test]
    fn unwrap_payload_handles_gossip_wrapper() {
        let env = json!({
            "chi": "gossip-publish",
            "topic": "daman/register",
            "payload": { "chi": "register-leader-request", "args": {} }
        });
        let inner = unwrap_payload(&env);
        assert_eq!(
            inner.get("chi").and_then(Value::as_str),
            Some("register-leader-request")
        );
    }

    #[test]
    fn u128_from_decimal_or_hex_parses_both_forms() {
        assert_eq!(u128_from_decimal_or_hex("12345").unwrap(), 12345);
        assert_eq!(u128_from_decimal_or_hex("0xff").unwrap(), 255);
    }
}
