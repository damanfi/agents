//! Daman persona binary. One process per bee: one EOA, one hid, one namespaced tool surface,
//! one humd connection. Composes daman-arc-fs::daman_tools with the substrate's
//! PersonaForagerBuilder + BeeIdentity per BRIEF_PERSONA_AS_FORAGER.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use daman_arc_fs::{
    daman_tool_specs, daman_tools, fetch_bee_state, namespace_for_bee, register_agent,
    render_state_block, DamanAddrs, DamanCtx, RegisterOutcome,
};
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

    // Guaranteed on-chain identity anchor BEFORE we open the humd handshake.
    // Per-bee role registration on DamanAgentRegistry; the bee's role-of-record
    // exists on chain before any claude turn fires, so downstream readers do
    // not depend on whichever turn happened to call the equivalent tool.
    // AlreadyRegistered (steady-state after the first boot) is treated as
    // success. Hard-fails are warned and skipped to avoid blocking boot on a
    // transient RPC issue; the bee can still operate via humd, the gap is
    // only the on-chain role-of-record being one turn late.
    match register_agent(&ctx, cli.role.as_str()).await {
        Ok(RegisterOutcome::Registered(tx)) => {
            info!(tx_hash = %tx, role = %cli.role, "agent.registered");
        }
        Ok(RegisterOutcome::AlreadyRegistered) => {
            info!(role = %cli.role, "agent.already-registered");
        }
        Err(e) => {
            warn!(error = %e, role = %cli.role, "agent.register.failed (proceeding to humd anyway)");
        }
    }

    let tools = daman_tools(ctx.clone(), &namespace);

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

    // Emit bootstrap prompt to seed the first decision. The substrate's
    // PersonaForagerBuilder already shipped the tool defs in hello, but
    // hum's canonical pattern (per WIRE.md) is for the asker to also ride
    // tools on every chi:"prompt" so the worker reliably sees them in
    // each turn's `tools` array. Belt + suspenders.
    let prompt_tools = daman_tool_specs(&namespace);
    let system_prompt = compose_system_prompt(role, &cli.variant, &cli.bee_name, &cli.eoa_addr);
    let bootstrap_text = compose_prompt_body(
        &ctx,
        bootstrap_directive(role, &cli.variant, &cli.bee_name),
    )
    .await;
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

    // Track turn lifecycle so the tick loop never overlaps an in-flight turn.
    // `last_finish` is updated when humd forwards a `finish` frame; `last_prompt_sent`
    // is updated when this binary writes a prompt. The tick gate fires only when
    // `last_finish > last_prompt_sent` AND a cooldown has elapsed.
    let last_finish = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let last_prompt_sent = Arc::new(std::sync::atomic::AtomicU64::new(now_ms_u64()));
    last_finish.store(0, std::sync::atomic::Ordering::SeqCst);

    // Spawn the tick loop. Continues for the lifetime of the persona process;
    // `hum bee <bee> exit` is the only way to stop it (launchd sends SIGTERM,
    // tokio drops the task with the runtime).
    {
        let last_finish = last_finish.clone();
        let last_prompt_sent = last_prompt_sent.clone();
        let write_half = write_half.clone();
        let sid = sid.clone();
        let bee_name = cli.bee_name.clone();
        let _namespace = namespace.clone();
        let system_prompt = system_prompt.clone();
        let prompt_tools = prompt_tools.clone();
        let variant = cli.variant.clone();
        let eoa = cli.eoa_addr.clone();
        let ctx_for_tick = ctx.clone();
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
                // Skip if prior prompt has not finished yet.
                if lf <= lps {
                    tracing::info!(skip = "prior-in-flight", "tick skipped");
                    continue;
                }
                // Honor cooldown between finish and next tick to avoid hot-loop.
                if now.saturating_sub(lf) < cooldown_ms {
                    continue;
                }
                counter = counter.saturating_add(1);
                let tick_text = compose_prompt_body(
                    &ctx_for_tick,
                    tick_directive(role, &variant, &bee_name, &eoa, counter),
                )
                .await;
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
                let tool_name = call.tool_name.clone();
                let result = tool.invoke(call).await;
                tracing::info!(ok = result.ok, "tool-result");
                let bust = !result.ok && error_looks_like_bust(&result.error);
                emit_result(&write_half, &result, &sid).await?;

                if bust {
                    tracing::warn!(
                        tool = %tool_name,
                        sid = %sid,
                        "bee.bust.detected"
                    );
                    let body = compose_bust_recovery_prompt(&ctx, &namespace, &tool_name).await;
                    let recovery = json!({
                        "chi": "prompt",
                        "sid": &sid,
                        "from": &cli.bee_name,
                        "modelId": "claude-opus-4-7",
                        "systemPrompt": system_prompt,
                        "text": body,
                        "tools": prompt_tools,
                        "disallowedTools": CLAUDE_BUILT_IN_BLOCKLIST,
                    });
                    if let Err(e) = write_line(&write_half, &recovery).await {
                        warn!(error = %e, "bust-recovery prompt send failed");
                    } else {
                        last_prompt_sent.store(now_ms_u64(), std::sync::atomic::Ordering::SeqCst);
                        tracing::info!(sid = %sid, "bust-recovery prompt emitted");
                    }
                }
            }
            "finish" => {
                let usage = frame.get("usage").cloned().unwrap_or(Value::Null);
                let out_tokens = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                last_finish.store(now_ms_u64(), std::sync::atomic::Ordering::SeqCst);
                tracing::info!(out_tokens, "finish");
            }
            "chunk" => {
                // One-shot capture: log the first text_delta chunk per turn so the
                // operator can see what the model is actually producing without
                // having to attach a side-channel. Suppresses subsequent chunks
                // so the log stays scannable.
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
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
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

fn bootstrap_directive(role: Role, variant: &str, bee_name: &str) -> String {
    let _ = bee_name;
    match role {
        Role::Leader => format!(
            "First boot. Goal: ensure your EOA is registered as a leader on DamanCopyBond, then idle.\n\n\
             Current state is at the top of this prompt; use it instead of re-reading.\n\
             1. If the state shows you are not yet registered as leader (tier=None, active=false), call register_leader. Use tier 0 (retail) and a claimedAum in USDC base units (6 decimals) consistent with your variant ({variant}); for steady-within-universe variants, around 10000 USDC = 10000000000 base units is appropriate.\n\
             2. If the state shows you are already registered, skip register_leader and just note your state.\n\
             3. Do not record a trade in this bootstrap turn; the tick will re-prompt you for that."
        ),
        Role::Follower => format!(
            "First boot. Goal: discover an active leader matching your variant ({variant}) and subscribe with a small starter position.\n\n\
             Current state is at the top of this prompt.\n\
             1. Call read_reputation against a few candidate leader addresses; you do not have a peer list inline, so make reasonable guesses or call read_leader_state on any address you've seen on the mesh.\n\
             2. If no leader is registered yet, the mesh just spawned; idle this turn and wait for the next tick.\n\
             3. If a leader exists, call subscribe with capital 1000000 (1 USDC in base units) to take a starter position. Confirm your USDC balance (in the state block) covers capital + gas pre-deduction.\n\
             4. Stop after one subscription this turn."
        ),
        Role::Watchdog => "First boot. Goal: open your event stream and survey the current state.\n\n\
             Current state is at the top of this prompt.\n\
             1. Call subscribe_to_role_events with role='watchdog' to declare your interest.\n\
             2. Call read_active_claims to inspect what is currently pending.\n\
             3. Idle for this bootstrap turn; the tick will re-prompt you to act on what you observe.".to_string(),
        Role::Arbiter => "First boot. Goal: open your event stream and survey pending claims.\n\n\
             Current state is at the top of this prompt.\n\
             1. Call subscribe_to_role_events with role='arbiter'.\n\
             2. Call read_active_claims.\n\
             3. If a clear-cut claim is already pending, rule on it via rule_claim. Otherwise idle; the tick will re-prompt you when claims accumulate.".to_string(),
        Role::Relief => "First boot. Goal: open the credit-mutual-aid inbox loop.\n\n\
             Current state is at the top of this prompt.\n\
             1. Call subscribe_to_role_events with role='relief' for observability.\n\
             2. Call read_credit_inbox to check whether any signed loan requests are already pending.\n\
             3. If the inbox is empty, idle. The tick will re-prompt you to poll the inbox; you only act when there is something to act on.".to_string(),
    }
}

fn tick_directive(role: Role, variant: &str, bee_name: &str, eoa: &str, tick: u32) -> String {
    let _ = bee_name;
    let _ = eoa;
    let asset_for_variant = pick_asset_for_tick(role, variant, tick);
    let side_for_variant = pick_side_for_tick(variant, tick);
    match role {
        Role::Leader => format!(
            "Tick {tick}. Sustain on-chain activity consistent with your variant ({variant}).\n\n\
             Your current state is at the top of this prompt. Use the USDC balance to confirm you have gas headroom before sending a tx.\n\
             1. If the state shows you are not active as a leader, call register_leader first (tier 0, claimedAum ~10000000000).\n\
             2. Record one trade via record_trade. Use asset='{asset_for_variant}' (in the HLAL_2026Q2 universe) and side='{side_for_variant}'. Pick size and leverage from your variant. For steady variants use size around 500000 (0.5 USDC) with leverage 2; for risk-on use size around 2000000 with leverage 5; for the 'echo' rogue-capable variant you may occasionally trade outside the universe or above tier-cap leverage to test the substrate.\n\
             3. After the trade, stop. The next tick will arrive in ~75 seconds."
        ),
        Role::Follower => format!(
            "Tick {tick}. Sustain on-chain activity.\n\n\
             Your current state is at the top of this prompt.\n\
             1. Call read_reputation on one or two leaders you've seen recently.\n\
             2. If your active subscription's leader has gained reputation, hold. If they've lost, unsubscribe and resubscribe to a higher-reputation leader with capital 1000000.\n\
             3. If you have no subscription yet, subscribe to the highest-reputation leader you can find.\n\
             4. One write per tick is plenty; don't churn."
        ),
        Role::Watchdog => format!(
            "Tick {tick}. Look for violations.\n\n\
             Your current state is at the top of this prompt.\n\
             1. Call read_active_claims to see what's already pending.\n\
             2. If you have grounds to file a new claim (e.g. you observed a leader trading outside the HLAL_2026Q2 universe), call file_claim with the leader address and a short evidence string referencing the offending tx.\n\
             3. If a claim you filed has been upheld and not yet collected, call claim_bounty.\n\
             4. If nothing actionable, idle this tick."
        ),
        Role::Arbiter => format!(
            "Tick {tick}. Rule on what's pending.\n\n\
             Your current state is at the top of this prompt.\n\
             1. Call read_active_claims.\n\
             2. For each clearly-decidable claim, call rule_claim with your verdict. Uphold when the evidence shows a universe violation or tier-cap breach; reject when the claim is malformed or evidence is thin.\n\
             3. If no claims are pending, idle."
        ),
        Role::Relief => format!(
            "Tick {tick}. Check the credit-mutual-aid inbox via read_credit_inbox.\n\n\
             Your current state (including treasury available) is at the top of this prompt.\n\
             For each pending entry returned:\n\
             1. Validate the request: deadline (unix seconds) must be > now, amount must be <= treasury available, the entry must not be obviously malformed.\n\
             2. Call request_loan_with_signature with that entry's borrower / amount / nonce / deadline / signature. This deducts gas from YOUR USDC balance to relay; you are the relay.\n\
             3. On a successful receipt, call mark_credit_processed with the filename and the resulting tx_hash so the inbox does not re-surface the entry next tick.\n\
             If the inbox is empty, idle. Do not initiate trades, bonds, or claims; you only serve the relay."
        ),
    }
}

/// Pick a HLAL_2026Q2 ticker for this tick. Cycles through the universe so successive
/// ticks don't all land on the same asset; rogue-capable 'echo' occasionally picks an
/// out-of-universe ticker to stress the substrate.
fn pick_asset_for_tick(role: Role, variant: &str, tick: u32) -> &'static str {
    const UNIVERSE: &[&str] = &[
        "HLAL-AAPL", "HLAL-MSFT", "HLAL-NVDA", "HLAL-GOOGL", "HLAL-JNJ",
        "HLAL-XOM", "HLAL-TSLA", "HLAL-ABBV", "HLAL-LLY", "HLAL-PG",
    ];
    const OUT_OF_UNIVERSE: &[&str] = &["OUT-XYZ", "OUT-LEND", "OUT-CASINO"];
    if matches!(role, Role::Leader) && variant == "echo" && tick % 4 == 0 {
        OUT_OF_UNIVERSE[(tick as usize / 4) % OUT_OF_UNIVERSE.len()]
    } else {
        UNIVERSE[tick as usize % UNIVERSE.len()]
    }
}

fn pick_side_for_tick(variant: &str, tick: u32) -> &'static str {
    // Variants with directional bias get pinned; others alternate per tick.
    match variant {
        "delta" => "long",
        _ if tick % 2 == 0 => "long",
        _ => "short",
    }
}

fn now_ms_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Prefix the live financial-state block to a directive body. Called for
/// every bootstrap / tick / bust-recovery prompt so claude opens every turn
/// with up-to-date balance, bond, debt, reputation, and treasury numbers.
async fn compose_prompt_body(ctx: &DamanCtx, directive: String) -> String {
    let state = fetch_bee_state(ctx).await;
    let block = render_state_block(&state);
    format!("{block}\n{directive}")
}

/// Substring scan against the structured ForagerError. We Display the error
/// so both ABI-validation errors (which carry the revert reason in `reason`)
/// and send/receipt errors match the same lowercase predicate.
fn error_looks_like_bust(err: &Option<reverb_arc_fs::ForagerError>) -> bool {
    let Some(e) = err else { return false };
    let s = e.to_string().to_ascii_lowercase();
    s.contains("exceeds balance") || s.contains("insufficient")
}

/// Build the bust-recovery follow-up prompt. Carries the live state block at
/// the top (so claude sees current debt + treasury), the tool name that
/// reverted (so it can frame its next move), and the 4-step recovery loop.
async fn compose_bust_recovery_prompt(
    ctx: &DamanCtx,
    namespace: &str,
    failing_tool: &str,
) -> String {
    let state = fetch_bee_state(ctx).await;
    let block = render_state_block(&state);
    let ns = namespace;
    let directive = format!(
        "Bust-recovery prompt. Your last call to `{failing_tool}` reverted because your USDC \
         balance cannot cover the next tx's gas-budget pre-deduction (Arc uses USDC as native \
         gas; `with_recommended_fillers().wallet(...)` pre-deducts `gas_limit * \
         max_fee_per_gas` before the tx executes).\n\
         \n\
         Recovery via the credit-mutual-aid mesh, in 4 steps:\n\
         1. Read benevolence.nonceOf(borrower) implicitly via the state above (if you do not \
            have it, the next attempt to sign will surface the right value via the chain). \
            Call `{ns}_sign_loan_request` with: amount in USDC base units (try 2000000 = 2 \
            USDC for a small gas top-up; do not exceed the treasury available shown above), \
            a deadline 1 hour out (current unix seconds + 3600), and a short `reason` like \
            'gas top-up after {failing_tool} revert'. This tool BOTH signs the EIP-712 \
            request locally (no gas) AND publishes the signed payload to the relief inbox \
            in the same call.\n\
         2. Wait. A relief peer polls the inbox each tick and will submit on your behalf via \
            `{ns}_request_loan_with_signature`. The borrowed USDC arrives in your EOA from the \
            benevolence treasury.\n\
         3. Once the state block on the next tick shows your USDC balance restored, resume your \
            normal duties.\n\
         4. When you have earned back enough, call `{ns}_repay` to clear the debt; that frees \
            treasury headroom for the next bust peer.\n\
         \n\
         Constraints from the current state:\n\
         - Treasury available for new loans: {treasury}. Your loan amount must be <= this.\n\
         - Current debt: {debt}. The per-borrower cap on benevolence is 5_000_000 (5 USDC); \
           ensure debt + new amount <= cap.\n\
         - If your balance is already restored (e.g. yield arrived), skip the loan and just \
           retry the original call.",
        ns = ns,
        failing_tool = failing_tool,
        treasury = state.treasury_pretty,
        debt = state.debt_pretty,
    );
    format!("{block}\n{directive}")
}
