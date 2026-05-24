//! daman-trace-pinner. The IPFS storage forager bee.
//!
//! Talks directly to a local kubo node's HTTP API (the IPFS reference
//! implementation) to pin reasoning-trace JSON. The CID returned is
//! content-addressable and re-fetchable from any IPFS gateway. No
//! web3.storage / Storacha credential is required: the operator runs
//! a kubo container alongside the watchdog farm and the bee dials its
//! local HTTP port.
//!
//! Mirrors the humfs storage-as-bee template from hum. The chi
//! vocabulary is unchanged from prior SaaS-wrapper iterations so
//! consumer agents (watchdog, arbiter) need no refactor.
//!
//! Wire (gossip-publish wrappers; payload chi is the semantic):
//!
//!   consumer ─► chi:"pin-trace"   { trace_json, metadata } ─► pinner
//!   consumer ◄─ chi:"trace-pinned" { cid, pin_method }      ◄─ pinner
//!
//! Configure:
//!
//!   HUM_THRUM_SOCK   humd's NDJSON socket (defaults to XDG runtime)
//!   KUBO_API_URL     default http://localhost:5001
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

const BEE_NAME: &str = "daman-trace-pinner";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");
const TRACE_TOPIC: &str = "daman/trace";
const DEFAULT_KUBO_URL: &str = "http://localhost:5001";
const KUBO_ADD_PATH: &str = "/api/v0/add";

#[derive(Debug, Clone)]
struct Config {
    sock_path: String,
    kubo_api_url: String,
    request_timeout: Duration,
}

impl Config {
    fn from_env() -> Result<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::geteuid() }));
        let default_sock = format!("{runtime}/hum/thrum.sock");
        Ok(Self {
            sock_path: std::env::var("HUM_THRUM_SOCK").unwrap_or(default_sock),
            kubo_api_url: std::env::var("KUBO_API_URL")
                .unwrap_or_else(|_| DEFAULT_KUBO_URL.into()),
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
        kubo = %cfg.kubo_api_url,
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
            "wire": "ipfs/kubo-http"
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
    match upload_to_kubo(cfg, http, &req).await {
        Ok(cid) => {
            let result = PinResult {
                cid: cid.clone(),
                pin_method: "ipfs/kubo".into(),
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

/// POST the trace JSON to kubo's `/api/v0/add` endpoint as a multipart
/// file upload. Kubo returns a single-line NDJSON response containing
/// the `Hash` field with the resulting CID. The chunk is pinned to
/// the local node by default; the CID is content-addressable and
/// re-fetchable from any IPFS gateway.
async fn upload_to_kubo(
    cfg: &Config,
    http: &reqwest::Client,
    req: &PinRequest,
) -> Result<String> {
    let url = format!("{}{}?pin=true", cfg.kubo_api_url, KUBO_ADD_PATH);
    let body = serde_json::to_vec(&json!({
        "trace": req.trace_json,
        "metadata": req.metadata,
    }))?;
    let part = reqwest::multipart::Part::bytes(body)
        .file_name("trace.json")
        .mime_str("application/json")
        .context("multipart mime")?;
    let form = reqwest::multipart::Form::new().part("file", part);
    let resp = http
        .post(&url)
        .multipart(form)
        .send()
        .await
        .context("kubo request")?;
    let status = resp.status();
    let text = resp.text().await.context("kubo response body")?;
    if !status.is_success() {
        return Err(anyhow!("kubo {} {}", status, text));
    }
    parse_kubo_add_response(&text)
}

/// Extract the CID from kubo's `/api/v0/add` response. The response is
/// NDJSON; the final non-empty line carries the file's CID under
/// `Hash`. Factored out for fixture-based testing.
fn parse_kubo_add_response(body: &str) -> Result<String> {
    let last_line = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .last()
        .ok_or_else(|| anyhow!("empty kubo response"))?;
    let v: Value =
        serde_json::from_str(last_line).context("kubo NDJSON line not JSON")?;
    v.get("Hash")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("missing Hash in kubo response: {}", last_line))
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
    fn parse_kubo_add_response_extracts_cid() {
        let body = r#"{"Name":"trace.json","Hash":"QmZjT8MgKM7hY7vXxBgnNQyHbS3hRdGRkrK6tF3kEx5e8b","Size":"512"}"#;
        let cid = parse_kubo_add_response(body).unwrap();
        assert!(cid.starts_with("Qm") || cid.starts_with("bafy"));
    }

    #[test]
    fn parse_kubo_add_response_handles_multiline_responses() {
        let body = "{\"Name\":\"a\",\"Hash\":\"Qmfirst\",\"Size\":\"100\"}\n{\"Name\":\"b\",\"Hash\":\"Qmsecond\",\"Size\":\"200\"}";
        let cid = parse_kubo_add_response(body).unwrap();
        assert_eq!(cid, "Qmsecond");
    }

    #[test]
    fn parse_kubo_add_response_errors_on_missing_hash() {
        let body = r#"{"Name":"trace.json","Size":"512"}"#;
        assert!(parse_kubo_add_response(body).is_err());
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
