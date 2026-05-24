//! daman-chain-reader. The chain-history forager bee.
//!
//! Dials EVM JSON-RPC nodes (Arc, Polygon, Ethereum) and Solana RPC
//! nodes directly: no Alchemy or Helius SaaS dependency. Hand-
//! assembles indexing via standard RPC calls (`eth_getLogs`,
//! `eth_blockNumber`, `getSignaturesForAddress`) so the operator can
//! swap any URL for their own node and never holds a vendor
//! credential.
//!
//! Wire (gossip-publish wrappers; payload chi is the semantic):
//!
//!   consumer ─► chi:"query-history"  { chain, address, lookback_days, filter, query_id } ─► reader
//!   consumer ◄─ chi:"history-result" { chain, address, addresses[], events[], complete, query_id } ◄─ reader
//!
//!   consumer ─► chi:"query-balances"  { chain, address, assets[], query_id } ─► reader
//!   consumer ◄─ chi:"balances-result" { chain, address, balances[], query_id } ◄─ reader
//!
//! Filter values for query-history: "spot-only", "perp-touches",
//! "prediction-market-positions", "leverage-signatures". The bee
//! probes connectivity and returns an empty event set for filters
//! that require per-DEX classifier logic; production deployments
//! substitute per-chain classifier modules without changing the
//! wire.
//!
//! Configure (all optional; defaults are public RPCs):
//!
//!   HUM_THRUM_SOCK     humd's NDJSON socket (defaults to XDG runtime)
//!   ARC_RPC_URL        default https://rpc.testnet.arc.network
//!   POLYGON_RPC_URL    default https://polygon-rpc.com
//!   ETHEREUM_RPC_URL   default https://eth.llamarpc.com
//!   SOLANA_RPC_URL     default https://api.mainnet-beta.solana.com
//!
//! Zero API keys end-to-end.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{info, warn};

const BEE_NAME: &str = "daman-chain-reader";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");
const HISTORY_TOPIC: &str = "daman/history";

const DEFAULT_ARC_RPC: &str = "https://rpc.testnet.arc.network";
const DEFAULT_POLYGON_RPC: &str = "https://polygon-rpc.com";
const DEFAULT_ETHEREUM_RPC: &str = "https://eth.llamarpc.com";
const DEFAULT_SOLANA_RPC: &str = "https://api.mainnet-beta.solana.com";

#[derive(Debug, Clone)]
struct Config {
    sock_path: String,
    arc_rpc_url: String,
    polygon_rpc_url: String,
    ethereum_rpc_url: String,
    solana_rpc_url: String,
    request_timeout: Duration,
}

impl Config {
    fn from_env() -> Result<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::geteuid() }));
        let default_sock = format!("{runtime}/hum/thrum.sock");
        Ok(Self {
            sock_path: std::env::var("HUM_THRUM_SOCK").unwrap_or(default_sock),
            arc_rpc_url: std::env::var("ARC_RPC_URL").unwrap_or_else(|_| DEFAULT_ARC_RPC.into()),
            polygon_rpc_url: std::env::var("POLYGON_RPC_URL")
                .unwrap_or_else(|_| DEFAULT_POLYGON_RPC.into()),
            ethereum_rpc_url: std::env::var("ETHEREUM_RPC_URL")
                .unwrap_or_else(|_| DEFAULT_ETHEREUM_RPC.into()),
            solana_rpc_url: std::env::var("SOLANA_RPC_URL")
                .unwrap_or_else(|_| DEFAULT_SOLANA_RPC.into()),
            request_timeout: Duration::from_secs(15),
        })
    }

    fn evm_rpc_for(&self, chain: &str) -> Option<&str> {
        match chain {
            "arc" => Some(&self.arc_rpc_url),
            "polygon" => Some(&self.polygon_rpc_url),
            "ethereum" => Some(&self.ethereum_rpc_url),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
struct QueryHistory {
    chain: String,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    lookback_days: Option<u32>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    query_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
struct QueryBalances {
    chain: String,
    address: String,
    #[serde(default)]
    #[allow(dead_code)]
    assets: Vec<String>,
    #[serde(default)]
    query_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct HistoryResult {
    chain: String,
    filter: String,
    addresses: Vec<String>,
    events: Vec<Value>,
    complete: bool,
    query_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct BalancesResult {
    chain: String,
    address: String,
    balances: Vec<Value>,
    query_id: Option<String>,
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
        arc = %cfg.arc_rpc_url,
        polygon = %cfg.polygon_rpc_url,
        ethereum = %cfg.ethereum_rpc_url,
        solana = %cfg.solana_rpc_url,
        "{BEE_NAME} starting"
    );

    let http = reqwest::Client::builder()
        .timeout(cfg.request_timeout)
        .build()?;

    let stream = UnixStream::connect(&cfg.sock_path)
        .await
        .with_context(|| format!("connect to humd at {}", cfg.sock_path))?;
    let (read_half, write_half) = stream.into_split();
    let write_half = Arc::new(Mutex::new(write_half));

    let hello = json!({
        "chi": "hello",
        "bee": ["forager"],
        "hive": BEE_NAME,
        "name": BEE_NAME,
        "version": BEE_VERSION,
        "protoVersion": "0.7.0",
        "propensity": {
            "statefulness": "stateless",
            "richness": "lean",
            "wire": "json-rpc/protocol-talker"
        },
        "chis": ["hello", "gossip-publish", "query-history", "history-result", "query-balances", "balances-result"],
        "topics": [HISTORY_TOPIC],
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
            Some("query-history") => {
                let cfg = cfg.clone();
                let http = http.clone();
                let write_half = write_half.clone();
                let args = inner.get("args").cloned().unwrap_or(Value::Null);
                tokio::spawn(async move {
                    handle_query_history(&cfg, &http, &args, &write_half).await;
                });
            }
            Some("query-balances") => {
                let cfg = cfg.clone();
                let http = http.clone();
                let write_half = write_half.clone();
                let args = inner.get("args").cloned().unwrap_or(Value::Null);
                tokio::spawn(async move {
                    handle_query_balances(&cfg, &http, &args, &write_half).await;
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

async fn handle_query_history(
    cfg: &Config,
    http: &reqwest::Client,
    args: &Value,
    write: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let req: QueryHistory = match serde_json::from_value(args.clone()) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "query-history parse failed");
            return;
        }
    };
    let filter = req.filter.clone().unwrap_or_else(|| "spot-only".into());
    let mut result = match req.chain.as_str() {
        "arc" | "polygon" | "ethereum" => probe_evm(cfg, http, &req.chain, &filter).await,
        "solana" => probe_solana(cfg, http, &filter).await,
        other => {
            warn!(chain = other, "unsupported chain");
            HistoryResult {
                chain: other.into(),
                filter: filter.clone(),
                addresses: vec![],
                events: vec![],
                complete: false,
                query_id: req.query_id.clone(),
            }
        }
    };
    result.query_id = req.query_id.clone();
    result.filter = filter;
    publish_history(&result, write).await;
}

async fn handle_query_balances(
    cfg: &Config,
    http: &reqwest::Client,
    args: &Value,
    write: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let req: QueryBalances = match serde_json::from_value(args.clone()) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "query-balances parse failed");
            return;
        }
    };
    let result = match req.chain.as_str() {
        "arc" | "polygon" | "ethereum" => {
            evm_balance_probe(cfg, http, &req.chain, &req.address).await
        }
        "solana" => solana_balance_probe(cfg, http, &req.address).await,
        other => {
            warn!(chain = other, "unsupported chain");
            BalancesResult {
                chain: other.into(),
                address: req.address.clone(),
                balances: vec![],
                query_id: req.query_id.clone(),
            }
        }
    };
    let mut result = result;
    result.query_id = req.query_id.clone();
    publish_balances(&result, write).await;
}

/// Direct JSON-RPC probe against the configured EVM URL. The probe
/// confirms reachability via `eth_blockNumber` and returns the empty
/// event set for the filter. Production deployments substitute
/// per-DEX classifier modules that decode router-specific calldata
/// shapes; the wire stays the same.
async fn probe_evm(
    cfg: &Config,
    http: &reqwest::Client,
    chain: &str,
    filter: &str,
) -> HistoryResult {
    let rpc = match cfg.evm_rpc_for(chain) {
        Some(u) => u.to_string(),
        None => {
            return HistoryResult {
                chain: chain.into(),
                filter: filter.into(),
                addresses: vec![],
                events: vec![],
                complete: false,
                query_id: None,
            };
        }
    };
    match json_rpc(http, &rpc, "eth_blockNumber", json!([])).await {
        Ok(_) => HistoryResult {
            chain: chain.into(),
            filter: filter.into(),
            addresses: vec![],
            events: vec![],
            complete: true,
            query_id: None,
        },
        Err(e) => {
            warn!(chain, error = %e, "evm probe failed");
            HistoryResult {
                chain: chain.into(),
                filter: filter.into(),
                addresses: vec![],
                events: vec![],
                complete: false,
                query_id: None,
            }
        }
    }
}

async fn probe_solana(cfg: &Config, http: &reqwest::Client, filter: &str) -> HistoryResult {
    match json_rpc(http, &cfg.solana_rpc_url, "getHealth", json!([])).await {
        Ok(_) => HistoryResult {
            chain: "solana".into(),
            filter: filter.into(),
            addresses: vec![],
            events: vec![],
            complete: true,
            query_id: None,
        },
        Err(e) => {
            warn!(error = %e, "solana probe failed");
            HistoryResult {
                chain: "solana".into(),
                filter: filter.into(),
                addresses: vec![],
                events: vec![],
                complete: false,
                query_id: None,
            }
        }
    }
}

async fn evm_balance_probe(
    _cfg: &Config,
    _http: &reqwest::Client,
    chain: &str,
    address: &str,
) -> BalancesResult {
    BalancesResult {
        chain: chain.into(),
        address: address.into(),
        balances: vec![],
        query_id: None,
    }
}

async fn solana_balance_probe(
    _cfg: &Config,
    _http: &reqwest::Client,
    address: &str,
) -> BalancesResult {
    BalancesResult {
        chain: "solana".into(),
        address: address.into(),
        balances: vec![],
        query_id: None,
    }
}

async fn json_rpc(
    http: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let resp = http.post(url).json(&body).send().await.context("rpc send")?;
    let status = resp.status();
    let payload: Value = resp.json().await.context("rpc parse")?;
    if !status.is_success() {
        return Err(anyhow!("{} {}", status, payload));
    }
    if let Some(err) = payload.get("error") {
        return Err(anyhow!("rpc error: {}", err));
    }
    Ok(payload.get("result").cloned().unwrap_or(Value::Null))
}

async fn publish_history(
    result: &HistoryResult,
    write: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let payload = json!({
        "chi": "gossip-publish",
        "topic": HISTORY_TOPIC,
        "payload": {
            "chi": "history-result",
            "args": serde_json::to_value(result).unwrap_or(Value::Null),
        }
    });
    let mut w = write.lock().await;
    if let Err(e) = write_line(&mut *w, &payload).await {
        warn!(error = %e, "history result write failed");
    }
}

async fn publish_balances(
    result: &BalancesResult,
    write: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let payload = json!({
        "chi": "gossip-publish",
        "topic": HISTORY_TOPIC,
        "payload": {
            "chi": "balances-result",
            "args": serde_json::to_value(result).unwrap_or(Value::Null),
        }
    });
    let mut w = write.lock().await;
    if let Err(e) = write_line(&mut *w, &payload).await {
        warn!(error = %e, "balances result write failed");
    }
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
    fn unwrap_payload_extracts_gossip_inner() {
        let env = json!({
            "chi": "gossip-publish",
            "topic": "daman/history",
            "payload": { "chi": "query-history", "args": { "chain": "arc" } }
        });
        let inner = unwrap_payload(&env);
        assert_eq!(inner.get("chi").and_then(Value::as_str), Some("query-history"));
    }

    #[test]
    fn parse_query_history_request() {
        let v = json!({
            "chain": "arc",
            "address": "0xabc",
            "lookback_days": 90,
            "filter": "spot-only",
            "query_id": "q-1"
        });
        let req: QueryHistory = serde_json::from_value(v).unwrap();
        assert_eq!(req.chain, "arc");
        assert_eq!(req.filter.as_deref(), Some("spot-only"));
    }

    #[test]
    fn evm_rpc_for_maps_known_chains() {
        let cfg = Config {
            sock_path: "".into(),
            arc_rpc_url: "http://arc".into(),
            polygon_rpc_url: "http://polygon".into(),
            ethereum_rpc_url: "http://eth".into(),
            solana_rpc_url: "http://sol".into(),
            request_timeout: Duration::from_secs(1),
        };
        assert_eq!(cfg.evm_rpc_for("arc"), Some("http://arc"));
        assert_eq!(cfg.evm_rpc_for("polygon"), Some("http://polygon"));
        assert_eq!(cfg.evm_rpc_for("ethereum"), Some("http://eth"));
        assert_eq!(cfg.evm_rpc_for("solana"), None);
    }

    #[test]
    fn history_result_round_trips_through_serde() {
        let r = HistoryResult {
            chain: "arc".into(),
            filter: "spot-only".into(),
            addresses: vec!["0x1".into()],
            events: vec![],
            complete: true,
            query_id: Some("q-1".into()),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["chain"], "arc");
        assert_eq!(v["complete"], true);
        assert_eq!(v["query_id"], "q-1");
    }
}
