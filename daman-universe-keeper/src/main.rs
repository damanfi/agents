//! daman-universe-keeper. The rebalance-driven asset-screening bee.
//!
//! Polls a published universe-screening source JSON on a configurable
//! cadence (default 6 hours), maintains a last-seen snapshot in
//! memory, computes the diff on each tick, and emits
//! `chi:universe-rebalance` carrying the added and removed asset
//! lists. The bridge bee translates the rebalance chi into
//! `addAsset(address, bytes32 source)` and `removeAsset(address,
//! bytes32 reason)` calls on the UniverseRegistry contract.
//!
//! Net: the whitelist is not admin-curated. The keeper polls the
//! source, the contract reflects the source. The operator can swap
//! sources by reconfiguring the keeper's env; the contract is
//! curation-agnostic.
//!
//! Wire (gossip-publish wrapper; payload chi is the semantic):
//!
//!   keeper ─► chi:"universe-rebalance" { source_tag, added[], removed[], updated_at_iso } ─► bridge
//!
//! Configure:
//!
//!   HUM_THRUM_SOCK                          humd's NDJSON socket
//!   DAMAN_UNIVERSE_HOLDINGS_URL             https URL serving the universe holdings JSON
//!   DAMAN_UNIVERSE_POLL_INTERVAL_SECS       seconds between polls (default 21600 = 6h)
//!   DAMAN_UNIVERSE_SOURCE_TAG               tag emitted with each rebalance (default HLAL_2026Q2)

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::time::interval;
use tracing::{info, warn};

const BEE_NAME: &str = "daman-universe-keeper";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");
const UNIVERSE_TOPIC: &str = "daman/universe";
const DEFAULT_POLL_INTERVAL_SECS: u64 = 6 * 60 * 60;
const DEFAULT_SOURCE_TAG: &str = "HLAL_2026Q2";

#[derive(Debug, Clone)]
struct Config {
    sock_path: String,
    holdings_url: String,
    poll_interval: Duration,
    source_tag: String,
    request_timeout: Duration,
}

impl Config {
    fn from_env() -> Result<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::geteuid() }));
        let default_sock = format!("{runtime}/hum/thrum.sock");
        Ok(Self {
            sock_path: std::env::var("HUM_THRUM_SOCK").unwrap_or(default_sock),
            holdings_url: std::env::var("DAMAN_UNIVERSE_HOLDINGS_URL")
                .context("DAMAN_UNIVERSE_HOLDINGS_URL is required")?,
            poll_interval: Duration::from_secs(
                std::env::var("DAMAN_UNIVERSE_POLL_INTERVAL_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_POLL_INTERVAL_SECS),
            ),
            source_tag: std::env::var("DAMAN_UNIVERSE_SOURCE_TAG")
                .unwrap_or_else(|_| DEFAULT_SOURCE_TAG.to_string()),
            request_timeout: Duration::from_secs(15),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Holdings {
    #[serde(default)]
    source_tag: Option<String>,
    /// List of token contract addresses (or placeholder pseudo-addresses
    /// during development). Other fields in the JSON are ignored.
    #[serde(default)]
    assets: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct RebalancePayload {
    source_tag: String,
    added: Vec<String>,
    removed: Vec<String>,
    updated_at_iso: String,
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
        url = %cfg.holdings_url,
        poll = ?cfg.poll_interval,
        source = %cfg.source_tag,
        "{BEE_NAME} starting"
    );

    let http = reqwest::Client::builder()
        .timeout(cfg.request_timeout)
        .build()?;

    let stream = UnixStream::connect(&cfg.sock_path)
        .await
        .with_context(|| format!("connect to humd at {}", cfg.sock_path))?;
    let (_read_half, write_half) = stream.into_split();
    let write_half = Arc::new(Mutex::new(write_half));

    let hello = json!({
        "chi": "hello",
        "bee": ["worker"],
        "hive": BEE_NAME,
        "name": BEE_NAME,
        "version": BEE_VERSION,
        "protoVersion": "0.7.0",
        "propensity": {
            "statefulness": "stateful",
            "richness": "lean",
            "wire": "http/json-poll"
        },
        "chis": ["hello", "gossip-publish", "universe-rebalance"],
        "topics": [UNIVERSE_TOPIC],
        "source": "https://github.com/damanfi/agents",
    });
    {
        let mut w = write_half.lock().await;
        write_line(&mut *w, &hello).await?;
    }

    let mut last_snapshot: HashSet<String> = HashSet::new();
    let mut tick = interval(cfg.poll_interval);
    // Fire once immediately, then on the cadence.
    loop {
        tick.tick().await;
        match fetch_holdings(&http, &cfg.holdings_url).await {
            Ok(holdings) => {
                let next: HashSet<String> = holdings.assets.into_iter().collect();
                let added: Vec<String> = next.difference(&last_snapshot).cloned().collect();
                let removed: Vec<String> =
                    last_snapshot.difference(&next).cloned().collect();
                if added.is_empty() && removed.is_empty() && !last_snapshot.is_empty() {
                    info!("universe unchanged");
                } else {
                    let payload = RebalancePayload {
                        source_tag: holdings
                            .source_tag
                            .unwrap_or_else(|| cfg.source_tag.clone()),
                        added: sorted(added),
                        removed: sorted(removed),
                        updated_at_iso: now_rfc3339(),
                    };
                    info!(
                        added = payload.added.len(),
                        removed = payload.removed.len(),
                        source = %payload.source_tag,
                        "universe rebalance"
                    );
                    publish_rebalance(&payload, &write_half).await;
                    last_snapshot = next;
                }
            }
            Err(e) => warn!(error = %e, "holdings fetch failed"),
        }
    }
}

async fn fetch_holdings(http: &reqwest::Client, url: &str) -> Result<Holdings> {
    let resp = http.get(url).send().await.context("holdings fetch")?;
    let status = resp.status();
    let body = resp.text().await.context("holdings body")?;
    if !status.is_success() {
        return Err(anyhow!("holdings {} {}", status, body));
    }
    parse_holdings(&body)
}

/// Parse a holdings response. Accepts either a top-level `{ source_tag,
/// assets[] }` object or a plain `[address, ...]` array. Factored out
/// for fixture-based testing.
fn parse_holdings(body: &str) -> Result<Holdings> {
    let v: Value = serde_json::from_str(body).context("holdings parse")?;
    if v.is_array() {
        let assets: Vec<String> = v
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        return Ok(Holdings {
            source_tag: None,
            assets,
        });
    }
    serde_json::from_value(v).context("holdings object decode")
}

async fn publish_rebalance(
    payload: &RebalancePayload,
    write: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let tone = json!({
        "chi": "gossip-publish",
        "topic": UNIVERSE_TOPIC,
        "payload": {
            "chi": "universe-rebalance",
            "args": serde_json::to_value(payload).unwrap_or(Value::Null),
        }
    });
    let mut w = write.lock().await;
    if let Err(e) = write_line(&mut *w, &tone).await {
        warn!(error = %e, "rebalance write failed");
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}", now)
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
    fn parse_holdings_accepts_array_shape() {
        let body = r#"["0xAAA", "0xBBB", "0xCCC"]"#;
        let h = parse_holdings(body).unwrap();
        assert_eq!(h.assets.len(), 3);
        assert!(h.source_tag.is_none());
    }

    #[test]
    fn parse_holdings_accepts_object_shape() {
        let body = r#"{ "source_tag": "HLAL_2026Q3", "assets": ["0xAAA"] }"#;
        let h = parse_holdings(body).unwrap();
        assert_eq!(h.assets.len(), 1);
        assert_eq!(h.source_tag.as_deref(), Some("HLAL_2026Q3"));
    }

    #[test]
    fn diff_logic_adds_and_removes() {
        let prev: HashSet<String> =
            ["0xA", "0xB", "0xC"].iter().map(|s| s.to_string()).collect();
        let next: HashSet<String> =
            ["0xB", "0xC", "0xD"].iter().map(|s| s.to_string()).collect();
        let added: Vec<String> = next.difference(&prev).cloned().collect();
        let removed: Vec<String> = prev.difference(&next).cloned().collect();
        assert_eq!(added, vec!["0xD".to_string()]);
        assert_eq!(removed, vec!["0xA".to_string()]);
    }

    #[test]
    fn rebalance_payload_serializes_with_snake_case() {
        let payload = RebalancePayload {
            source_tag: "HLAL_2026Q2".into(),
            added: vec!["0xA".into()],
            removed: vec![],
            updated_at_iso: "1716579600".into(),
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["source_tag"], "HLAL_2026Q2");
        assert_eq!(v["updated_at_iso"], "1716579600");
    }
}
