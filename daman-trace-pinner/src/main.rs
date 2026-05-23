//! daman-trace-pinner. The IPFS storage forager bee.
//!
//! Wraps the web3.storage HTTP upload API to expose reasoning-trace
//! pinning behind a single chi-pair. Consumer agents (watchdog,
//! arbiter) publish `chi:pin-trace` carrying the structured trace
//! payload and a metadata block; the pinner uploads, captures the
//! returned CID, and emits `chi:trace-pinned` with the CID + pin
//! method.
//!
//! The CID is then written into the on-chain ArbiterRuled.traceCid
//! field by the consumer agent (via the bridge) so every ruling
//! has a verifiable structured-output trail. Mirrors the humfs
//! storage-as-bee template from hum.
//!
//! Wire (gossip-publish wrappers; payload chi is the semantic):
//!
//!   consumer ─► chi:"pin-trace"   { trace_json, metadata } ─► pinner
//!   consumer ◄─ chi:"trace-pinned" { cid, pin_method }      ◄─ pinner
//!
//! Credentials:
//!
//!   WEB3_STORAGE_TOKEN   bearer token from web3.storage console
//!   HUM_THRUM_SOCK       humd's NDJSON socket (defaults to XDG runtime)
//!   WEB3_STORAGE_BASE    override for tests (defaults to https://api.web3.storage)

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{info, warn};

const BEE_NAME: &str = "daman-trace-pinner";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");
const TRACE_TOPIC: &str = "daman/trace";
const WEB3_STORAGE_DEFAULT_BASE: &str = "https://api.web3.storage";
const UPLOAD_PATH: &str = "/upload";

#[derive(Debug, Clone)]
struct Config {
    sock_path: String,
    web3_storage_token: String,
    web3_storage_base: String,
    request_timeout: Duration,
}

impl Config {
    fn from_env() -> Result<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::geteuid() }));
        let default_sock = format!("{runtime}/hum/thrum.sock");
        Ok(Self {
            sock_path: std::env::var("HUM_THRUM_SOCK").unwrap_or(default_sock),
            web3_storage_token: std::env::var("WEB3_STORAGE_TOKEN")
                .context("WEB3_STORAGE_TOKEN is required")?,
            web3_storage_base: std::env::var("WEB3_STORAGE_BASE")
                .unwrap_or_else(|_| WEB3_STORAGE_DEFAULT_BASE.to_string()),
            request_timeout: Duration::from_secs(30),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
struct PinRequest {
    trace_json: Value,
    #[serde(default)]
    metadata: Value,
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct PinResult {
    cid: String,
    pin_method: String,
    request_id: Option<String>,
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
        base = %cfg.web3_storage_base,
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
            "wire": "web3.storage/upload"
        },
        "chis": ["hello", "gossip-publish", "pin-trace", "trace-pinned"],
        "topics": [TRACE_TOPIC],
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
        if inner.get("chi").and_then(Value::as_str) != Some("pin-trace") {
            continue;
        }
        let args = match inner.get("args").cloned() {
            Some(a) => a,
            None => continue,
        };

        let cfg_clone = cfg.clone();
        let http_clone = http.clone();
        let write_half = write_half.clone();
        tokio::spawn(async move {
            handle_pin_request(&cfg_clone, &http_clone, &args, &write_half).await;
        });
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

async fn handle_pin_request(
    cfg: &Config,
    http: &reqwest::Client,
    args: &Value,
    write: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let req: PinRequest = match serde_json::from_value(args.clone()) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "pin-trace parse failed");
            return;
        }
    };
    match upload(cfg, http, &req).await {
        Ok(cid) => {
            let result = PinResult {
                cid: cid.clone(),
                pin_method: "ipfs".into(),
                request_id: req.request_id.clone(),
            };
            publish_result(&result, write).await;
            info!(cid = %cid, "trace pinned");
        }
        Err(e) => {
            warn!(error = %e, "pin failed");
            publish_error(req.request_id.clone(), &e.to_string(), write).await;
        }
    }
}

async fn upload(
    cfg: &Config,
    http: &reqwest::Client,
    req: &PinRequest,
) -> Result<String> {
    let url = format!("{}{}", cfg.web3_storage_base, UPLOAD_PATH);
    let body = serde_json::to_vec(&json!({
        "trace": req.trace_json,
        "metadata": req.metadata,
    }))?;
    let resp = http
        .post(&url)
        .bearer_auth(&cfg.web3_storage_token)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .context("web3.storage request")?;
    let status = resp.status();
    let payload: Value = resp.json().await.context("web3.storage parse")?;
    if !status.is_success() {
        return Err(anyhow!("web3.storage {} {}", status, payload));
    }
    parse_upload_response(&payload)
}

/// Extract the CID from a web3.storage upload response. Factored for
/// fixture-based testing without an HTTP server.
fn parse_upload_response(payload: &Value) -> Result<String> {
    payload
        .get("cid")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("missing cid in response: {}", payload))
}

async fn publish_result(
    result: &PinResult,
    write: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let payload = json!({
        "chi": "gossip-publish",
        "topic": TRACE_TOPIC,
        "payload": {
            "chi": "trace-pinned",
            "args": serde_json::to_value(result).unwrap_or(Value::Null),
        }
    });
    let mut w = write.lock().await;
    if let Err(e) = write_line(&mut *w, &payload).await {
        warn!(error = %e, "result write failed");
    }
}

async fn publish_error(
    request_id: Option<String>,
    error: &str,
    write: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    let payload = json!({
        "chi": "gossip-publish",
        "topic": TRACE_TOPIC,
        "payload": {
            "chi": "trace-pin-error",
            "args": { "error": error, "request_id": request_id }
        }
    });
    let mut w = write.lock().await;
    let _ = write_line(&mut *w, &payload).await;
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
    fn parse_pin_request_round_trips() {
        let v = json!({
            "trace_json": { "inputs": [1, 2, 3], "decision": "uphold" },
            "metadata": { "agent": "daman-arbiter", "version": "0.1.0" },
            "request_id": "r-1"
        });
        let req: PinRequest = serde_json::from_value(v).unwrap();
        assert_eq!(req.request_id.as_deref(), Some("r-1"));
    }

    #[test]
    fn parse_upload_response_extracts_cid() {
        let payload = json!({
            "cid": "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
            "carCid": "bagbaiera...",
            "type": "Multipart"
        });
        let cid = parse_upload_response(&payload).unwrap();
        assert!(cid.starts_with("bafybei"));
    }

    #[test]
    fn parse_upload_response_errors_on_missing_cid() {
        let payload = json!({ "error": "unauthorized" });
        assert!(parse_upload_response(&payload).is_err());
    }

    #[test]
    fn unwrap_payload_handles_gossip_wrapper() {
        let env = json!({
            "chi": "gossip-publish",
            "topic": "daman/trace",
            "payload": { "chi": "pin-trace", "args": {} }
        });
        let inner = unwrap_payload(&env);
        assert_eq!(inner.get("chi").and_then(Value::as_str), Some("pin-trace"));
    }
}
