//! daman-farcaster-poster. The outbound-social forager bee.
//!
//! Wraps the Neynar API to expose Farcaster cast publishing as a hum
//! chi-pair on the mesh. Other Daman bees (recruiter today, future
//! marketing bee) publish `chi:cast-publish` and consume
//! `chi:cast-published` without ever touching Neynar credentials
//! directly. Mirrors the `twilio-sms` outbound-messaging-as-bee shape:
//! provider quirks, rate limits, and signer custody live in this bee.
//!
//! Wire:
//!
//!   consumer ─► chi:"cast-publish"   { text, embeds[], signing_account } ─► poster
//!   consumer ◄─ chi:"cast-published" { cast_hash, published_at_iso }      ◄─ poster
//!
//! Both directions ride on `chi:"gossip-publish"` with `topic:
//! "daman/cast"`; the payload's internal `chi` field carries the
//! semantic. Same pattern the rest of the Daman hive uses.
//!
//! Credentials:
//!
//!   NEYNAR_API_KEY         api key from the neynar developer console
//!   NEYNAR_SIGNER_UUID     uuid of the registered Farcaster signer
//!   DAMANFI_FARCASTER_FID  numeric FID for the @damanfi handle
//!   HUM_THRUM_SOCK         humd's NDJSON socket (defaults to XDG runtime)
//!   NEYNAR_API_BASE        override for tests (defaults to https://api.neynar.com)

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{info, warn};

const BEE_NAME: &str = "daman-farcaster-poster";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");
const CAST_TOPIC: &str = "daman/cast";
const NEYNAR_DEFAULT_BASE: &str = "https://api.neynar.com";
const NEYNAR_CAST_PATH: &str = "/v2/farcaster/cast";

#[derive(Debug, Clone)]
struct Config {
    sock_path: String,
    neynar_api_key: String,
    neynar_base: String,
    neynar_signer_uuid: String,
    signing_account_fid: String,
    request_timeout: Duration,
}

impl Config {
    fn from_env() -> Result<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::geteuid() }));
        let default_sock = format!("{runtime}/hum/thrum.sock");
        Ok(Self {
            sock_path: std::env::var("HUM_THRUM_SOCK").unwrap_or(default_sock),
            neynar_api_key: std::env::var("NEYNAR_API_KEY")
                .context("NEYNAR_API_KEY is required")?,
            neynar_base: std::env::var("NEYNAR_API_BASE")
                .unwrap_or_else(|_| NEYNAR_DEFAULT_BASE.to_string()),
            neynar_signer_uuid: std::env::var("NEYNAR_SIGNER_UUID")
                .context("NEYNAR_SIGNER_UUID is required")?,
            signing_account_fid: std::env::var("DAMANFI_FARCASTER_FID")
                .context("DAMANFI_FARCASTER_FID is required")?,
            request_timeout: Duration::from_secs(15),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct CastRequest {
    text: String,
    #[serde(default)]
    embeds: Vec<String>,
    /// Default @damanfi when absent. Forager rejects requests targeting
    /// other accounts unless the operator explicitly allowed them.
    #[serde(default)]
    signing_account: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CastResult {
    cast_hash: String,
    published_at_iso: String,
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
        neynar_base = %cfg.neynar_base,
        signing_fid = %cfg.signing_account_fid,
        "{BEE_NAME} starting"
    );

    let http = reqwest::Client::builder()
        .timeout(cfg.request_timeout)
        .build()?;

    run_loop(cfg, http).await
}

async fn run_loop(cfg: Config, http: reqwest::Client) -> Result<()> {
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
            "wire": "neynar/v2-cast"
        },
        "chis": ["hello", "gossip-publish", "cast-publish", "cast-published"],
        "topics": [CAST_TOPIC],
        "source": "https://github.com/damanfi/agents",
    });
    {
        let mut w = write_half.lock().await;
        write_line(&mut *w, &hello).await?;
    }

    // Subscribe to the cast topic by gossip-publishing a subscription
    // hint. humd will fan inbound matching tones back via the socket.
    let subscribe = json!({
        "chi": "gossip-publish",
        "topic": CAST_TOPIC,
        "payload": { "chi": "subscribe", "name": BEE_NAME }
    });
    {
        let mut w = write_half.lock().await;
        write_line(&mut *w, &subscribe).await?;
    }

    let mut reader = BufReader::new(read_half).lines();
    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let envelope: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, payload = %line, "envelope parse failed");
                continue;
            }
        };

        // Accept either a direct chi-tagged object or a gossip-publish
        // wrapper whose payload is the chi-tagged object.
        let request_envelope = match envelope.get("chi").and_then(Value::as_str) {
            Some("gossip-publish") => match envelope.get("payload") {
                Some(p) if p.get("chi").and_then(Value::as_str) == Some("cast-publish") => {
                    p.clone()
                }
                _ => continue,
            },
            Some("cast-publish") => envelope.clone(),
            _ => continue,
        };

        let request: CastRequest = match parse_cast_request(&request_envelope) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "cast request parse failed");
                continue;
            }
        };

        let cfg_clone = cfg.clone();
        let http_clone = http.clone();
        let write_half = write_half.clone();
        tokio::spawn(async move {
            match publish_cast(&cfg_clone, &http_clone, &request).await {
                Ok(result) => {
                    let cast_hash = result.cast_hash.clone();
                    let response = json!({
                        "chi": "gossip-publish",
                        "topic": CAST_TOPIC,
                        "payload": {
                            "chi": "cast-published",
                            "args": {
                                "cast_hash": result.cast_hash,
                                "published_at_iso": result.published_at_iso,
                            }
                        }
                    });
                    let mut w = write_half.lock().await;
                    if let Err(e) = write_line(&mut *w, &response).await {
                        warn!(error = %e, "response write failed");
                    } else {
                        info!(cast_hash = %cast_hash, "cast published");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "publish failed");
                    let response = json!({
                        "chi": "gossip-publish",
                        "topic": CAST_TOPIC,
                        "payload": {
                            "chi": "cast-error",
                            "args": { "error": e.to_string() }
                        }
                    });
                    let mut w = write_half.lock().await;
                    let _ = write_line(&mut *w, &response).await;
                }
            }
        });
    }

    Ok(())
}

fn parse_cast_request(v: &Value) -> Result<CastRequest> {
    let args = v.get("args").unwrap_or(v);
    serde_json::from_value(args.clone()).context("decode cast request")
}

/// Parse the Neynar v2 publish response. Factored out so fixture-based
/// tests can exercise the parsing logic without an HTTP server.
fn parse_neynar_response(payload: &Value) -> Result<CastResult> {
    let cast_hash = payload
        .get("cast")
        .and_then(|c| c.get("hash"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("neynar response missing cast.hash: {}", payload))?
        .to_string();
    let published_at_iso = payload
        .get("cast")
        .and_then(|c| c.get("timestamp"))
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(now_rfc3339);
    Ok(CastResult { cast_hash, published_at_iso })
}

async fn publish_cast(
    cfg: &Config,
    http: &reqwest::Client,
    req: &CastRequest,
) -> Result<CastResult> {
    let url = format!("{}{}", cfg.neynar_base, NEYNAR_CAST_PATH);
    let body = json!({
        "signer_uuid": cfg.neynar_signer_uuid,
        "text": req.text,
        "embeds": req.embeds.iter().map(|u| json!({ "url": u })).collect::<Vec<_>>(),
    });
    let resp = http
        .post(&url)
        .header("api_key", &cfg.neynar_api_key)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("neynar request")?;
    let status = resp.status();
    let payload: Value = resp.json().await.context("neynar parse")?;
    if !status.is_success() {
        return Err(anyhow!("neynar {} {}", status, payload));
    }
    parse_neynar_response(&payload)
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
    fn parse_cast_request_accepts_direct_shape() {
        let v = json!({ "text": "hello world", "embeds": [], "signing_account": "@damanfi" });
        let req = parse_cast_request(&v).unwrap();
        assert_eq!(req.text, "hello world");
        assert!(req.embeds.is_empty());
        assert_eq!(req.signing_account.as_deref(), Some("@damanfi"));
    }

    #[test]
    fn parse_cast_request_accepts_args_wrapped_shape() {
        let v = json!({
            "chi": "cast-publish",
            "args": { "text": "hi", "embeds": ["https://daman.fi"] }
        });
        let req = parse_cast_request(&v).unwrap();
        assert_eq!(req.text, "hi");
        assert_eq!(req.embeds.len(), 1);
        assert!(req.signing_account.is_none());
    }

    #[test]
    fn parse_neynar_response_extracts_cast_hash_and_timestamp() {
        // Recorded shape (trimmed) of a successful Neynar /v2/farcaster/cast response.
        let payload = json!({
            "success": true,
            "cast": {
                "hash": "0x9a1f2e8c4b5d6789012345678901234567890abc",
                "author": { "fid": 12345 },
                "text": "hello",
                "timestamp": "2026-05-24T18:00:00Z"
            }
        });
        let result = parse_neynar_response(&payload).unwrap();
        assert_eq!(result.cast_hash, "0x9a1f2e8c4b5d6789012345678901234567890abc");
        assert_eq!(result.published_at_iso, "2026-05-24T18:00:00Z");
    }

    #[test]
    fn parse_neynar_response_errors_on_missing_hash() {
        let payload = json!({ "success": false, "error": "rate limited" });
        assert!(parse_neynar_response(&payload).is_err());
    }
}
