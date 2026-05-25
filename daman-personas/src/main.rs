//! daman-persona binary entry point.
//!
//! Per-process model: each persona instance runs as its own OS process. The launcher
//! script `scripts/launch-swarm.sh` spawns N of these with role + variant + bee-name +
//! sid env. The binary opens a Unix-socket connection to the local humd, emits the
//! `chi:"hello"` manifest for this persona, subscribes to its gossip topics + chain
//! event filters, and starts the asker loop.
//!
//! The asker loop body lives in `persona-base::AskerLoop`. This binary provides the
//! concrete `Transport` impl that wires it against humd.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use daman_personas::{personas::PersonaConfig, personas, variant::Role};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{info, warn};

/// Claude Code's built-in tool surface that this swarm doesn't want claude to invoke.
/// Per hum maintainers: humd only auto-removes built-ins that map to a capability a
/// forager `provides` for; only `fs` is mapped today, so everything else rides through
/// on top of our 17 daman_* tools unless we explicitly disallow per-turn.
///
/// disallowedTools is per-turn, not sticky on the session, so every chi:"prompt" tone
/// must carry it.
///
/// The first block is the maintainer-suggested set. The second block captures every
/// additional Claude Code built-in observed in a session-ready tools listing that has
/// no role in the swarm: cron scheduling, worktree management, push notifications,
/// the internal Task todo list, etc. Anything left out of this list rides through.
const CLAUDE_BUILT_IN_BLOCKLIST: &[&str] = &[
    // shell
    "Bash", "BashOutput", "KillShell",
    // filesystem
    "Read", "Edit", "Write", "MultiEdit", "NotebookEdit",
    // search
    "Glob", "Grep",
    // network
    "WebFetch", "WebSearch",
    // task / planning / chat
    "Task", "TodoWrite", "AskUserQuestion", "ExitPlanMode", "SlashCommand",
    // observed-but-irrelevant: cron + scheduling
    "CronCreate", "CronDelete", "CronList", "ScheduleWakeup",
    // worktree management
    "EnterPlanMode", "EnterWorktree", "ExitWorktree",
    // notifications / monitors / skills
    "Monitor", "PushNotification", "Skill",
    // internal Task todo list
    "TaskCreate", "TaskGet", "TaskList", "TaskOutput", "TaskUpdate", "TaskStop",
    // tool-search meta-tool (already shouldn't appear, but defensive)
    "ToolSearch",
    // remote trigger / share / cli setup helpers
    "RemoteTrigger", "ShareOnboardingGuide",
];

fn disallowed_tools_value() -> Value {
    Value::Array(
        CLAUDE_BUILT_IN_BLOCKLIST
            .iter()
            .map(|s| Value::String((*s).to_string()))
            .collect(),
    )
}

#[derive(Parser, Debug)]
#[command(name = "daman-persona", about = "Daman persona bee runtime")]
struct Cli {
    /// Role: leader | follower | watchdog | arbiter | relief
    #[arg(long, env = "DAMAN_PERSONA_ROLE")]
    role: String,

    /// Variant identifier per role (e.g. alpha, bravo, v1, v2).
    #[arg(long, env = "DAMAN_PERSONA_VARIANT", default_value = "alpha")]
    variant: String,

    /// Persona bee name. Must be unique across the swarm and present in daman-arc-fs's
    /// keyring.
    #[arg(long, env = "DAMAN_PERSONA_BEE_NAME")]
    bee_name: String,

    /// EOA address bound to this persona. Used in the persona's system prompt + tool args.
    #[arg(long, env = "DAMAN_PERSONA_EOA_ADDR")]
    eoa_addr: String,

    /// Session id this persona uses for its claude-cli conversation. Defaults to the
    /// bee_name with a `sid-` prefix.
    #[arg(long, env = "DAMAN_PERSONA_SID")]
    sid: Option<String>,

    /// humd socket path. Defaults to env / XDG runtime / /run/user/<uid>.
    #[arg(long, env = "HUM_THRUM_SOCK")]
    sock_path: Option<String>,

    /// Log directory for the persona's chi traffic.
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
    let cfg = PersonaConfig::new(cli.bee_name.clone(), cli.variant.clone(), cli.eoa_addr.clone(), sid.clone());

    let persona = personas::build(role, cfg);
    let sock_path = cli.sock_path.unwrap_or_else(sock_path_default);
    info!(
        bee = %persona.bee_name(),
        role = %cli.role,
        variant = %cli.variant,
        sid = %sid,
        sock = %sock_path,
        "persona starting"
    );

    let stream = UnixStream::connect(&sock_path)
        .await
        .with_context(|| format!("connect humd at {sock_path}"))?;
    let (read_half, write_half) = stream.into_split();
    let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

    // Emit hello on behalf of this persona.
    let hello = json!({
        "chi": "hello",
        "bee": persona.bee_name(),
        "version": daman_personas::version(),
        "protoVersion": "0.7.0",
        "propensity": {
            "statefulness": "stateful",
            "richness": "rich",
            "wire": format!("daman/persona/{}", cli.role),
        },
        // Declare the chi vocabulary this persona speaks. Subscriptions are implicit:
        // humd routes incoming tones by chi value to bees whose hello listed that chi.
        // We list inbound daman chis the persona reacts to plus the standard nestler set.
        "chis": [
            "hello", "echo", "log", "perf-mark",
            "prompt", "chunk", "tool-call", "tool-result", "tool-meta", "finish", "error",
            // daman chis the persona reacts to
            "trade-claim", "slash-claim", "ruling", "bounty-claimed",
            "credit-need", "credit-signed-request", "credit-relayed", "credit-error",
            "loan-requested", "loan-repaid", "loan-blocked"
        ],
        "source": "https://github.com/damanfi/agents/tree/main/daman-personas",
    });
    write_line(&write_half, &hello).await?;

    // Subscribe to gossip topics. Topic subscription is implicit via the chis declaration
    // in the hello manifest above; humd routes incoming tones by chi value to bees whose
    // hello listed that chi. There is no separate `gossip-subscribe` wire frame.
    let _topics = persona.subscribe_topics();
    let _chain_events = persona.subscribe_chain_events();

    // Bootstrap tick. Personas are event-driven, but on startup they have nothing to
    // react to. Emit one synthetic event so the worker gets its first prompt and the
    // persona's first decision goes on chain. The synthetic event carries
    // `kind: bootstrap` so the worker knows this is the session-open prompt.
    if let Some(sys_prompt) = persona.persona_system_prompt() {
        // Per-role bootstrap directive. Tools are surfaced to claude via MCP as
        // `mcp__hum__daman_*`. The directive must be imperative or claude tends to
        // finish without calling anything.
        let role_directive = match role {
            Role::Leader => "Your first action: call mcp__hum__daman_register_leader with args {tier: 0, claimedAum: \"10000000000000000000000\", as_bee: \"<your bee_name>\"} to register as a retail-tier leader claiming 10000 USDC AUM. Do not explain; call the tool now.",
            Role::Follower => "Your first action: call mcp__hum__daman_read_reputation for a few candidate leader addresses (you have none yet; query 0x15f8A419eEd9Dc1e21C6bb86B06be979ad80De29 as a starting probe). After you see at least one valid leader, call mcp__hum__daman_subscribe with that leader and a capital of 1000000 USDC.",
            Role::Watchdog => "Your first action: call mcp__hum__daman_subscribe_to_role_events with args {role: \"watchdog\", as_bee: \"<your bee_name>\"} to open the event stream. Then idle until a degradation candidate appears.",
            Role::Arbiter => "Your first action: call mcp__hum__daman_subscribe_to_role_events with args {role: \"arbiter\", as_bee: \"<your bee_name>\"} to open the event stream. Then idle until a dispute lands.",
            Role::Relief => "Your first action: call mcp__hum__daman_subscribe_to_role_events with args {role: \"relief\", as_bee: \"<your bee_name>\"} to open the relief stream. Then idle.",
        };
        let user_text = format!(
            "Bootstrap tick. {directive}\n\nWhen calling tools, set the `as_bee` arg to your bee_name (your identity in this session).",
            directive = role_directive
        );
        let bootstrap = json!({
            "chi": "prompt",
            "sid": &sid,
            "from": persona.bee_name(),
            "modelId": "claude-opus-4-7",
            "systemPrompt": sys_prompt,
            "text": user_text,
            "disallowedTools": disallowed_tools_value(),
        });
        write_line(&write_half, &bootstrap).await?;
        tracing::info!(sid = %sid, "bootstrap prompt emitted");
    }

    // Tick loop. Asker behavior is event-driven: each inbound gossip / chain-event tone
    // produces an Event, which we hand to persona.on_event. On Decision::Prompt, we emit
    // chi:prompt on the persona's sid; humd routes to the claude-cli worker's cell which
    // observes via its bound sid. The persona then logs the bloom (chunks, tool-calls,
    // finish) as they arrive on its inbound stream.
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
        match chi {
            "gossip-event" | "gossip-deliver" => {
                let topic = frame.get("topic").and_then(|t| t.as_str()).unwrap_or("");
                let body = frame.get("payload").cloned().unwrap_or(Value::Null);
                let event = persona_base::persona::Event::Gossip {
                    topic: topic.into(),
                    body,
                };
                handle_event(&persona, &write_half, event, &sid).await?;
            }
            "chain-event" => {
                let contract = frame.get("contract").and_then(|c| c.as_str()).unwrap_or("");
                let event_name = frame.get("event").and_then(|c| c.as_str()).unwrap_or("");
                let data = frame.get("data").cloned().unwrap_or(Value::Null);
                let event = persona_base::persona::Event::ChainEvent {
                    contract: contract.into(),
                    event: event_name.into(),
                    data,
                };
                handle_event(&persona, &write_half, event, &sid).await?;
            }
            // Forward chunks / tool-calls / tool-results / finishes from the worker to the
            // persona's log. The persona doesn't act on them directly; the forager (daman-
            // arc-fs) handles tool-calls and emits tool-results back to humd.
            "chunk" | "tool-call" | "tool-result" | "finish" | "error" => {
                tracing::debug!(chi, "observation");
            }
            _ => {
                tracing::trace!(chi, "ignored frame");
            }
        }
    }
    Ok(())
}

async fn handle_event(
    persona: &Box<dyn persona_base::persona::PersonaBee>,
    write_half: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    event: persona_base::persona::Event,
    _sid: &str,
) -> Result<()> {
    use persona_base::persona::Decision;
    match persona.on_event(event).await {
        Decision::Skip { reason } => {
            tracing::debug!(reason, "persona skipped");
        }
        Decision::Prompt { sid, system_prompt, user_prompt } => {
            let prompt = json!({
                "chi": "prompt",
                "sid": sid,
                "from": persona.bee_name(),
                "modelId": "claude-opus-4-7",
                "systemPrompt": system_prompt,
                "text": user_prompt,
                "disallowedTools": disallowed_tools_value(),
            });
            write_line(write_half, &prompt).await?;
        }
    }
    Ok(())
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
