//! Daman persona binary. One process per bee: one EOA, one hid, one namespaced tool surface,
//! one humd connection. Composes daman-arc-fs::daman_tools with the substrate's
//! PersonaForagerBuilder + BeeIdentity per BRIEF_PERSONA_AS_FORAGER.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use daman_arc_fs::{daman_tools, namespace_for_bee, DamanAddrs, DamanCtx};
use daman_personas::variant::{compose_system_prompt, Role};
use alloy::signers::local::PrivateKeySigner;
use reverb_arc_fs::{BeeIdentity, BeeRole, PersonaForagerBuilder, PrivateKey};
use reverb_arc_fs::tools::ToolCall;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{info, warn};

const CLAUDE_BUILT_IN_BLOCKLIST: &[&str] = &[
    "Bash", "BashOutput", "KillShell",
    "Read", "Edit", "Write", "MultiEdit", "NotebookEdit",
    "Glob", "Grep",
    "WebFetch", "WebSearch",
    "Task", "TodoWrite", "AskUserQuestion", "ExitPlanMode", "SlashCommand",
    "CronCreate", "CronDelete", "CronList", "ScheduleWakeup",
    "EnterPlanMode", "EnterWorktree", "ExitWorktree",
    "Monitor", "PushNotification", "Skill",
    "TaskCreate", "TaskGet", "TaskList", "TaskOutput", "TaskUpdate", "TaskStop",
    "ToolSearch", "RemoteTrigger", "ShareOnboardingGuide",
];

#[derive(Parser, Debug)]
#[command(name = "daman-persona")]
struct Cli {
    #[arg(long, env = "DAMAN_PERSONA_ROLE")]
    role: String,
    #[arg(long, env = "DAMAN_PERSONA_VARIANT", default_value = "alpha")]
    variant: String,
    #[arg(long, env = "DAMAN_PERSONA_BEE_NAME")]
    bee_name: String,
    #[arg(long, env = "DAMAN_PERSONA_EOA_ADDR")]
    eoa_addr: String,
    /// Path to the per-bee EOA private-key file (64-char hex, no 0x prefix, no newline).
    #[arg(long, env = "DAMAN_PERSONA_KEY_PATH")]
    key_path: PathBuf,
    #[arg(long, env = "DAMAN_PERSONA_SID")]
    sid: Option<String>,
    #[arg(long, env = "HUM_THRUM_SOCK")]
    sock_path: Option<String>,
    #[arg(long, env = "ARC_TESTNET_RPC", default_value = "https://rpc.testnet.arc.network")]
    rpc_url: String,
    #[arg(long, env = "ARC_CHAIN_ID", default_value = "5042002")]
    chain_id: u64,
    #[arg(long, env = "DAMAN_PERSONA_LOG_DIR")]
    log_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let role = Role::parse(&cli.role).context("--role must be leader|follower|watchdog|arbiter|relief")?;
    let sid = cli.sid.unwrap_or_else(|| format!("sid-{}", cli.bee_name));
    let sock_path = cli.sock_path.unwrap_or_else(sock_path_default);
    let namespace = namespace_for_bee(&cli.bee_name);

    // Load per-bee EOA private key (64-char hex, no 0x).
    let key_hex = std::fs::read_to_string(&cli.key_path)
        .with_context(|| format!("read key file {}", cli.key_path.display()))?
        .trim()
        .to_string();
    let private_key = PrivateKey::new(format!("0x{key_hex}"))
        .context("PrivateKey parse")?;
    let signer = PrivateKeySigner::from_str(&key_hex).context("PrivateKeySigner parse")?;

    // Mint or load the persona's stable ed25519 hid. humd dedupes manifests by hid;
    // without a stable hid every reconnect leaks a fresh manifest entry.
    let identity = BeeIdentity::load_or_mint_with_role(&cli.bee_name, BeeRole::Forager)
        .context("BeeIdentity::load_or_mint_with_role")?;

    info!(
        bee = %cli.bee_name,
        role = %cli.role,
        variant = %cli.variant,
        namespace = %namespace,
        eoa = %cli.eoa_addr,
        hid = %identity.hid_string(),
        sid = %sid,
        sock = %sock_path,
        "persona starting"
    );

    // Build the per-bee tool set.
    let addrs = DamanAddrs::default();
    let ctx = DamanCtx::new(
        cli.bee_name.clone(),
        cli.rpc_url.clone(),
        cli.chain_id,
        addrs.clone(),
        signer.clone(),
    );
    let tools = daman_tools(ctx, &namespace);

    // Compose the forager via the substrate builder.
    let forager = PersonaForagerBuilder::default()
        .bee_name(cli.bee_name.clone())
        .namespace(namespace.clone())
        .identity(identity)
        .private_key(private_key)
        .with_tools(tools)
        .allowed_contracts(addrs.allowlist())
        .wire("daman/arc-fs")
        .source("https://github.com/damanfi/agents/tree/main/daman-personas")
        .build()
        .map_err(|e| anyhow::anyhow!("PersonaForager build: {e:?}"))?;

    let registry = Arc::new(forager.tools);
    let bee_name = cli.bee_name.clone();

    // Connect to humd.
    let stream = UnixStream::connect(&sock_path)
        .await
        .with_context(|| format!("connect humd at {sock_path}"))?;
    let (read_half, write_half) = stream.into_split();
    let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

    // Emit forager hello (canonical shape via the substrate-built Hello).
    let hello = serde_json::to_value(&forager.hello)?;
    write_line(&write_half, &hello).await?;
    info!(tools = registry.len(), "forager hello emitted");

    // Emit bootstrap prompt to seed the first decision.
    let system_prompt = compose_system_prompt(role, &cli.variant, &cli.bee_name, &cli.eoa_addr);
    let bootstrap_text = bootstrap_directive(role, &namespace);
    let bootstrap = json!({
        "chi": "prompt",
        "sid": &sid,
        "from": bee_name,
        "modelId": "claude-opus-4-7",
        "systemPrompt": system_prompt,
        "text": bootstrap_text,
        "disallowedTools": CLAUDE_BUILT_IN_BLOCKLIST,
    });
    write_line(&write_half, &bootstrap).await?;
    info!(sid = %sid, "bootstrap prompt emitted");

    // Demux loop: read humd, dispatch chi:tool-call to registry, log finish/error.
    let mut reader = BufReader::new(read_half).lines();
    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let frame: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "frame parse failed");
                continue;
            }
        };
        let chi = frame.get("chi").and_then(|c| c.as_str()).unwrap_or("");
        match chi {
            "tool-call" => {
                let call = match parse_tool_call(&frame) {
                    Some(c) => c,
                    None => continue,
                };
                tracing::info!(tool = %call.tool_name, call_id = %call.call_id, "tool-call received");
                let tool = match registry.lookup(&call.tool_name) {
                    Some(t) => t,
                    None => {
                        warn!(tool = %call.tool_name, "unknown tool");
                        let res = reverb_arc_fs::tools::ToolResult::fail(
                            call.call_id,
                            reverb_arc_fs::ForagerError::UnknownTool { tool: call.tool_name.clone() },
                        );
                        emit_result(&write_half, &res, &sid).await?;
                        continue;
                    }
                };
                let result = tool.invoke(call).await;
                tracing::info!(ok = result.ok, "tool-result");
                emit_result(&write_half, &result, &sid).await?;
            }
            "finish" => {
                let usage = frame.get("usage").cloned().unwrap_or(Value::Null);
                let out_tokens = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                tracing::info!(out_tokens, "finish");
            }
            "chunk" => {
                tracing::debug!("chunk");
            }
            "error" => {
                let q = frame.get("qualifier").and_then(|v| v.as_str()).unwrap_or("");
                let d = frame.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                warn!(qualifier = %q, detail = %d, "error frame");
            }
            "session-ready" => {
                tracing::info!("session-ready");
            }
            "breath" => {}
            _ => {
                tracing::trace!(chi, "frame");
            }
        }
    }
    Ok(())
}

fn parse_tool_call(frame: &Value) -> Option<ToolCall> {
    Some(ToolCall {
        call_id: frame.get("callId").and_then(|v| v.as_str())?.to_string(),
        from: frame.get("from").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        as_bee: None,
        tool_name: frame.get("toolName").and_then(|v| v.as_str())?.to_string(),
        args: frame.get("args").cloned().unwrap_or(Value::Null),
    })
}

async fn emit_result(
    write_half: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    result: &reverb_arc_fs::tools::ToolResult,
    sid: &str,
) -> Result<()> {
    let payload = json!({
        "chi": "tool-result",
        "callId": result.call_id,
        "sid": sid,
        "ok": result.ok,
        "value": result.value,
        "error": result.error,
    });
    write_line(write_half, &payload).await
}

fn sock_path_default() -> String {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return format!("{rt}/hum/thrum.sock");
    }
    let uid = unsafe { libc::geteuid() };
    format!("/run/user/{uid}/hum/thrum.sock")
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

fn bootstrap_directive(role: Role, ns: &str) -> String {
    let action = match role {
        Role::Leader => format!(
            "Call mcp__hum__{ns}_register_leader with args {{tier: 0, claimedAum: \"10000000000000000000000\"}} to register as a retail-tier leader claiming 10000 USDC AUM. Do not explain; call the tool now."
        ),
        Role::Follower => format!(
            "Call mcp__hum__{ns}_read_reputation for a candidate leader address (start with 0x15f8A419eEd9Dc1e21C6bb86B06be979ad80De29). Then call mcp__hum__{ns}_subscribe with the leader and capital 1000000."
        ),
        Role::Watchdog => format!(
            "Call mcp__hum__{ns}_subscribe_to_role_events with args {{role: \"watchdog\"}} to open the event stream. Then idle until a degradation candidate appears."
        ),
        Role::Arbiter => format!(
            "Call mcp__hum__{ns}_subscribe_to_role_events with args {{role: \"arbiter\"}} to open the event stream. Then idle until a dispute lands."
        ),
        Role::Relief => format!(
            "Call mcp__hum__{ns}_subscribe_to_role_events with args {{role: \"relief\"}} to open the relief stream. Then idle."
        ),
    };
    format!("Bootstrap tick. {action}")
}
