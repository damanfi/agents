//! Daman operator binary. One process, one EOA (the deployer key), one hid, one narrow
//! tool surface, one humd connection. Plays both the `oracle` and the `arbiterAddr`
//! role on `DamanCopyBond` because the proxy's `initialize` write set both slots to
//! the deployer EOA and the implementation exposes no setters.
//!
//! Per `/tmp/audit/operator_persona.md` + `/tmp/audit/auth_ops.md` Path B: no on-chain
//! rotation, the operator daemon plays both privileged write seats in-process. Single
//! bee, no variant overlay, no leader/watchdog/arbiter prompt selection. The role
//! string for `register_agent` at boot is `"operator"`.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use daman_arc_fs::{namespace_for_bee, register_agent, DamanAddrs, RegisterOutcome};
use daman_operator::tools::{operator_tools, OperatorCtx};
use daman_operator::specs::operator_tool_specs;
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
#[command(name = "daman-operator")]
struct Cli {
    #[arg(long, env = "DAMAN_OPERATOR_BEE_NAME", default_value = "daman-operator")]
    bee_name: String,
    #[arg(long, env = "DAMAN_OPERATOR_EOA_ADDR", default_value = "")]
    eoa_addr: String,
    /// Path to the operator EOA private-key file (64-char hex, no 0x prefix, no newline).
    /// File must be mode 0600. The operator does NOT auto-copy the value from a repo
    /// .env; copy it manually so a stray chmod on the repo does not leak the key.
    #[arg(long, env = "DAMAN_OPERATOR_KEY_PATH")]
    key_path: PathBuf,
    #[arg(long, env = "DAMAN_OPERATOR_SID")]
    sid: Option<String>,
    #[arg(long, env = "HUM_THRUM_SOCK")]
    sock_path: Option<String>,
    #[arg(long, env = "ARC_TESTNET_RPC", default_value = "https://rpc.testnet.arc.network")]
    rpc_url: String,
    #[arg(long, env = "ARC_CHAIN_ID", default_value = "5042002")]
    chain_id: u64,
    #[arg(long, env = "DAMAN_OPERATOR_LOG_DIR")]
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
    let sid = cli.sid.clone().unwrap_or_else(|| format!("sid-{}", cli.bee_name));
    let sock_path = cli.sock_path.clone().unwrap_or_else(sock_path_default);
    let namespace = namespace_for_bee(&cli.bee_name);

    // Load the operator EOA key. Hard fail if the file is missing or mode is wider than 0600.
    check_key_file_mode(&cli.key_path)?;
    let key_hex = std::fs::read_to_string(&cli.key_path)
        .with_context(|| format!("read key file {}", cli.key_path.display()))?
        .trim()
        .to_string();
    if key_hex.len() != 64 || !key_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!(
            "operator key at {} must be 64-char hex with no 0x prefix and no newline",
            cli.key_path.display()
        );
    }
    let private_key = PrivateKey::new(format!("0x{key_hex}"))
        .context("PrivateKey parse")?;
    let signer = PrivateKeySigner::from_str(&key_hex).context("PrivateKeySigner parse")?;

    // Resolve the effective EOA: the CLI flag wins if set, else derive from the signer.
    let derived_eoa = format!("{:#x}", signer.address());
    let eoa_addr = if cli.eoa_addr.is_empty() {
        derived_eoa.clone()
    } else {
        cli.eoa_addr.clone()
    };

    // Stable ed25519 hid per BeeIdentity::load_or_mint_with_role.
    let identity = BeeIdentity::load_or_mint_with_role(&cli.bee_name, BeeRole::Forager)
        .context("BeeIdentity::load_or_mint_with_role")?;

    info!(
        bee = %cli.bee_name,
        role = "operator",
        namespace = %namespace,
        eoa = %eoa_addr,
        hid = %identity.hid_string(),
        sid = %sid,
        sock = %sock_path,
        "operator starting"
    );

    let addrs = DamanAddrs::default();
    let ctx = OperatorCtx::new(
        cli.bee_name.clone(),
        cli.rpc_url.clone(),
        cli.chain_id,
        addrs.clone(),
        signer.clone(),
    );

    // On-chain identity anchor before opening the humd handshake. The operator registers
    // under role "operator" rather than "oracle" because it also rules claims; the
    // role-of-record should match what it actually does.
    //
    // register_agent is shared with the persona binary and uses daman_arc_fs::DamanCtx;
    // build a DamanCtx wrapper for that one call only, with the same signer + addrs.
    let daman_ctx = daman_arc_fs::DamanCtx::new(
        cli.bee_name.clone(),
        cli.rpc_url.clone(),
        cli.chain_id,
        addrs.clone(),
        signer.clone(),
    );
    match register_agent(&daman_ctx, "operator").await {
        Ok(RegisterOutcome::Registered(tx)) => {
            info!(tx_hash = %tx, role = "operator", "agent.registered");
        }
        Ok(RegisterOutcome::AlreadyRegistered) => {
            info!(role = "operator", "agent.already-registered");
        }
        Err(e) => {
            warn!(error = %e, role = "operator", "agent.register.failed (proceeding to humd anyway)");
        }
    }

    let tools = operator_tools(ctx, &namespace);

    // Compose the forager via the substrate builder.
    let forager = PersonaForagerBuilder::default()
        .bee_name(cli.bee_name.clone())
        .namespace(namespace.clone())
        .identity(identity)
        .private_key(private_key)
        .with_tools(tools)
        .allowed_contracts(addrs.allowlist())
        .wire("daman/arc-fs")
        .source("https://github.com/damanfi/agents/tree/main/daman-operator")
        .build()
        .map_err(|e| anyhow::anyhow!("PersonaForager build: {e:?}"))?;

    let registry = Arc::new(forager.tools);
    let bee_name = cli.bee_name.clone();

    let stream = UnixStream::connect(&sock_path)
        .await
        .with_context(|| format!("connect humd at {sock_path}"))?;
    let (read_half, write_half) = stream.into_split();
    let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

    // Forager hello (canonical shape from the substrate-built Hello).
    let hello = serde_json::to_value(&forager.hello)?;
    write_line(&write_half, &hello).await?;
    info!(tools = registry.len(), "forager hello emitted");

    // Bootstrap chi:prompt. The prompt frames the operator's narrow scope and lists the
    // four tools; the tick loop then re-prompts the worker every 75s.
    let prompt_tools = operator_tool_specs(&namespace);
    let system_prompt = compose_system_prompt(&cli.bee_name, &eoa_addr);
    let bootstrap_text = bootstrap_directive(&cli.bee_name, &namespace);
    let bootstrap = json!({
        "chi": "prompt",
        "sid": &sid,
        "from": bee_name,
        "modelId": "claude-opus-4-7",
        "systemPrompt": system_prompt,
        "text": bootstrap_text,
        "tools": prompt_tools,
        "disallowedTools": CLAUDE_BUILT_IN_BLOCKLIST,
    });
    write_line(&write_half, &bootstrap).await?;
    info!(sid = %sid, tool_count = registry.len(), "bootstrap prompt emitted");

    // Turn-lifecycle bookkeeping. The tick gate only fires when the prior prompt has
    // finished and a cooldown has elapsed, mirroring daman-personas.
    let last_finish = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let last_prompt_sent = Arc::new(std::sync::atomic::AtomicU64::new(now_ms_u64()));

    {
        let last_finish = last_finish.clone();
        let last_prompt_sent = last_prompt_sent.clone();
        let write_half = write_half.clone();
        let sid = sid.clone();
        let bee_name = cli.bee_name.clone();
        let namespace = namespace.clone();
        let system_prompt = system_prompt.clone();
        let prompt_tools = prompt_tools.clone();
        let disallowed: Vec<String> = CLAUDE_BUILT_IN_BLOCKLIST.iter().map(|s| s.to_string()).collect();
        tokio::spawn(async move {
            let tick_interval = std::time::Duration::from_secs(75);
            let cooldown_ms: u64 = 30_000;
            let mut counter: u32 = 0;
            loop {
                tokio::time::sleep(tick_interval).await;
                let now = now_ms_u64();
                let lf = last_finish.load(std::sync::atomic::Ordering::SeqCst);
                let lps = last_prompt_sent.load(std::sync::atomic::Ordering::SeqCst);
                if lf <= lps {
                    tracing::info!(skip = "prior-in-flight", "tick skipped");
                    continue;
                }
                if now.saturating_sub(lf) < cooldown_ms {
                    continue;
                }
                counter = counter.saturating_add(1);
                let tick_text = tick_directive(&namespace, counter);
                let prompt = json!({
                    "chi": "prompt",
                    "sid": &sid,
                    "from": bee_name,
                    "modelId": "claude-opus-4-7",
                    "systemPrompt": system_prompt,
                    "text": tick_text,
                    "tools": prompt_tools,
                    "disallowedTools": disallowed,
                });
                if let Err(e) = write_line(&write_half, &prompt).await {
                    tracing::warn!(error = %e, "tick prompt send failed");
                    continue;
                }
                last_prompt_sent.store(now_ms_u64(), std::sync::atomic::Ordering::SeqCst);
                tracing::info!(tick = counter, sid = %sid, "tick prompt emitted");
            }
        });
    }

    // Demux loop: read humd, dispatch chi:tool-call to registry, log finish + chunks.
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
                last_finish.store(now_ms_u64(), std::sync::atomic::Ordering::SeqCst);
                tracing::info!(out_tokens, "finish");
            }
            "chunk" => {
                let chunk_type = frame
                    .get("chunkType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let text = frame
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .or_else(|| frame.get("text").and_then(|v| v.as_str()))
                    .unwrap_or("");
                if !text.is_empty() {
                    tracing::info!(chunk_type = %chunk_type, chars = text.len(), text = %text, "chunk text");
                }
            }
            "session-ready" => {
                let tools = frame
                    .get("tools")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|t| t.as_str()).collect::<Vec<_>>().join(","))
                    .unwrap_or_default();
                let mcp = frame
                    .get("mcp_servers")
                    .or_else(|| frame.get("mcpServers"))
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                tracing::info!(tools = %tools, mcp = %mcp, "session-ready");
            }
            "error" => {
                let q = frame.get("qualifier").and_then(|v| v.as_str()).unwrap_or("");
                let d = frame.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                warn!(qualifier = %q, detail = %d, "error frame");
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

fn friendly_handle(bee_name: &str) -> String {
    if bee_name == "daman-operator" {
        return "Operator".to_string();
    }
    if let Some(rest) = bee_name.strip_prefix("daman-operator-") {
        return format!("Operator {rest}");
    }
    bee_name.to_string()
}

fn compose_system_prompt(bee_name: &str, eoa_addr: &str) -> String {
    let handle = friendly_handle(bee_name);
    format!(
        "Daman is a slash-bonded copy-trading substrate. Leaders post bonds, followers \
mirror their trades, watchdogs file slash-claims, arbiters rule. The substrate enforces \
all rules on chain.\n\n\
You are {handle}. Your wallet address is {eoa_addr}. You hold the deployer key, which \
the proxy's initialize() pinned as both the `oracle` and the `arbiterAddr` on \
DamanCopyBond. No other address can submit recordTrade or arbiterRule. You are the only \
process in the swarm that can clear those gates.\n\n\
Your scope is narrow:\n\
  - Submit recordTrade(leader, asset, amount, isLong) when a leader's trade-claim is \
    valid (leader active, asset eligible, isLong true).\n\
  - Submit arbiterRule(claimId, slashAmount, upheld, builder, traceCid) on claims whose \
    dispute window has closed. Uphold when evidence shows an out-of-universe asset; \
    reject when evidence is thin.\n\
  - Read getLeader + getClaim to validate inputs and size slash amounts.\n\n\
You do not register as a leader, post bonds, subscribe, file claims, or take any other \
write that is not in your four-tool surface. Your reputation is the swarm's reputation; \
mistakes you make appear on chain under the deployer's address.\n\n\
Operating loop: ticks roughly every 75 seconds. Each tick: scan for pending claims via \
read_claim and rule the ones ready. Trade-ingest is currently log-only pending the \
hum-gossip-bridge; do not synthesize trades on your own. State your reasoning briefly \
before each tool call so the audit log stays legible.\n\n\
USDC has 6 decimals throughout. 1 USDC = 1_000_000 base units."
    )
}

fn bootstrap_directive(bee_name: &str, namespace: &str) -> String {
    let _ = bee_name;
    format!(
        "First boot. Goal: confirm you're online and idle.\n\n\
1. State your role and wallet address (from the system prompt) in one sentence.\n\
2. Do NOT call any tool this turn. The next tick (~75s) will prompt you to scan for \
   actionable claims via {namespace}_read_claim.\n\
3. Stop."
    )
}

fn tick_directive(namespace: &str, tick: u32) -> String {
    format!(
        "Tick {tick}. Two responsibilities.\n\n\
1. Trade-ingest: gossip-bridge for daman/trade-claims/* is not yet wired. Log \
   `trade-ingest pending hum-gossip-bridge` in your reasoning and move on. Do NOT \
   synthesize a recordTrade call without an upstream claim.\n\
2. Active-claim ruling: call {namespace}_read_claim for any claim id you have seen \
   recently (from prior ticks or from gossip when it's wired). For each pending claim:\n\
   - Skip if status is already Upheld (3) or Rejected (4).\n\
   - Skip if disputeWindowEnds is still in the future.\n\
   - Otherwise, decide:\n\
     * Uphold when the claim's evidence references an out-of-universe asset. Call \
       {namespace}_read_leader_state for the claim's leader, then call \
       {namespace}_operator_rule_claim with slashAmount = 25% of bondAmount (the \
       substrate caps slashes at 25%; use the cap as the upheld amount).\n\
     * Reject otherwise. Submit with slashAmount = \"0\" and upheld = false. Default \
       deny when evidence is thin.\n\
3. If no claim ids are known yet, idle this tick. The next tick will arrive in ~75s."
    )
}

fn now_ms_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Hard-fail if the key file is missing or world/group readable. Mode 0600 only.
fn check_key_file_mode(path: &PathBuf) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)
        .with_context(|| format!("stat operator key at {}", path.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        anyhow::bail!(
            "operator key at {} has mode {:o}; required 0600 (chmod 600 it before retrying)",
            path.display(),
            mode
        );
    }
    Ok(())
}
