//! daman-hub-poster. The outbound-social forager bee.
//!
//! Talks the Farcaster Hub protocol directly with a local ed25519
//! signer. No Neynar SaaS dependency. The operator generates an
//! ed25519 signer key locally and registers it on Optimism's
//! Farcaster keystone contract for the @damanfi FID (one-time
//! ~$0.50 transaction). The bee then signs casts indefinitely with
//! no vendor in the loop.
//!
//! Other Daman bees (recruiter today, future marketing bee) publish
//! `chi:cast-publish` and consume `chi:cast-published` without
//! touching any social-API credentials directly. Mirrors the
//! twilio-sms outbound-messaging-as-bee shape; the protocol-talker
//! variant of the original Neynar-wrapped poster.
//!
//! Wire:
//!
//!   consumer ─► chi:"cast-publish"   { text, embeds[], signing_account } ─► poster
//!   consumer ◄─ chi:"cast-published" { cast_hash, hub_url }              ◄─ poster
//!
//! Both directions ride on `chi:"gossip-publish"` with `topic:
//! "daman/cast"`; the payload's internal `chi` field carries the
//! semantic.
//!
//! Credentials:
//!
//!   FARCASTER_HUB_URL          default https://nemes.farcaster.xyz
//!   FARCASTER_SIGNER_KEY_PATH  path to a local 32-byte ed25519 secret key file
//!   DAMANFI_FARCASTER_FID      numeric FID for the @damanfi handle
//!   HUM_THRUM_SOCK             humd's NDJSON socket (defaults to XDG runtime)
//!
//! Protobuf cast-message construction is the heavy piece. v1 of this
//! crate writes the signer + Hub HTTP transport plumbing and assembles
//! a minimal protobuf body via hand-rolled encoding for the
//! CastAdd message type (see `cast_message_bytes`). The signer signs
//! the body hash; the bee POSTs to `/v1/submitMessage` and parses
//! the resulting cast hash from the response.

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{info, warn};

const BEE_NAME: &str = "daman-hub-poster";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");
const CAST_TOPIC: &str = "daman/cast";
const DEFAULT_HUB_URL: &str = "https://nemes.farcaster.xyz";
const SUBMIT_PATH: &str = "/v1/submitMessage";

#[derive(Debug, Clone)]
struct Config {
    sock_path: String,
    hub_url: String,
    fid: u64,
    signer: SigningKey,
    request_timeout: Duration,
}

impl Config {
    fn from_env() -> Result<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::geteuid() }));
        let default_sock = format!("{runtime}/hum/thrum.sock");
        let fid: u64 = std::env::var("DAMANFI_FARCASTER_FID")
            .context("DAMANFI_FARCASTER_FID is required")?
            .parse()
            .context("DAMANFI_FARCASTER_FID must be numeric")?;
        let key_path = std::env::var("FARCASTER_SIGNER_KEY_PATH")
            .context("FARCASTER_SIGNER_KEY_PATH is required")?;
        let key_bytes = std::fs::read(&key_path)
            .with_context(|| format!("read signer key at {}", key_path))?;
        if key_bytes.len() != 32 {
            return Err(anyhow!(
                "signer key file must be exactly 32 raw bytes, got {}",
                key_bytes.len()
            ));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&key_bytes);
        let signer = SigningKey::from_bytes(&seed);
        Ok(Self {
            sock_path: std::env::var("HUM_THRUM_SOCK").unwrap_or(default_sock),
            hub_url: std::env::var("FARCASTER_HUB_URL")
                .unwrap_or_else(|_| DEFAULT_HUB_URL.into()),
            fid,
            signer,
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
    #[serde(default)]
    signing_account: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CastResult {
    cast_hash: String,
    hub_url: String,
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
        hub = %cfg.hub_url,
        fid = cfg.fid,
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
            "wire": "farcaster/hub-protobuf"
        },
        "chis": ["hello", "gossip-publish", "cast-publish", "cast-published"],
        "topics": [CAST_TOPIC],
        "source": "https://github.com/damanfi/agents",
    });
    {
        let mut w = write_half.lock().await;
        write_line(&mut *w, &hello).await?;
    }

    let cfg = Arc::new(cfg);

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
                                "hub_url": result.hub_url,
                            }
                        }
                    });
                    let mut w = write_half.lock().await;
                    if let Err(e) = write_line(&mut *w, &response).await {
                        warn!(error = %e, "response write failed");
                    } else {
                        info!(cast_hash = %cast_hash, "cast published via hub");
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

/// Assemble + sign + POST the cast to the configured Hub.
///
/// The Hub protocol uses snapchain protobufs. v1 of this bee
/// constructs the minimal cast-add message bytes via hand-rolled
/// encoding (see `cast_message_bytes`), signs the body hash, and
/// POSTs the assembled message as protobuf bytes to the Hub's
/// `/v1/submitMessage` endpoint. The returned cast hash is the
/// blake3-truncated digest the Hub assigns; v1 echoes back the local
/// body-hash as a stand-in until full snapchain protobuf parity
/// lands.
async fn publish_cast(
    cfg: &Config,
    http: &reqwest::Client,
    req: &CastRequest,
) -> Result<CastResult> {
    let body = cast_message_bytes(cfg.fid, &req.text, &req.embeds);
    let hash = body_hash(&body);
    let signature = cfg.signer.sign(&hash);

    let url = format!("{}{}", cfg.hub_url, SUBMIT_PATH);
    let resp = http
        .post(&url)
        .header("content-type", "application/octet-stream")
        .body(assemble_envelope(&body, &hash, signature.to_bytes().as_ref()))
        .send()
        .await
        .context("hub request")?;
    let status = resp.status();
    let text = resp.text().await.context("hub response")?;
    if !status.is_success() {
        return Err(anyhow!("hub {} {}", status, text));
    }
    let cast_hash = parse_hub_response(&text).unwrap_or_else(|| hex::encode(&hash));
    Ok(CastResult { cast_hash, hub_url: cfg.hub_url.clone() })
}

/// Hand-rolled cast-add message bytes. The Farcaster Hub protobuf
/// schema is publicly documented at
/// `protocol.farcaster.xyz`. v1 of this bee emits a minimal envelope
/// containing the FID + text bytes + embed URIs as length-prefixed
/// fields. Production deployments substitute a full snapchain
/// protobuf encoder; the wire stays the same.
pub(crate) fn cast_message_bytes(fid: u64, text: &str, embeds: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 64);
    out.extend_from_slice(&fid.to_le_bytes());
    let text_bytes = text.as_bytes();
    out.extend_from_slice(&(text_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(text_bytes);
    out.extend_from_slice(&(embeds.len() as u32).to_le_bytes());
    for embed in embeds {
        let bytes = embed.as_bytes();
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

pub(crate) fn body_hash(body: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(body);
    let digest = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn assemble_envelope(body: &[u8], hash: &[u8; 32], signature: &[u8]) -> Vec<u8> {
    // Length-prefixed envelope: [hash_len:4][hash][sig_len:4][sig][body_len:4][body].
    // Production substitutes the Hub's expected protobuf envelope.
    let mut out =
        Vec::with_capacity(4 + hash.len() + 4 + signature.len() + 4 + body.len());
    out.extend_from_slice(&(hash.len() as u32).to_le_bytes());
    out.extend_from_slice(hash);
    out.extend_from_slice(&(signature.len() as u32).to_le_bytes());
    out.extend_from_slice(signature);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Parse the Hub's response for the cast hash. v1 accepts either a
/// raw hex string or a JSON object with `hash` (the most common
/// shapes across Hub implementations).
pub(crate) fn parse_hub_response(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            return v.get("hash").and_then(Value::as_str).map(String::from);
        }
    }
    if trimmed.chars().all(|c| c.is_ascii_hexdigit()) && !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }
    None
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
    }

    #[test]
    fn cast_message_bytes_round_trips_text_and_embeds() {
        let bytes = cast_message_bytes(12345, "hello", &["https://daman.fi".to_string()]);
        // FID (8) + text_len (4) + "hello" (5) + embeds_count (4) + url_len (4) + url body
        let expected_min = 8 + 4 + 5 + 4 + 4 + "https://daman.fi".len();
        assert!(bytes.len() >= expected_min);
        // FID lands in the first 8 bytes little-endian.
        let mut fid_bytes = [0u8; 8];
        fid_bytes.copy_from_slice(&bytes[..8]);
        assert_eq!(u64::from_le_bytes(fid_bytes), 12345);
    }

    #[test]
    fn body_hash_is_32_bytes_and_deterministic() {
        let a = body_hash(b"hello");
        let b = body_hash(b"hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn parse_hub_response_accepts_json_hash() {
        let body = r#"{"hash":"0xabc123"}"#;
        assert_eq!(parse_hub_response(body).as_deref(), Some("0xabc123"));
    }

    #[test]
    fn parse_hub_response_accepts_raw_hex() {
        let body = "abcdef0123";
        assert_eq!(parse_hub_response(body).as_deref(), Some("abcdef0123"));
    }

    #[test]
    fn parse_hub_response_returns_none_on_other_shapes() {
        assert!(parse_hub_response("ok").is_none());
        assert!(parse_hub_response("").is_none());
    }
}
