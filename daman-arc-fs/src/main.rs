//! daman-arc-fs binary entry point. Connects to the local humd over the thrum NDJSON
//! socket, emits the hello manifest, then dispatches incoming `chi:"tool-call"` tones
//! through the unified Handler (alloy-backed, keyring-scoped) and emits `chi:"tool-result"`
//! back via humd's reverse-route map.
//!
//! Configuration at boot:
//! - `HUM_THRUM_SOCK` (or `$XDG_RUNTIME_DIR/hum/thrum.sock` or `/run/user/<uid>/...`)
//! - `ARC_TESTNET_RPC` (default https://rpc.testnet.arc.network)
//! - `DAMAN_ARC_FS_KEYRING` (default `~/.config/hum/daman-arc-fs/keyring.json`, 0600)
//! - `DAMAN_ARC_FS_CONFIG`  (default `~/.config/hum/daman-arc-fs/config.json`)

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use daman_arc_fs::hello::build_hello;
use daman_arc_fs::tools::DamanAddrs;
use daman_arc_fs::Handler;
use reverb_arc_fs::config::{Config, RateLimit};
use reverb_arc_fs::keyring::Keyring;
use reverb_arc_fs::safety::RateLimiter;
use reverb_arc_fs::tools::{ToolCall, ToolResult};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{info, warn};

const BEE_NAME: &str = "daman-arc-fs";
const BEE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let sock_path = sock_path();
    let rpc_url = std::env::var("ARC_TESTNET_RPC")
        .unwrap_or_else(|_| "https://rpc.testnet.arc.network".to_string());
    let chain_id: u64 = std::env::var("ARC_CHAIN_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5042002);

    info!(sock = %sock_path, rpc = %rpc_url, chain_id, "{BEE_NAME} starting");

    let keyring = load_keyring()?;
    let config = load_config(&rpc_url, chain_id)?;
    let rate_limiter = Arc::new(RateLimiter::new(config.rate_limit.clone()));
    let handler = Arc::new(Handler::new(
        rpc_url,
        chain_id,
        DamanAddrs::default(),
        Arc::new(keyring),
        Arc::new(config),
        rate_limiter,
    ));

    let stream = UnixStream::connect(&sock_path)
        .await
        .with_context(|| format!("connect to humd at {sock_path}"))?;
    let (read_half, write_half) = stream.into_split();
    let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

    // Emit canonical forager hello. humd routes tool-calls by tool_name to bees whose
    // hello declared `bee: ["forager"]`; the substrate's `Hello::base()` puts the bee
    // name in the `bee` field which doesn't match humd's routing predicate, so we build
    // the on-wire envelope by hand here matching the shape in hum/hives/common/src/forager.rs.
    let tools_value: Vec<Value> = daman_arc_fs::tools::catalog()
        .iter()
        .map(|t| serde_json::json!({
            "name": t.name,
            "description": format!("Daman tool: {}", t.name),
            "inputSchema": {"type": "object", "additionalProperties": true},
        }))
        .collect();
    let tool_names: Vec<&str> = daman_arc_fs::tools::catalog().iter().map(|t| t.name).collect();
    let hid_hex = format!("{:0>64}", "daman-arc-fs-stub-hid");
    let hello_envelope = serde_json::json!({
        "chi": "hello",
        "bee": ["forager"],
        "hid": &hid_hex,
        "from": &hid_hex,
        "hive": BEE_NAME,
        "version": BEE_VERSION,
        "protoVersion": "0.7.0",
        "tools": tools_value,
        "toolNames": tool_names,
        "provides": ["daman"],
        "chis": ["hello", "tool-call", "tool-result", "cancel", "breath", "echo", "gossip-publish"],
        "source": "https://github.com/damanfi/agents/tree/main/daman-arc-fs",
        "propensity": {
            "statefulness": "stateful",
            "richness": "rich",
            "wire": "daman/arc-fs",
        }
    });
    write_line(&write_half, &hello_envelope).await?;
    info!("hello emitted: {} tools as forager hive `{}`", tool_names.len(), BEE_NAME);
    // legacy build_hello call retained but unused now
    let _ = build_hello(BEE_VERSION);

    let mut reader = BufReader::new(read_half).lines();
    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let frame: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, payload = %line, "frame parse failed");
                continue;
            }
        };
        let chi = frame.get("chi").and_then(|c| c.as_str()).unwrap_or("");
        if chi != "tool-call" {
            continue;
        }
        let call: ToolCall = match parse_tool_call(&frame) {
            Some(c) => c,
            None => continue,
        };
        let result = handler.dispatch(call).await;
        emit_tool_result(&write_half, &result).await?;
    }

    Ok(())
}

fn sock_path() -> String {
    if let Ok(p) = std::env::var("HUM_THRUM_SOCK") {
        return p;
    }
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return format!("{rt}/hum/thrum.sock");
    }
    let uid = unsafe { libc::geteuid() };
    format!("/run/user/{uid}/hum/thrum.sock")
}

fn load_keyring() -> Result<Keyring> {
    let path = std::env::var("DAMAN_ARC_FS_KEYRING")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config/hum/daman-arc-fs/keyring.json")
        });
    if !path.exists() {
        warn!(path = %path.display(), "keyring path missing; starting with empty keyring (write tools will fail)");
        return Ok(Keyring::new());
    }
    Keyring::load(&path).map_err(|e| anyhow::anyhow!("keyring load failed: {e:?}"))
}

fn load_config(rpc_url: &str, chain_id: u64) -> Result<Config> {
    let path = std::env::var("DAMAN_ARC_FS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config/hum/daman-arc-fs/config.json")
        });
    if !path.exists() {
        warn!(path = %path.display(), "config missing; using defaults with full Daman + substrate allowed_contracts");
        let addrs = DamanAddrs::default();
        return Ok(Config {
            rpc_url: rpc_url.to_string(),
            explorer_api: "https://testnet.arcscan.app/api/v2".to_string(),
            chain_id,
            allowed_contracts: vec![
                addrs.copy_bond.clone(),
                addrs.bounty_accrual.clone(),
                addrs.reputation_registry.clone(),
                addrs.bond_yield_vault.clone(),
                addrs.universe_registry.clone(),
                addrs.benevolence.clone(),
                addrs.agent_registry.clone(),
                addrs.refund_protocol.clone(),
            ],
            rate_limit: RateLimit::default(),
        });
    }
    Config::load(&path).map_err(|e| anyhow::anyhow!("config load failed: {e:?}"))
}

fn parse_tool_call(frame: &Value) -> Option<ToolCall> {
    Some(ToolCall {
        call_id: frame.get("callId").and_then(|v| v.as_str())?.to_string(),
        from: frame.get("from").and_then(|v| v.as_str())?.to_string(),
        as_bee: frame
            .get("args")
            .and_then(|a| a.get("as_bee"))
            .and_then(|v| v.as_str())
            .map(String::from),
        tool_name: frame.get("toolName").and_then(|v| v.as_str())?.to_string(),
        args: frame.get("args").cloned().unwrap_or(Value::Null),
    })
}

async fn emit_tool_result(
    write_half: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    result: &ToolResult,
) -> Result<()> {
    let payload = json!({
        "chi": "tool-result",
        "callId": result.call_id,
        "ok": result.ok,
        "value": result.value,
        "error": result.error,
    });
    write_line(write_half, &payload).await
}

async fn write_line(
    handle: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    v: &Value,
) -> Result<()> {
    let s = serde_json::to_string(v)?;
    let mut bytes = s.into_bytes();
    bytes.push(b'\n');
    let mut guard = handle.lock().await;
    guard.write_all(&bytes).await?;
    Ok(())
}
