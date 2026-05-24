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
        "chis": ["hello", "echo", "log", "perf-mark", "prompt", "chunk", "tool-call", "tool-result", "finish", "gossip-subscribe"],
        "source": "https://github.com/damanfi/agents/tree/main/daman-personas",
    });
    write_line(&write_half, &hello).await?;

    // Subscribe to gossip topics.
    for topic in persona.subscribe_topics() {
        let sub = json!({ "chi": "gossip-subscribe", "topic": topic });
        write_line(&write_half, &sub).await?;
    }

    // Chain-event subscription is handled via a separate forager in the operating model;
    // declare intent here for the runtime to pick up (this persona binary leaves the
    // chain-stream attachment to humd's chain-reader forager + bridge).
    for filter in persona.subscribe_chain_events() {
        let req = json!({
            "chi": "gossip-publish",
            "topic": "daman/chain-stream/request",
            "payload": {
                "chi": "subscribe-chain-event",
                "args": {
                    "contract": filter.contract,
                    "event_signature": filter.event_signature,
                    "from_block": filter.from_block,
                    "subscriber": persona.bee_name(),
                }
            }
        });
        write_line(&write_half, &req).await?;
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
                "systemPrompt": system_prompt,
                "userPrompt": user_prompt,
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
