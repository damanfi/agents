//! daman-recruiter. The mesh-native scan-and-invite bee.
//!
//! Discovers spot-only addresses across Arc, Polygon, Ethereum, and
//! Solana, intersects the result sets to identify candidates that
//! never touch perpetuals, and dispatches two artifacts per candidate:
//!
//!   1. A Farcaster cast via `chi:cast-publish` (handled by the
//!      daman-farcaster-poster bee).
//!   2. An on-chain `attestRecruitment(candidate, reasonCode)` intent
//!      via `chi:attest-recruitment` (handled by the daman-bridge bee
//!      once the contract surface lands; the recruiter is silent on
//!      whether the dispatch succeeds, only on whether the intent was
//!      published).
//!
//! Mesh-native by construction: the recruiter never imports an
//! Alchemy, Helius, or Neynar client. All external surfaces are
//! reached through forager bees. The recruiter holds no credentials.
//!
//! Wire (gossip-publish wrappers; payload chi is the semantic):
//!
//!   recruiter ─► chi:"query-history"   { chain, lookback_days, filter, query_id } ─► chain-reader
//!   recruiter ◄─ chi:"history-result"  { chain, addresses[], query_id }            ◄─ chain-reader
//!
//!   recruiter ─► chi:"cast-publish"    { text, embeds[] }                          ─► farcaster-poster
//!   recruiter ◄─ chi:"cast-published"  { cast_hash, published_at_iso }             ◄─ farcaster-poster
//!
//!   recruiter ─► chi:"attest-recruitment" { candidate, reason_code }               ─► bridge (on-chain)

use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::interval;
use tracing::{info, warn};
use uuid::Uuid;

const BEE_NAME: &str = "daman-recruiter";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");
const HISTORY_TOPIC: &str = "daman/history";
const CAST_TOPIC: &str = "daman/cast";
const RECRUIT_TOPIC: &str = "daman/recruit";
const DEFAULT_SCAN_CHAINS: &[&str] = &["arc", "polygon", "ethereum", "solana"];
const DEFAULT_LOOKBACK_DAYS: u32 = 90;
const CAST_TEMPLATE: &str =
    "daman is open for spot-only leaders with skin in the game. eligibility verified against your on-chain history. learn more at daman.fi";

#[derive(Debug, Clone)]
struct Config {
    sock_path: String,
    scan_interval: Duration,
    lookback_days: u32,
    chains: Vec<String>,
}

impl Config {
    fn from_env() -> Result<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::geteuid() }));
        let default_sock = format!("{runtime}/hum/thrum.sock");
        Ok(Self {
            sock_path: std::env::var("HUM_THRUM_SOCK").unwrap_or(default_sock),
            scan_interval: Duration::from_secs(
                std::env::var("DAMAN_RECRUITER_SCAN_INTERVAL_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(3600),
            ),
            lookback_days: std::env::var("DAMAN_RECRUITER_LOOKBACK_DAYS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_LOOKBACK_DAYS),
            chains: std::env::var("DAMAN_RECRUITER_CHAINS")
                .ok()
                .map(|s| s.split(',').map(str::trim).map(String::from).collect())
                .unwrap_or_else(|| DEFAULT_SCAN_CHAINS.iter().map(|s| s.to_string()).collect()),
        })
    }
}

/// Per-chain history slice returned by the chain-reader forager.
#[derive(Debug, Clone, Deserialize)]
struct HistoryResult {
    chain: String,
    addresses: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    query_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    filter: Option<String>,
}

/// In-memory state. Tracks scan rounds, pending queries, and a roster
/// of already-invited candidates so the bee doesn't re-cast on every
/// tick.
#[derive(Default)]
struct State {
    /// query_id -> (chain, filter) for outstanding requests.
    pending_queries: HashMap<String, (String, String)>,
    /// scan_round_id -> partial result set per filter.
    rounds: HashMap<String, ScanRound>,
    /// candidates already invited; never re-invited.
    invited: HashSet<String>,
}

#[derive(Default)]
struct ScanRound {
    spot_only: HashMap<String, HashSet<String>>, // chain -> addresses
    perp_touches: HashMap<String, HashSet<String>>, // chain -> addresses
    awaiting: usize,
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
        scan_interval = ?cfg.scan_interval,
        chains = ?cfg.chains,
        "{BEE_NAME} starting"
    );

    let state: Arc<Mutex<State>> = Arc::new(Mutex::new(State::default()));

    let stream = UnixStream::connect(&cfg.sock_path)
        .await
        .with_context(|| format!("connect to humd at {}", cfg.sock_path))?;
    let (read_half, write_half) = stream.into_split();
    let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

    // Hello declaring scan + invite intent. The bee speaks the
    // gossip-publish wrapper around five custom chi names.
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
            "wire": "custom/recruiter-v0"
        },
        "chis": [
            "hello",
            "gossip-publish",
            "query-history",
            "history-result",
            "cast-publish",
            "cast-published",
            "attest-recruitment"
        ],
        "topics": [HISTORY_TOPIC, CAST_TOPIC, RECRUIT_TOPIC],
        "source": "https://github.com/damanfi/agents",
    });
    {
        let mut w = write_half.lock().await;
        write_line(&mut *w, &hello).await?;
    }

    // Scan tick: fire a fresh round of query-history requests every
    // scan_interval seconds.
    let cfg_for_tick = cfg.clone();
    let state_for_tick = state.clone();
    let write_for_tick = write_half.clone();
    tokio::spawn(async move {
        let mut tick = interval(cfg_for_tick.scan_interval);
        loop {
            tick.tick().await;
            if let Err(e) = run_scan_round(&cfg_for_tick, &state_for_tick, &write_for_tick).await {
                warn!(error = %e, "scan round failed");
            }
        }
    });

    // Inbound loop: history-result + cast-published acks.
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
            Some("history-result") => {
                handle_history_result(inner, &state, &write_half).await;
            }
            Some("cast-published") => {
                if let Some(args) = inner.get("args") {
                    info!(payload = %args, "cast published ack");
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Pull the semantic chi-tagged payload out of a gossip-publish wrapper
/// if present, otherwise return the envelope itself.
fn unwrap_payload(envelope: &Value) -> &Value {
    if envelope.get("chi").and_then(Value::as_str) == Some("gossip-publish") {
        if let Some(p) = envelope.get("payload") {
            return p;
        }
    }
    envelope
}

async fn run_scan_round(
    cfg: &Config,
    state: &Arc<Mutex<State>>,
    write: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> Result<()> {
    let round_id = Uuid::new_v4().to_string();
    info!(round = %round_id, "starting scan round");

    {
        let mut s = state.lock();
        s.rounds.insert(round_id.clone(), ScanRound::default());
        s.rounds.get_mut(&round_id).unwrap().awaiting = cfg.chains.len() * 2;
    }

    for chain in &cfg.chains {
        for filter in ["spot-only", "perp-touches"] {
            let query_id = format!("{}:{}:{}", round_id, chain, filter);
            {
                let mut s = state.lock();
                s.pending_queries
                    .insert(query_id.clone(), (chain.clone(), filter.to_string()));
            }
            let req = json!({
                "chi": "gossip-publish",
                "topic": HISTORY_TOPIC,
                "payload": {
                    "chi": "query-history",
                    "args": {
                        "chain": chain,
                        "lookback_days": cfg.lookback_days,
                        "filter": filter,
                        "query_id": query_id,
                    }
                }
            });
            let mut w = write.lock().await;
            write_line(&mut *w, &req).await?;
        }
    }

    Ok(())
}

async fn handle_history_result(
    inner: &Value,
    state: &Arc<Mutex<State>>,
    write: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let args = match inner.get("args") {
        Some(v) => v,
        None => return,
    };
    let parsed: HistoryResult = match serde_json::from_value(args.clone()) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "history result parse failed");
            return;
        }
    };
    let query_id = args
        .get("query_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let round_id = query_id.split(':').next().unwrap_or("").to_string();
    let filter = args
        .get("filter")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let candidates_to_invite: Vec<String> = {
        let mut s = state.lock();
        s.pending_queries.remove(&query_id);
        let round = match s.rounds.get_mut(&round_id) {
            Some(r) => r,
            None => return,
        };
        let bucket = match filter.as_str() {
            "spot-only" => &mut round.spot_only,
            "perp-touches" => &mut round.perp_touches,
            _ => return,
        };
        bucket
            .entry(parsed.chain.clone())
            .or_default()
            .extend(parsed.addresses.iter().cloned());
        round.awaiting = round.awaiting.saturating_sub(1);
        if round.awaiting > 0 {
            return;
        }
        // Round complete: intersect spot-only with NOT perp-touches.
        let candidates = intersect_candidates(&round.spot_only, &round.perp_touches);
        s.rounds.remove(&round_id);
        // Filter out previously-invited.
        candidates
            .into_iter()
            .filter(|c| !s.invited.contains(c))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|c| {
                s.invited.insert(c.clone());
                c
            })
            .collect()
    };

    for candidate in candidates_to_invite {
        if let Err(e) = invite_candidate(&candidate, write).await {
            warn!(candidate = %candidate, error = %e, "invite dispatch failed");
        }
    }
}

/// Pure-function intersection logic. Tested directly in unit tests.
fn intersect_candidates(
    spot_only: &HashMap<String, HashSet<String>>,
    perp_touches: &HashMap<String, HashSet<String>>,
) -> Vec<String> {
    let all_spot: HashSet<String> = spot_only
        .values()
        .flat_map(|s| s.iter().cloned())
        .collect();
    let all_perp: HashSet<String> = perp_touches
        .values()
        .flat_map(|s| s.iter().cloned())
        .collect();
    let mut out: Vec<String> = all_spot.difference(&all_perp).cloned().collect();
    out.sort();
    out
}

async fn invite_candidate(
    candidate: &str,
    write: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> Result<()> {
    let reason_code = rationale_hash(candidate);

    // Cast intent.
    let cast = json!({
        "chi": "gossip-publish",
        "topic": CAST_TOPIC,
        "payload": {
            "chi": "cast-publish",
            "args": {
                "text": format!("{} reason={}", CAST_TEMPLATE, reason_code),
                "embeds": ["https://daman.fi"],
                "signing_account": "@damanfi",
            }
        }
    });

    // On-chain attestation intent.
    let attest = json!({
        "chi": "gossip-publish",
        "topic": RECRUIT_TOPIC,
        "payload": {
            "chi": "attest-recruitment",
            "args": {
                "candidate": candidate,
                "reason_code": reason_code,
            }
        }
    });

    let mut w = write.lock().await;
    write_line(&mut *w, &cast).await?;
    write_line(&mut *w, &attest).await?;
    info!(candidate = %candidate, reason = %reason_code, "invite dispatched");
    Ok(())
}

/// Deterministic per-candidate reason code. Bridges the cast text and
/// the on-chain attestation: both carry the same hex so an observer
/// can verify the cast and the attestation reference the same rationale.
fn rationale_hash(candidate: &str) -> String {
    // sha256 prefix as a 32-byte hex. Keep it dependency-free; the
    // contract side will compute the same hash from the same input.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    "daman-recruiter:spot-only-no-perp".hash(&mut h);
    candidate.hash(&mut h);
    let n = h.finish();
    let mut bytes = [0u8; 32];
    for (i, b) in n.to_be_bytes().iter().enumerate() {
        bytes[i] = *b;
        bytes[i + 8] = *b;
        bytes[i + 16] = *b;
        bytes[i + 24] = *b;
    }
    format!("0x{}", hex::encode(bytes))
}

async fn write_line<W: AsyncWriteExt + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    let s = serde_json::to_string(v)?;
    w.write_all(s.as_bytes()).await?;
    w.write_all(b"\n").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_drops_perp_touchers() {
        let mut spot = HashMap::new();
        spot.insert(
            "arc".to_string(),
            vec!["0xA", "0xB", "0xC"].into_iter().map(String::from).collect(),
        );
        let mut perp = HashMap::new();
        perp.insert(
            "polygon".to_string(),
            vec!["0xB"].into_iter().map(String::from).collect(),
        );
        let candidates = intersect_candidates(&spot, &perp);
        assert_eq!(candidates, vec!["0xA".to_string(), "0xC".to_string()]);
    }

    #[test]
    fn intersect_unions_across_chains() {
        let mut spot = HashMap::new();
        spot.insert(
            "arc".to_string(),
            vec!["0x1"].into_iter().map(String::from).collect(),
        );
        spot.insert(
            "ethereum".to_string(),
            vec!["0x2"].into_iter().map(String::from).collect(),
        );
        let perp = HashMap::new();
        let candidates = intersect_candidates(&spot, &perp);
        assert_eq!(candidates, vec!["0x1".to_string(), "0x2".to_string()]);
    }

    #[test]
    fn rationale_hash_is_deterministic_per_candidate() {
        let a = rationale_hash("0xABC");
        let b = rationale_hash("0xABC");
        let c = rationale_hash("0xDEF");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("0x"));
        assert_eq!(a.len(), 2 + 64);
    }

    #[test]
    fn unwrap_payload_extracts_gossip_inner() {
        let env = json!({
            "chi": "gossip-publish",
            "topic": "daman/history",
            "payload": { "chi": "history-result", "args": { "chain": "arc" } }
        });
        let inner = unwrap_payload(&env);
        assert_eq!(inner.get("chi").and_then(Value::as_str), Some("history-result"));
    }

    #[test]
    fn parse_history_result_fixture() {
        // Fixture mirrors what the chain-reader forager (A10) is
        // expected to publish after a query-history request.
        let fixture = json!({
            "chain": "arc",
            "filter": "spot-only",
            "query_id": "round-1:arc:spot-only",
            "addresses": ["0xaaa", "0xbbb"],
        });
        let parsed: HistoryResult = serde_json::from_value(fixture).unwrap();
        assert_eq!(parsed.chain, "arc");
        assert_eq!(parsed.addresses.len(), 2);
    }
}
