use clap::{Parser, Subcommand, ValueEnum};
use clawchat_client::{ClawChatClient, ClientError};
use clawchat_core::{ChatMessage, ErrorCode, FrameType};
use std::io::Write;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};

/// `metadata.kind` marking the last message of a conversation. `send --end` sets
/// it; `wait` exits 3 on receiving one so a reply-then-wait loop terminates.
const KIND_CONVERSATION_END: &str = "conversation_end";

fn render_export(
    messages: &[ChatMessage],
    format: ExportFormat,
    include_thinking: bool,
    room_label: &str,
) -> String {
    let mut out = String::new();
    let filtered: Vec<&ChatMessage> = messages
        .iter()
        .filter(|m| {
            let kind = m.metadata.get("type").and_then(|v| v.as_str());
            if kind == Some("system") {
                return false; // never include system rows in exports
            }
            if !include_thinking && kind == Some("thinking") {
                return false;
            }
            true
        })
        .collect();

    match format {
        ExportFormat::Md => {
            out.push_str(&format!("# Room: {}\n\n", room_label));
            if let (Some(first), Some(last)) = (filtered.first(), filtered.last()) {
                out.push_str(&format!(
                    "_{} entries from seq {} to {} ({} → {})_\n\n",
                    filtered.len(),
                    first.seq,
                    last.seq,
                    first.timestamp.format("%Y-%m-%d %H:%M:%S UTC"),
                    last.timestamp.format("%Y-%m-%d %H:%M:%S UTC"),
                ));
            }
            for m in &filtered {
                let kind = m.metadata.get("type").and_then(|v| v.as_str());
                let header = match kind {
                    Some("thinking") => format!(
                        "### 💭 {} · seq {} · {}",
                        m.agent_name,
                        m.seq,
                        m.timestamp.format("%H:%M:%S")
                    ),
                    _ => format!(
                        "### {} · seq {} · {}",
                        m.agent_name,
                        m.seq,
                        m.timestamp.format("%H:%M:%S")
                    ),
                };
                out.push_str(&header);
                out.push_str("\n\n");
                out.push_str(&m.content);
                if !m.content.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
        }
        ExportFormat::Json => {
            for m in &filtered {
                out.push_str(&serde_json::to_string(m).unwrap_or_default());
                out.push('\n');
            }
        }
        ExportFormat::Txt => {
            for m in &filtered {
                let kind = m.metadata.get("type").and_then(|v| v.as_str());
                let tag = if kind == Some("thinking") {
                    "(thinking) "
                } else {
                    ""
                };
                out.push_str(&format!(
                    "[{}] #{} {}{}: {}\n",
                    m.timestamp.format("%H:%M:%S"),
                    m.seq,
                    tag,
                    m.agent_name,
                    m.content
                ));
                out.push('\n');
            }
        }
    }
    out
}

fn format_message(msg: &ChatMessage) -> String {
    let ts = msg.timestamp.format("%H:%M:%S");
    let is_system = msg.metadata.get("type").and_then(|v| v.as_str()) == Some("system");
    if is_system {
        format!("[{}] #{} * {} {} *", ts, msg.seq, msg.agent_name, msg.content)
    } else {
        format!("[{}] #{} {}: {}", ts, msg.seq, msg.agent_name, msg.content)
    }
}

#[derive(Parser)]
#[command(
    name = "clawchat",
    version,
    about = "ClawChat - Agent-to-agent chat infrastructure"
)]
struct Cli {
    /// Unix socket path to connect to
    #[arg(long, global = true, default_value = default_socket_path())]
    socket: PathBuf,

    /// Use TCP instead of Unix socket
    #[arg(long, global = true)]
    tcp: Option<String>,

    /// Connect over WebSocket to a remote server, e.g.
    /// wss://chat.clawchat.live/ws. Takes precedence over --tcp/--socket.
    #[arg(long, global = true)]
    url: Option<String>,

    /// API key (reads from ~/.clawchat/auth.key if not provided)
    #[arg(long, global = true)]
    key: Option<String>,

    /// Pre-shared key for end-to-end encrypted rooms. Overrides the
    /// CLAWCHAT_ROOM_KEY environment variable. When set, message content is
    /// encrypted before send and decrypted after receive, keyed per-room.
    #[arg(long, global = true)]
    room_key: Option<String>,

    /// Agent name for this CLI session
    #[arg(long, global = true, default_value = "cli")]
    name: String,

    /// Stable agent id for this session. Pass a consistent value across calls so
    /// the server treats your separate send/wait invocations as ONE agent —
    /// required for leader election and reconnect to work across one-shot calls.
    #[arg(long, global = true)]
    agent_id: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Send a message to a room
    Send {
        /// Room ID or name
        room: String,
        /// Message content
        message: String,
        /// Reply to a specific message ID
        #[arg(long)]
        reply_to: Option<String>,
        /// Tag the message with a `kind` (stored in `metadata.kind`). Free-form,
        /// but conventions: `review_request`, `verdict`, `checkpoint`, `fyi`.
        /// Peers can filter on these via `wait --only-kind` / `history --kind`.
        #[arg(long)]
        kind: Option<String>,
        /// End the conversation: tags the message `kind=conversation_end`. A peer
        /// running `wait` surfaces this message, then exits 3 so its reply-then-wait
        /// loop terminates cleanly instead of blocking for another turn.
        #[arg(long, conflicts_with = "kind")]
        end: bool,
    },

    /// Post a "thinking out loud" pulse to a room (persisted to history,
    /// broadcast as a `thinking` event, does NOT advance the turn token,
    /// does NOT wake peers' `wait`).
    Thinking {
        /// Room ID or name
        room: String,
        /// Thought / status content (keep it short)
        content: String,
    },

    /// Room management
    Rooms {
        #[command(subcommand)]
        action: RoomAction,
    },

    /// List connected agents
    Agents {
        /// Filter by room ID
        #[arg(long)]
        room: Option<String>,
    },

    /// View message history
    History {
        /// Room ID
        room: String,
        /// Number of messages
        #[arg(long, default_value = "50")]
        limit: u32,
        /// Stream new messages (like tail -f)
        #[arg(long)]
        follow: bool,
        /// Only return messages after this message ID
        #[arg(long)]
        since: Option<String>,
        /// Only return messages with seq strictly greater than this value
        #[arg(long)]
        since_seq: Option<i64>,
        /// Only return messages tagged with this `metadata.kind`
        /// (e.g. `--kind verdict`).
        #[arg(long)]
        kind: Option<String>,
        /// Write to file instead of stdout
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },

    /// Wait for a new message in a room (blocks until one arrives)
    Wait {
        /// Room ID or name
        room: String,
        /// Timeout in seconds (0 = wait forever). In --loop mode this is the
        /// per-iteration budget; outer wall-clock is unbounded.
        #[arg(long, default_value = "60")]
        timeout: u64,
        /// Output as JSON. Default. Pass --text for human-readable output.
        #[arg(long, conflicts_with = "text")]
        json: bool,
        /// Output as human-readable text instead of JSON.
        #[arg(long)]
        text: bool,
        /// Also catch up: return the oldest chat message with seq > this value
        /// if one already exists in history, else block for a new message.
        /// Use this to safely resume after a prior wait — pass the seq of the
        /// last message you saw. Accepts an integer, or `tip`/`auto` to resolve
        /// to the room's current tip on start.
        #[arg(long)]
        since_seq: Option<String>,
        /// Stay in wait indefinitely: re-poll on internal timeout (tracking the
        /// bookmark) until a real chat message arrives. Pairs with --since-seq
        /// so messages that land between iterations are never missed. With this
        /// flag the single CLI invocation replaces the "re-run wait on timeout"
        /// discipline — the agent makes one call and gets one message back.
        #[arg(long = "loop")]
        loop_: bool,
        /// Bound the total wall-clock of a `--loop` wait (seconds). On expiry the
        /// command exits 2 (distinct from 0=message, 1=error) and prints the seq to
        /// resume from, so a stalled turn returns control instead of hanging forever.
        /// 0 = unbounded (default). Without `--loop`, `--timeout` already bounds the
        /// single wait, so this has no effect there.
        #[arg(long, default_value = "0")]
        idle_timeout: u64,
        /// Seconds between liveness heartbeats printed to stderr while blocked.
        /// Tool wrappers that kill silent processes see this and let the wait
        /// continue. 0 disables. Default 30.
        #[arg(long, default_value = "30")]
        heartbeat_secs: u64,
        /// Only wake on messages from this `--name` (peer filter).
        #[arg(long)]
        only_from: Option<String>,
        /// Skip messages from this `--name` (in addition to your own).
        #[arg(long)]
        not_from: Option<String>,
        /// Only wake on messages tagged with this `metadata.kind`.
        /// E.g. `--only-kind review_request`.
        #[arg(long)]
        only_kind: Option<String>,
        /// Print peer `thinking` pulses to stderr while blocked, for live
        /// visibility during long runs. Does NOT wake the wait — it still only
        /// returns on a real chat message, preserving turn-taking. Content is
        /// decrypted if a room key is set.
        #[arg(long)]
        show_thinking: bool,
        /// Write result to file instead of stdout. Useful when tool wrappers
        /// truncate large stdout payloads.
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },

    /// Monitor events in real-time
    Monitor {
        /// Filter to a specific room
        #[arg(long)]
        room: Option<String>,
        /// Output raw JSON frames
        #[arg(long)]
        json: bool,
    },

    /// Interactive persistent session for room coordination
    Shell {
        /// Optional room ID or name to join on start
        #[arg(long)]
        room: Option<String>,
    },

    /// Show server status
    Status,

    /// Generate a random end-to-end room key (for CLAWCHAT_ROOM_KEY). Print it
    /// once, then set the SAME value on every agent in the group so they can
    /// read each other's encrypted messages. No server connection needed.
    Keygen,

    /// Webhook subscriptions — register an HTTP endpoint to be POSTed when
    /// matching messages land in a room. Lets external automations react to
    /// events without holding a long-running `wait --loop` open.
    Sub {
        #[command(subcommand)]
        action: SubAction,
    },

    /// Export a room's history as markdown (or json/text).
    Export {
        /// Room ID or exact name
        room: String,
        /// Output format
        #[arg(long, default_value = "md")]
        format: ExportFormat,
        /// Only include messages with seq > this value
        #[arg(long)]
        since_seq: Option<i64>,
        /// Maximum messages to include (default: all)
        #[arg(long)]
        limit: Option<u32>,
        /// Include `thinking` pulses in the export. Off by default — most
        /// archives want only the chat narrative.
        #[arg(long)]
        include_thinking: bool,
        /// Write to file instead of stdout
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },

    /// Voting commands
    Vote {
        #[command(subcommand)]
        action: VoteAction,
    },

    /// Leader election commands
    Election {
        #[command(subcommand)]
        action: ElectionAction,
    },

    /// Set agent presence status (idle, waiting, working)
    Presence {
        /// Status: idle, waiting, or working
        status: String,
        /// Human-readable detail, e.g. "reviewing section 3"
        #[arg(long)]
        detail: Option<String>,
        /// Progress percentage (0-100)
        #[arg(long)]
        progress: Option<u8>,
    },
}

#[derive(Subcommand)]
enum RoomAction {
    /// List all rooms
    List {
        /// Filter by parent room ID
        #[arg(long)]
        parent: Option<String>,
    },
    /// Create a new room
    Create {
        /// Room name
        name: String,
        /// Room description
        #[arg(long)]
        description: Option<String>,
        /// Parent room ID
        #[arg(long)]
        parent: Option<String>,
        /// Create as ephemeral (auto-deleted when empty)
        #[arg(long)]
        ephemeral: bool,
        /// Create as public: visible and joinable by any API key on the server.
        /// Default is private (only your key can see or resolve it by name) —
        /// use this so agents with different keys can find the room.
        #[arg(long)]
        public: bool,
        /// Create as end-to-end encrypted. Members must share a room key
        /// (--room-key or $CLAWCHAT_ROOM_KEY); the server stores only
        /// ciphertext and rejects plaintext sends to this room.
        #[arg(long)]
        encrypted: bool,
    },
    /// Get room info
    Info {
        /// Room ID
        room_id: String,
    },
    /// Get the latest seq for a room (the room's "tip")
    Tip {
        /// Room ID or exact name
        room: String,
    },
}

#[derive(Subcommand)]
enum VoteAction {
    /// Create a sealed-ballot vote in a room
    Create {
        /// Room ID
        room: String,
        /// Vote title / question
        title: String,
        /// Vote options (at least 2)
        #[arg(long, num_args = 2.., required = true)]
        options: Vec<String>,
        /// Optional description
        #[arg(long)]
        description: Option<String>,
        /// Deadline in seconds
        #[arg(long)]
        duration: Option<u64>,
    },
    /// Cast a ballot on an active vote
    Cast {
        /// Vote ID
        vote_id: String,
        /// Option index (0-based)
        option: usize,
    },
    /// Check status of a vote
    Status {
        /// Vote ID
        vote_id: String,
    },
    /// List recent votes in a room
    History {
        /// Room ID or exact room name
        room: String,
        /// Maximum number of votes to return
        #[arg(long, default_value = "20")]
        limit: u32,
    },
}

#[derive(Subcommand)]
enum ElectionAction {
    /// Start a leader election in a room
    Start {
        /// Room ID
        room: String,
    },
    /// Decline an active election
    Decline {
        /// Room ID
        room: String,
    },
    /// Issue a decision as room leader
    Decide {
        /// Room ID
        room: String,
        /// Decision content
        content: String,
    },
}

#[derive(Subcommand)]
enum SubAction {
    /// Create a new webhook subscription on a room.
    Create {
        /// Room ID or exact name
        room: String,
        /// Webhook URL (http or https). The server will POST messages here.
        #[arg(long)]
        url: String,
        /// HMAC-SHA256 shared secret used to sign each delivery (Standard
        /// Webhooks v1 signature). Keep it private — receivers verify with it.
        #[arg(long)]
        secret: String,
        /// Restrict to specific `--kind` values on the message (comma-separated).
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        /// Only deliver messages from this `--name`.
        #[arg(long)]
        only_from: Option<String>,
        /// Skip messages from this `--name`.
        #[arg(long)]
        not_from: Option<String>,
        /// Don't deliver `thinking` pulses (only real chat).
        #[arg(long)]
        exclude_thinking: bool,
        /// Start cursor. Integer, or `tip`/`auto` (default) for "only future
        /// messages", or `0` to backfill the entire room.
        #[arg(long, default_value = "tip")]
        since_seq: String,
    },
    /// List subscriptions owned by your API key.
    List {
        /// Optionally filter to one room.
        #[arg(long)]
        room: Option<String>,
    },
    /// Delete a subscription.
    Delete {
        subscription_id: String,
    },
    /// Re-enable a `failed` subscription. Replays the backlog past the cursor.
    Enable {
        subscription_id: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ExportFormat {
    /// Markdown — chat-style rendering with timestamps + agent labels.
    Md,
    /// One JSON object per message, newline-delimited.
    Json,
    /// Plain text — human-readable, no markup.
    Txt,
}

fn default_data_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".clawchat"))
        .unwrap_or_else(|| PathBuf::from(".clawchat"))
}

fn default_socket_path() -> &'static str {
    Box::leak(
        default_data_dir()
            .join("clawchat.sock")
            .to_string_lossy()
            .into_owned()
            .into_boxed_str(),
    )
}

fn default_key_path() -> PathBuf {
    default_data_dir().join("auth.key")
}

fn load_key(key_arg: &Option<String>) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(key) = key_arg {
        return Ok(key.clone());
    }
    let key_path = default_key_path();
    if key_path.exists() {
        Ok(std::fs::read_to_string(key_path)?.trim().to_string())
    } else {
        Err("No API key provided. Use --key or ensure ~/.clawchat/auth.key exists.".into())
    }
}

/// Resolve the end-to-end room secret: `--room-key` if given, else the
/// CLAWCHAT_ROOM_KEY environment variable. Returns None when neither is set
/// (the client then sends/receives plaintext).
fn resolve_room_secret(cli: &Cli) -> Option<Vec<u8>> {
    cli.room_key
        .clone()
        .or_else(|| std::env::var("CLAWCHAT_ROOM_KEY").ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.into_bytes())
}

/// Decrypt a raw frame `content` field for display. Used on pushed events
/// (monitor/shell/follow) where the client API hasn't already decrypted. Falls
/// back to the original string when there's no key or it isn't a ciphertext blob.
fn decrypt_field(secret: Option<&[u8]>, room_id: &str, content: &str) -> String {
    match secret {
        Some(s) if clawchat_core::crypto::is_ciphertext(content) => {
            clawchat_core::crypto::decrypt(s, room_id, content)
                .unwrap_or_else(|_| content.to_string())
        }
        _ => content.to_string(),
    }
}

async fn connect(cli: &Cli) -> Result<ClawChatClient, Box<dyn std::error::Error>> {
    let key = load_key(&cli.key)?;

    let agent_id = cli.agent_id.as_deref();
    let mut client = if let Some(url) = &cli.url {
        ClawChatClient::connect_ws(url, &key, &cli.name, agent_id, vec![]).await?
    } else if let Some(addr) = &cli.tcp {
        ClawChatClient::connect_tcp(addr, &key, &cli.name, agent_id, vec![]).await?
    } else {
        ClawChatClient::connect_uds(&cli.socket, &key, &cli.name, agent_id, vec![]).await?
    };
    if let Some(secret) = resolve_room_secret(cli) {
        client.set_room_secret(&secret);
    }
    Ok(client)
}

async fn resolve_room_id(
    client: &ClawChatClient,
    room: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    // Fast path: already a room ID.
    match client.room_info(room).await {
        Ok(_) => return Ok(room.to_string()),
        Err(ClientError::Server {
            code: ErrorCode::RoomNotFound,
            ..
        }) => {}
        Err(e) => return Err(Box::new(e)),
    }

    // Fallback: resolve by exact room name.
    let rooms = client.list_rooms(None).await?;
    let matches: Vec<_> = rooms.into_iter().filter(|r| r.name == room).collect();

    match matches.as_slice() {
        [single] => Ok(single.room_id.clone()),
        [] => Err(format!("Room '{room}' not found (expected ID or exact name)").into()),
        _ => Err(format!("Room name '{room}' is ambiguous; use the room ID").into()),
    }
}

fn print_shell_help() {
    println!("Interactive shell commands:");
    println!("  /help                 Show this help");
    println!("  /join <room>          Join room (id or exact name) and make it active");
    println!("  /leave [room]         Leave active room (or explicit room)");
    println!("  /room                 Show current active room");
    println!("  /rooms                List rooms");
    println!("  /agents               List agents in active room");
    println!("  /history [limit]      Show room history (default 20)");
    println!("  /send <message>       Send message to active room");
    println!("  /quit                 Exit shell");
    println!("  <text>                Shortcut for /send <text>");
}

fn print_shell_prompt(current_room: Option<&str>) {
    let room = current_room.unwrap_or("no-room");
    print!("clawchat[{room}]> ");
    let _ = std::io::stdout().flush();
}

async fn run_shell(
    cli: &Cli,
    start_room: &Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = connect(cli).await?;
    let room_secret = resolve_room_secret(cli);
    let mut current_room: Option<String> = None;

    if let Some(room_ref) = start_room {
        let room_id = resolve_room_id(&client, room_ref).await?;
        client.join_room(&room_id).await?;
        println!("Joined room: {}", room_id);
        current_room = Some(room_id);
    }

    println!(
        "Connected as '{}' (agent_id: {})",
        client.agent_name, client.agent_id
    );
    print_shell_help();

    let mut stdin_lines = BufReader::new(tokio::io::stdin()).lines();
    let mut events = client.subscribe();

    loop {
        print_shell_prompt(current_room.as_deref());

        tokio::select! {
            line = stdin_lines.next_line() => {
                let Some(line) = line? else {
                    println!();
                    break;
                };

                let input = line.trim();
                if input.is_empty() {
                    continue;
                }

                if let Some(command_text) = input.strip_prefix('/') {
                    let (cmd, rest) = match command_text.split_once(' ') {
                        Some((cmd, rest)) => (cmd.trim(), rest.trim()),
                        None => (command_text.trim(), ""),
                    };

                    match cmd {
                        "help" => print_shell_help(),
                        "join" => {
                            if rest.is_empty() {
                                println!("Usage: /join <room-id-or-name>");
                                continue;
                            }
                            match resolve_room_id(&client, rest).await {
                                Ok(room_id) => {
                                    match client.join_room(&room_id).await {
                                        Ok(_) => {
                                            println!("Joined room: {}", room_id);
                                            current_room = Some(room_id);
                                        }
                                        Err(e) => println!("Join failed: {}", e),
                                    }
                                }
                                Err(e) => println!("Join failed: {}", e),
                            }
                        }
                        "leave" => {
                            let target_room = if rest.is_empty() {
                                current_room.clone()
                            } else {
                                match resolve_room_id(&client, rest).await {
                                    Ok(id) => Some(id),
                                    Err(e) => {
                                        println!("Leave failed: {}", e);
                                        None
                                    }
                                }
                            };

                            if let Some(room_id) = target_room {
                                match client.leave_room(&room_id).await {
                                    Ok(_) => {
                                        println!("Left room: {}", room_id);
                                        if current_room.as_deref() == Some(room_id.as_str()) {
                                            current_room = None;
                                        }
                                    }
                                    Err(e) => println!("Leave failed: {}", e),
                                }
                            } else {
                                println!("No active room to leave.");
                            }
                        }
                        "room" => {
                            match current_room.as_deref() {
                                Some(room_id) => println!("Active room: {}", room_id),
                                None => println!("No active room. Use /join <room> first."),
                            }
                        }
                        "rooms" => {
                            match client.list_rooms(None).await {
                                Ok(rooms) => {
                                    if rooms.is_empty() {
                                        println!("No rooms found.");
                                    } else {
                                        println!("{:<38} {:<20} {:<10} DESCRIPTION", "ID", "NAME", "TYPE");
                                        println!("{}", "-".repeat(80));
                                        for room in rooms {
                                            let room_type = if room.ephemeral { "ephemeral" } else { "permanent" };
                                            let desc = room.description.as_deref().unwrap_or("");
                                            println!("{:<38} {:<20} {:<10} {}", room.room_id, room.name, room_type, desc);
                                        }
                                    }
                                }
                                Err(e) => println!("Failed to list rooms: {}", e),
                            }
                        }
                        "agents" => {
                            match client.list_agents(current_room.as_deref()).await {
                                Ok(agents) => {
                                    if agents.is_empty() {
                                        println!("No agents connected.");
                                    } else {
                                        println!(
                                            "{:<38} {:<16} {:<10} {:<10} {}",
                                            "AGENT ID", "NAME", "STATUS", "PROGRESS", "DETAIL"
                                        );
                                        println!("{}", "-".repeat(100));
                                        for agent in agents {
                                            let status = agent.status.as_deref().unwrap_or("-");
                                            let progress = agent
                                                .progress
                                                .map(|p| format!("{}%", p))
                                                .unwrap_or_default();
                                            let detail = agent.status_detail.as_deref().unwrap_or("");
                                            println!(
                                                "{:<38} {:<16} {:<10} {:<10} {}",
                                                agent.agent_id, agent.name, status, progress, detail
                                            );
                                        }
                                    }
                                }
                                Err(e) => println!("Failed to list agents: {}", e),
                            }
                        }
                        "history" => {
                            let limit = if rest.is_empty() {
                                20
                            } else {
                                match rest.parse::<u32>() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        println!("Usage: /history [limit]");
                                        continue;
                                    }
                                }
                            };

                            let Some(room_id) = current_room.as_deref() else {
                                println!("No active room. Use /join <room> first.");
                                continue;
                            };

                            match client.get_history(room_id, limit, None).await {
                                Ok(messages) => {
                                    for msg in messages {
                                        println!("{}", format_message(&msg));
                                    }
                                }
                                Err(e) => println!("Failed to load history: {}", e),
                            }
                        }
                        "send" => {
                            if rest.is_empty() {
                                println!("Usage: /send <message>");
                                continue;
                            }
                            if let Some(room_id) = current_room.as_deref() {
                                if let Err(e) = client.send_message(room_id, rest, None, vec![]).await {
                                    println!("Send failed: {}", e);
                                }
                            } else {
                                println!("No active room. Use /join <room> first.");
                            }
                        }
                        "quit" | "exit" => break,
                        _ => {
                            println!("Unknown command: /{} (try /help)", cmd);
                        }
                    }
                } else if let Some(room_id) = current_room.as_deref() {
                    if let Err(e) = client.send_message(room_id, input, None, vec![]).await {
                        println!("Send failed: {}", e);
                    }
                } else {
                    println!("No active room. Use /join <room> first.");
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if let Some(active_room) = current_room.as_deref() {
                            if let Some(event_room) = event.frame.payload.get("room_id").and_then(|v| v.as_str()) {
                                if event_room != active_room {
                                    continue;
                                }
                            }
                        }
                        println!();
                        print_event(&event.frame, room_secret.as_deref());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        println!("\n[warn] event stream lagged (dropped {} events)", skipped);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        println!("\n[event stream closed]");
                        break;
                    }
                }
            }
        }
    }

    println!("Goodbye.");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let cli = Cli::parse();

    match &cli.command {
        Commands::Send {
            room,
            message,
            reply_to,
            kind,
            end,
        } => {
            let client = connect(&cli).await?;
            let room_id = resolve_room_id(&client, room).await?;
            client.join_room(&room_id).await?;
            // `--end` is sugar for `--kind conversation_end` (the two conflict at
            // the arg level, so at most one is set).
            let kind = if *end {
                Some(KIND_CONVERSATION_END.to_string())
            } else {
                kind.clone()
            };
            let msg = match &kind {
                Some(k) => {
                    client
                        .send_message_with_metadata(
                            &room_id,
                            message,
                            reply_to.as_deref(),
                            vec![],
                            serde_json::json!({ "kind": k }),
                        )
                        .await?
                }
                None => {
                    client
                        .send_message(&room_id, message, reply_to.as_deref(), vec![])
                        .await?
                }
            };
            println!("{}", format_message(&msg));
        }

        Commands::Thinking { room, content } => {
            let client = connect(&cli).await?;
            let room_id = resolve_room_id(&client, room).await?;
            client.join_room(&room_id).await?;
            let msg = client.thinking(&room_id, content).await?;
            println!("{}", format_message(&msg));
        }

        Commands::Rooms { action } => {
            let client = connect(&cli).await?;
            match action {
                RoomAction::List { parent } => {
                    let rooms = client.list_rooms(parent.as_deref()).await?;
                    if rooms.is_empty() {
                        println!("No rooms found.");
                    } else {
                        println!("{:<38} {:<20} {:<10} {:<8} {:<20} DESCRIPTION", "ID", "NAME", "TYPE", "MEMBERS", "LAST ACTIVITY");
                        println!("{}", "-".repeat(130));
                        for room in rooms {
                            let room_type = if room.ephemeral {
                                "ephemeral"
                            } else {
                                "permanent"
                            };
                            let desc = room.description.as_deref().unwrap_or("");
                            let members = room.member_count.map(|c| c.to_string()).unwrap_or_else(|| "-".to_string());
                            let activity = room.last_activity
                                .map(|t| t.format("%H:%M:%S").to_string())
                                .unwrap_or_else(|| "-".to_string());
                            println!(
                                "{:<38} {:<20} {:<10} {:<8} {:<20} {}",
                                room.room_id, room.name, room_type, members, activity, desc
                            );
                        }
                    }
                }
                RoomAction::Create {
                    name,
                    description,
                    parent,
                    ephemeral,
                    public,
                    encrypted,
                } => {
                    let room = client
                        .create_room_with_options(
                            name,
                            description.as_deref(),
                            parent.as_deref(),
                            *ephemeral,
                            *public,
                            *encrypted,
                        )
                        .await?;
                    println!("Created room: {} ({})", room.name, room.room_id);
                    if room.ephemeral {
                        println!("  Type: ephemeral (auto-deleted when empty)");
                    }
                    println!(
                        "  Visibility: {}",
                        if room.visibility == "public" {
                            "public (any key can find & join)"
                        } else {
                            "private (only your key)"
                        }
                    );
                    if room.encrypted {
                        println!("  End-to-end encrypted: members need a shared room key");
                        if resolve_room_secret(&cli).is_none() {
                            println!(
                                "  Note: no room key set — pass --room-key or set $CLAWCHAT_ROOM_KEY to send/read here"
                            );
                        }
                    }
                }
                RoomAction::Info { room_id } => {
                    let resolved = resolve_room_id(&client, room_id).await?;
                    let info = client.room_info(&resolved).await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
                RoomAction::Tip { room } => {
                    let room_id = resolve_room_id(&client, room).await?;
                    let seq = client.room_tip(&room_id).await?;
                    println!("{}", seq);
                }
            }
        }

        Commands::Agents { room } => {
            let client = connect(&cli).await?;
            let agents = client.list_agents(room.as_deref()).await?;

            // If --room is set, also pull recent history so we can surface
            // agents who've been posting recently even if they're not currently
            // connected (each CLI invocation registers + disconnects, so an
            // active reviewer flickers in and out of the live agents list).
            let (last_in_room, room_id_for_history): (
                std::collections::HashMap<String, (i64, String)>,
                Option<String>,
            ) = if let Some(r) = room {
                let room_id = resolve_room_id(&client, r).await?;
                let hist = client.get_history(&room_id, 200, None).await.unwrap_or_default();
                let mut map = std::collections::HashMap::new();
                for m in hist.iter().rev() {
                    // Iterate newest-first; keep first sighting per agent_name.
                    map.entry(m.agent_name.clone()).or_insert((
                        m.seq,
                        m.timestamp.format("%H:%M:%S").to_string(),
                    ));
                }
                (map, Some(room_id))
            } else {
                (std::collections::HashMap::new(), None)
            };

            let show_room_activity = room_id_for_history.is_some();
            let live_names: std::collections::HashSet<String> =
                agents.iter().map(|a| a.name.clone()).collect();

            if agents.is_empty() && last_in_room.is_empty() {
                println!("No agents connected; no recent activity in room.");
            } else {
                if show_room_activity {
                    println!(
                        "{:<10} {:<38} {:<16} {:<10} {:<8} {:<10} {:<10} {}",
                        "STATE",
                        "AGENT ID",
                        "NAME",
                        "STATUS",
                        "PROG",
                        "ACTIVE",
                        "LAST_SEQ",
                        "DETAIL"
                    );
                    println!("{}", "-".repeat(130));
                } else {
                    println!(
                        "{:<38} {:<16} {:<10} {:<8} {:<10} {}",
                        "AGENT ID", "NAME", "STATUS", "PROG", "ACTIVE", "DETAIL"
                    );
                    println!("{}", "-".repeat(110));
                }

                // First: currently connected agents.
                for agent in &agents {
                    let status = agent.status.as_deref().unwrap_or("-");
                    let progress = agent
                        .progress
                        .map(|p| format!("{}%", p))
                        .unwrap_or_default();
                    let active = agent
                        .last_active
                        .map(|t| t.format("%H:%M:%S").to_string())
                        .unwrap_or_else(|| "-".to_string());
                    let detail = agent.status_detail.as_deref().unwrap_or("");
                    if show_room_activity {
                        let (last_seq_s, _last_ts_s) =
                            last_in_room.get(&agent.name).cloned().unwrap_or((
                                0,
                                "-".to_string(),
                            ));
                        let last_seq = if last_seq_s > 0 {
                            format!("#{}", last_seq_s)
                        } else {
                            "-".to_string()
                        };
                        println!(
                            "{:<10} {:<38} {:<16} {:<10} {:<8} {:<10} {:<10} {}",
                            "LIVE", agent.agent_id, agent.name, status, progress, active, last_seq, detail
                        );
                    } else {
                        println!(
                            "{:<38} {:<16} {:<10} {:<8} {:<10} {}",
                            agent.agent_id, agent.name, status, progress, active, detail
                        );
                    }
                }

                // Then: agents seen in recent history but NOT currently connected.
                // These are the ghosts the user actually cares about: "has codex
                // been here recently even though they just disconnected?"
                if show_room_activity {
                    for (name, (seq, ts)) in &last_in_room {
                        if live_names.contains(name) {
                            continue;
                        }
                        println!(
                            "{:<10} {:<38} {:<16} {:<10} {:<8} {:<10} {:<10} {}",
                            "RECENT",
                            "-",
                            name,
                            "-",
                            "",
                            ts,
                            format!("#{}", seq),
                            "(last seen via history)"
                        );
                    }
                }
            }
        }

        Commands::History {
            room,
            limit,
            follow,
            since,
            since_seq,
            kind,
            output,
        } => {
            let client = connect(&cli).await?;
            let room_id = resolve_room_id(&client, room).await?;

            // Show history (with optional --since / --since-seq filter)
            let messages = client
                .get_history_filtered(&room_id, *limit, None, since.as_deref(), *since_seq)
                .await?;

            // Optional kind filter (applied client-side).
            let filtered: Vec<&ChatMessage> = messages
                .iter()
                .filter(|m| match kind {
                    Some(k) => {
                        m.metadata.get("kind").and_then(|v| v.as_str()) == Some(k.as_str())
                    }
                    None => true,
                })
                .collect();

            let body: String = filtered
                .iter()
                .map(|m| format_message(m))
                .collect::<Vec<_>>()
                .join("\n");

            match output {
                Some(path) => {
                    std::fs::write(path, format!("{}\n", body))?;
                    eprintln!("wrote {} entries to {}", filtered.len(), path.display());
                }
                None => {
                    if !body.is_empty() {
                        println!("{}", body);
                    }
                }
            }

            if *follow {
                // Join the room to receive new messages
                let _ = client.join_room(&room_id).await;
                let room_secret = resolve_room_secret(&cli);
                let mut events = client.subscribe();
                println!("--- streaming new messages (Ctrl+C to stop) ---");
                while let Ok(event) = events.recv().await {
                    if event.frame.frame_type == FrameType::MessageReceived {
                        if let Some(event_room_id) =
                            event.frame.payload.get("room_id").and_then(|v| v.as_str())
                        {
                            if event_room_id == room_id {
                                let agent_name = event
                                    .frame
                                    .payload
                                    .get("agent_name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let content = event
                                    .frame
                                    .payload
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let content =
                                    decrypt_field(room_secret.as_deref(), &room_id, content);
                                let ts = event
                                    .frame
                                    .payload
                                    .get("timestamp")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                println!(
                                    "[{}] {}: {}",
                                    &ts[11..19.min(ts.len())],
                                    agent_name,
                                    content
                                );
                            }
                        }
                    }
                }
            }
        }

        Commands::Wait {
            room,
            timeout,
            json: _json,
            text,
            since_seq,
            loop_,
            idle_timeout,
            heartbeat_secs,
            only_from,
            not_from,
            only_kind,
            show_thinking,
            output,
        } => {
            let client = connect(&cli).await?;
            let room_id = resolve_room_id(&client, room).await?;
            let _ = client.join_room(&room_id).await;

            // Resolve `--since-seq tip|auto` to the room's current tip. Done
            // BEFORE the wait subscribes so we don't miss anything arriving
            // between the tip read and the subscribe (the SDK's wait subscribes
            // first, then checks history with since_seq — that closes the race).
            let resolved_since_seq: Option<i64> = match since_seq.as_deref() {
                None => None,
                Some(s) if s.eq_ignore_ascii_case("tip") || s.eq_ignore_ascii_case("auto") => {
                    Some(client.room_tip(&room_id).await?)
                }
                Some(s) => Some(s.parse::<i64>().map_err(|e| {
                    Box::<dyn std::error::Error>::from(format!(
                        "--since-seq must be an integer, 'tip', or 'auto': {}",
                        e
                    ))
                })?),
            };

            // Announce we're waiting so other agents in the room know someone is blocked.
            // Best-effort — don't fail the wait if presence broadcast fails.
            let _ = client.set_presence("waiting", None, None).await;

            let effective_timeout = if *timeout == 0 { 86400 } else { *timeout }; // 0 = 24h

            // Heartbeat task: periodically prints to stderr so tool wrappers that kill
            // silent processes see liveness. Aborted as soon as wait returns.
            let heartbeat_task = if *heartbeat_secs > 0 {
                let interval = *heartbeat_secs;
                let room_label = room_id.clone();
                let since_label = resolved_since_seq
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let mode_label = if *loop_ { "loop" } else { "once" };
                Some(tokio::spawn(async move {
                    let started = std::time::Instant::now();
                    let mut tick = tokio::time::interval(
                        std::time::Duration::from_secs(interval),
                    );
                    tick.tick().await; // skip the immediate first tick
                    loop {
                        tick.tick().await;
                        eprintln!(
                            "wait: alive {}s room={} since_seq={} mode={}",
                            started.elapsed().as_secs(),
                            room_label,
                            since_label,
                            mode_label,
                        );
                    }
                }))
            } else {
                None
            };

            // Presence-watcher task: prints peer join/leave to stderr while the
            // wait blocks, so a waiting agent (or the human) can see the other
            // side arrive and know to keep waiting instead of concluding "gone".
            // Uses its own event subscription, independent of the SDK wait loop.
            // Gated by the same quiet switch as heartbeats.
            let presence_task = if *heartbeat_secs > 0 {
                let mut events = client.subscribe();
                let room_label = room_id.clone();
                Some(tokio::spawn(async move {
                    while let Ok(evt) = events.recv().await {
                        let in_room = evt
                            .frame
                            .payload
                            .get("room_id")
                            .and_then(|v| v.as_str())
                            == Some(room_label.as_str());
                        if !in_room {
                            continue;
                        }
                        match evt.frame.frame_type {
                            FrameType::AgentJoined => {
                                let name = evt
                                    .frame
                                    .payload
                                    .get("agent")
                                    .and_then(|a| a.get("name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                eprintln!("wait: peer {} joined", name);
                            }
                            FrameType::AgentLeft => {
                                let who = evt
                                    .frame
                                    .payload
                                    .get("agent_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                eprintln!("wait: peer {} left", who);
                            }
                            _ => {}
                        }
                    }
                }))
            } else {
                None
            };

            // Thinking-watcher task: with --show-thinking, print peers' thinking
            // pulses to stderr for live visibility, WITHOUT waking the wait (it
            // still only returns on a real chat message). Own pulses are skipped;
            // content is decrypted if a room key is configured.
            let thinking_task = if *show_thinking {
                let mut events = client.subscribe();
                let room_label = room_id.clone();
                let self_name = cli.name.clone();
                let secret = resolve_room_secret(&cli);
                Some(tokio::spawn(async move {
                    while let Ok(evt) = events.recv().await {
                        if evt.frame.frame_type != FrameType::Thinking {
                            continue;
                        }
                        let p = &evt.frame.payload;
                        if p.get("room_id").and_then(|v| v.as_str())
                            != Some(room_label.as_str())
                        {
                            continue;
                        }
                        let name = p.get("agent_name").and_then(|v| v.as_str()).unwrap_or("?");
                        if name == self_name {
                            continue; // don't echo our own pulses
                        }
                        let content = p.get("content").and_then(|v| v.as_str()).unwrap_or("");
                        let content = decrypt_field(secret.as_deref(), &room_label, content);
                        eprintln!("wait: thinking {}: {}", name, content);
                    }
                }))
            } else {
                None
            };

            // Helper closure: does a candidate message match all filters?
            let matches = |msg: &ChatMessage| -> bool {
                if let Some(want) = only_from {
                    if &msg.agent_name != want {
                        return false;
                    }
                }
                if let Some(skip) = not_from {
                    if &msg.agent_name == skip {
                        return false;
                    }
                }
                if let Some(want_kind) = only_kind {
                    let got_kind = msg.metadata.get("kind").and_then(|v| v.as_str());
                    if got_kind != Some(want_kind.as_str()) {
                        return false;
                    }
                }
                true
            };

            // Inner loop: with --loop, keep advancing the bookmark until the
            // returned message passes all filters. Without --loop, do at most
            // one underlying wait call. In --loop mode the loop never returns on
            // its own until a match arrives, so an optional idle deadline races it.
            // `latest_seq` mirrors the bookmark out of the future (which the
            // select! may drop) so the idle-expiry path can print an accurate
            // resume point even after filters advanced past some messages.
            // i64::MIN is the "never advanced" sentinel (resume from `tip`).
            let latest_seq = std::sync::atomic::AtomicI64::new(
                resolved_since_seq.unwrap_or(i64::MIN),
            );
            let wait_loop = async {
                let mut cursor = resolved_since_seq;
                loop {
                    let result: Result<Option<ChatMessage>, _> = if *loop_ {
                        client
                            .wait_for_message_loop(&room_id, effective_timeout, cursor)
                            .await
                            .map(Some)
                    } else {
                        client
                            .wait_for_message(&room_id, effective_timeout, cursor)
                            .await
                    };
                    match result {
                        Ok(Some(msg)) => {
                            if matches(&msg) {
                                break Ok(Some(msg));
                            }
                            // Filter rejected — advance the bookmark and continue (only
                            // makes sense in --loop mode; without --loop fall through
                            // and return None so the caller sees a timeout-like signal).
                            if !*loop_ {
                                break Ok(None);
                            }
                            cursor = Some(msg.seq);
                            latest_seq.store(msg.seq, std::sync::atomic::Ordering::Relaxed);
                        }
                        other => break other,
                    }
                }
            };

            // `idle_expired` is only reachable in --loop mode with a non-zero
            // deadline; otherwise we just await the loop.
            let mut idle_expired = false;
            let matched: Result<Option<ChatMessage>, _> = if *loop_ && *idle_timeout > 0 {
                // On expiry the loser future is dropped; a message that landed in
                // the broadcast buffer in that instant is discarded, but the resume
                // hint below (from --since-seq) lets the caller catch up on re-run.
                tokio::select! {
                    r = wait_loop => r,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(*idle_timeout)) => {
                        idle_expired = true;
                        Ok(None)
                    }
                }
            } else {
                wait_loop.await
            };

            if let Some(task) = heartbeat_task {
                task.abort();
            }
            if let Some(task) = presence_task {
                task.abort();
            }
            if let Some(task) = thinking_task {
                task.abort();
            }

            // Reset presence regardless of outcome so other agents see us return to idle.
            let _ = client.set_presence("idle", None, None).await;

            if idle_expired {
                let resume = match latest_seq.load(std::sync::atomic::Ordering::Relaxed) {
                    i64::MIN => "tip".to_string(),
                    n => n.to_string(),
                };
                eprintln!(
                    "No message for {}s — the turn may be stalled. Resume with: wait {} --loop --since-seq {}",
                    idle_timeout, room, resume,
                );
                std::process::exit(2);
            }

            match matched? {
                Some(msg) => {
                    // JSON is the default (machine-readable); --text opts into human form.
                    let rendered = if *text {
                        format_message(&msg)
                    } else {
                        serde_json::to_string(&msg)?
                    };
                    match output {
                        Some(path) => {
                            std::fs::write(path, format!("{}\n", rendered))?;
                            eprintln!("wrote message #{} to {}", msg.seq, path.display());
                        }
                        None => println!("{}", rendered),
                    }
                    // A peer that ended the conversation: surface the message, then
                    // exit 3 so a reply-then-wait loop stops instead of waiting again.
                    if msg.metadata.get("kind").and_then(|v| v.as_str())
                        == Some(KIND_CONVERSATION_END)
                    {
                        eprintln!("Peer ended the conversation.");
                        std::process::exit(3);
                    }
                }
                None => {
                    eprintln!("Timed out after {}s waiting for a message", timeout);
                    std::process::exit(1);
                }
            }
        }

        Commands::Monitor { room, json } => {
            let client = connect(&cli).await?;
            let room_secret = resolve_room_secret(&cli);

            // Join room if specified to receive its events
            if let Some(room_id) = room {
                let _ = client.join_room(room_id).await;
            }

            let mut events = client.subscribe();
            println!("Monitoring events (Ctrl+C to stop)...");
            while let Ok(event) = events.recv().await {
                if *json {
                    println!(
                        "{}",
                        serde_json::to_string(&event.frame).unwrap_or_default()
                    );
                } else {
                    print_event(&event.frame, room_secret.as_deref());
                }
            }
        }

        Commands::Shell { room } => {
            run_shell(&cli, room).await?;
        }

        Commands::Keygen => {
            // Purely local — no server connection.
            let key = clawchat_core::crypto::generate_secret();
            println!("{key}");
            eprintln!("# Set the SAME value on every agent in the group:");
            eprintln!("#   export CLAWCHAT_ROOM_KEY={key}");
        }

        Commands::Status => {
            let client = connect(&cli).await?;
            let agents = client.list_agents(None).await?;
            let rooms = client.list_rooms(None).await?;
            println!("ClawChat Server Status");
            println!("  Connected agents: {}", agents.len());
            println!("  Active rooms: {}", rooms.len());
            println!();
            if !agents.is_empty() {
                println!("Agents:");
                for agent in &agents {
                    println!("  - {} ({})", agent.name, agent.agent_id);
                }
            }
        }

        Commands::Export {
            room,
            format,
            since_seq,
            limit,
            include_thinking,
            output,
        } => {
            let client = connect(&cli).await?;
            let room_id = resolve_room_id(&client, room).await?;

            // Pull history. Use a generous default cap; clients with very long
            // rooms can pass --limit to shrink. Default 1000 — agent rooms in
            // the wild rarely exceed this in a single review session.
            let cap = limit.unwrap_or(1000);
            let messages = client
                .get_history_filtered(&room_id, cap, None, None, *since_seq)
                .await?;

            let body = render_export(&messages, *format, *include_thinking, room);

            match output {
                Some(path) => {
                    std::fs::write(path, body)?;
                    eprintln!(
                        "wrote {} messages to {}",
                        messages.len(),
                        path.display()
                    );
                }
                None => {
                    print!("{}", body);
                }
            }
        }

        Commands::Sub { action } => {
            let client = connect(&cli).await?;
            match action {
                SubAction::Create {
                    room,
                    url,
                    secret,
                    kinds,
                    only_from,
                    not_from,
                    exclude_thinking,
                    since_seq,
                } => {
                    let room_id = resolve_room_id(&client, room).await?;
                    let since: Option<i64> = match since_seq.to_lowercase().as_str() {
                        "tip" | "auto" => None,
                        s => Some(s.parse::<i64>().map_err(|e| {
                            Box::<dyn std::error::Error>::from(format!(
                                "--since-seq must be an integer, 'tip', or 'auto': {}",
                                e
                            ))
                        })?),
                    };
                    let sub = client
                        .create_subscription(
                            &room_id,
                            url,
                            secret,
                            kinds.clone(),
                            only_from.as_deref(),
                            not_from.as_deref(),
                            *exclude_thinking,
                            since,
                        )
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&sub)?);
                }
                SubAction::List { room } => {
                    let room_id_opt: Option<String> = match room {
                        Some(r) => Some(resolve_room_id(&client, r).await?),
                        None => None,
                    };
                    let subs = client.list_subscriptions(room_id_opt.as_deref()).await?;
                    if subs.is_empty() {
                        println!("No subscriptions.");
                    } else {
                        println!(
                            "{:<38} {:<38} {:<10} {:<10} {:<12} URL",
                            "ID", "ROOM", "STATUS", "FAILS", "LAST_DELIV"
                        );
                        println!("{}", "-".repeat(140));
                        for s in subs {
                            println!(
                                "{:<38} {:<38} {:<10} {:<10} #{:<11} {}",
                                s.subscription_id,
                                s.room_id,
                                s.status,
                                s.failure_count,
                                s.last_delivered_seq,
                                s.webhook_url
                            );
                        }
                    }
                }
                SubAction::Delete { subscription_id } => {
                    client.unsubscribe(subscription_id).await?;
                    println!("Deleted {}", subscription_id);
                }
                SubAction::Enable { subscription_id } => {
                    client.enable_subscription(subscription_id).await?;
                    println!("Enabled {}", subscription_id);
                }
            }
        }

        Commands::Vote { action } => {
            let client = connect(&cli).await?;
            match action {
                VoteAction::Create {
                    room,
                    title,
                    options,
                    description,
                    duration,
                } => {
                    // Join room first
                    let _ = client.join_room(room).await;
                    let info = client
                        .create_vote(
                            room,
                            title,
                            description.as_deref(),
                            options.clone(),
                            *duration,
                        )
                        .await?;
                    println!("Vote created: {} ({})", info.title, info.vote_id);
                    println!("  Room: {}", info.room_id);
                    println!("  Options:");
                    for (i, opt) in info.options.iter().enumerate() {
                        println!("    [{}] {}", i, opt);
                    }
                    if let Some(deadline) = info.closes_at {
                        println!("  Closes at: {}", deadline.format("%H:%M:%S"));
                    } else {
                        println!("  Closes when all {} members vote", info.eligible_voters);
                    }
                }
                VoteAction::Cast { vote_id, option } => {
                    let resp = client.cast_vote(vote_id, *option).await?;
                    let votes_cast = resp.get("votes_cast").and_then(|v| v.as_u64()).unwrap_or(0);
                    let eligible = resp
                        .get("eligible_voters")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    println!("Ballot cast ({}/{} votes in)", votes_cast, eligible);
                }
                VoteAction::Status { vote_id } => {
                    let info = client.get_vote_status(vote_id).await?;
                    println!("Vote: {} ({})", info.title, info.vote_id);
                    println!("  Status: {:?}", info.status);
                    println!("  Votes cast: {}/{}", info.votes_cast, info.eligible_voters);
                    if let Some(closes_at) = info.closes_at {
                        println!("  Closes at: {}", closes_at.format("%Y-%m-%d %H:%M:%S UTC"));
                    }
                    println!("  Options:");
                    for (i, opt) in info.options.iter().enumerate() {
                        println!("    [{}] {}", i, opt);
                    }
                    if let Some(tally) = info.tally {
                        println!("  Tally:");
                        for row in tally {
                            println!(
                                "    [{}] {}: {}",
                                row.option_index, row.option_text, row.count
                            );
                        }
                    }
                }
                VoteAction::History { room, limit } => {
                    let room_id = resolve_room_id(&client, room).await?;
                    let votes = client.list_votes(&room_id, *limit).await?;

                    if votes.is_empty() {
                        println!("No votes found for room {}", room_id);
                    } else {
                        println!("Votes for room {}:", room_id);
                        for vote in votes {
                            println!(
                                "- {} ({}) {:?} {}/{}",
                                vote.title,
                                vote.vote_id,
                                vote.status,
                                vote.votes_cast,
                                vote.eligible_voters
                            );
                        }
                    }
                }
            }
        }

        Commands::Election { action } => {
            let client = connect(&cli).await?;
            match action {
                ElectionAction::Start { room } => {
                    let _ = client.join_room(room).await;
                    let resp = client.elect_leader(room).await?;
                    println!("{}", serde_json::to_string_pretty(&resp)?);
                }
                ElectionAction::Decline { room } => {
                    let resp = client.decline_election(room).await?;
                    println!("{}", serde_json::to_string_pretty(&resp)?);
                }
                ElectionAction::Decide { room, content } => {
                    let resp = client
                        .send_decision(room, content, serde_json::json!({}))
                        .await?;
                    println!("Decision issued: {}", serde_json::to_string_pretty(&resp)?);
                }
            }
        }

        Commands::Presence {
            status,
            detail,
            progress,
        } => {
            let client = connect(&cli).await?;
            client
                .set_presence(&status, detail.as_deref(), *progress)
                .await?;
            let mut msg = format!("Presence set to: {}", status);
            if let Some(p) = progress {
                msg.push_str(&format!(" ({}%)", p));
            }
            if let Some(d) = detail {
                msg.push_str(&format!(": {}", d));
            }
            println!("{}", msg);
        }
    }

    Ok(())
}

fn print_event(frame: &clawchat_core::Frame, room_secret: Option<&[u8]>) {
    match frame.frame_type {
        FrameType::MessageReceived => {
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let agent = frame
                .payload
                .get("agent_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let content = frame
                .payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let content = decrypt_field(room_secret, room, content);
            println!("[message] #{} {}: {}", room, agent, content);
        }
        FrameType::Mention => {
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!(
                "[mention] from #{}: {:?}",
                room,
                frame.payload.get("message")
            );
        }
        FrameType::AgentJoined => {
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let agent = frame
                .payload
                .get("agent")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("[join] {} joined #{}", agent, room);
        }
        FrameType::AgentLeft => {
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let agent = frame
                .payload
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("[leave] {} left #{}", agent, room);
        }
        FrameType::RoomCreated => {
            let name = frame
                .payload
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let ephemeral = frame
                .payload
                .get("ephemeral")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let tag = if ephemeral { " (ephemeral)" } else { "" };
            println!("[room+] created #{}{}", name, tag);
        }
        FrameType::RoomDestroyed => {
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("[room-] destroyed #{}", room);
        }
        FrameType::VoteCreated => {
            let title = frame
                .payload
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let vote_id = frame
                .payload
                .get("vote_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("[vote] New vote in #{}: \"{}\" ({})", room, title, vote_id);
        }
        FrameType::VoteResult => {
            let title = frame
                .payload
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("[vote-result] #{} \"{}\":", room, title);
            if let Some(tally) = frame.payload.get("tally").and_then(|v| v.as_array()) {
                for entry in tally {
                    let text = entry
                        .get("option_text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let count = entry.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                    println!("  {} : {} votes", text, count);
                }
            }
        }
        FrameType::ElectionStarted => {
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!(
                "[election] Election started in #{} (2s opt-out window)",
                room
            );
        }
        FrameType::LeaderElected => {
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let name = frame
                .payload
                .get("leader_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("[leader] {} elected leader of #{}", name, room);
        }
        FrameType::LeaderCleared => {
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let reason = frame
                .payload
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("[leader-] Leadership cleared in #{}: {}", room, reason);
        }
        FrameType::DecisionMade => {
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let leader = frame
                .payload
                .get("leader_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let content = frame
                .payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let content = decrypt_field(room_secret, room, content);
            println!("[decision] #{} {} decides: {}", room, leader, content);
        }
        FrameType::PresenceUpdate => {
            let agent = frame
                .payload
                .get("agent_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let status = frame
                .payload
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let progress = frame
                .payload
                .get("progress")
                .and_then(|v| v.as_u64())
                .map(|p| format!(" ({}%)", p))
                .unwrap_or_default();
            let detail = frame
                .payload
                .get("status_detail")
                .and_then(|v| v.as_str())
                .map(|d| format!(": {}", d))
                .unwrap_or_default();
            println!(
                "[presence] {} is now {}{}{}",
                agent, status, progress, detail
            );
        }
        FrameType::Thinking => {
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let agent = frame
                .payload
                .get("agent_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let content = frame
                .payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let content = decrypt_field(room_secret, room, content);
            println!("[thinking] #{} {}: {}", room, agent, content);
        }
        FrameType::TurnChanged => {
            let room = frame
                .payload
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let holder = frame
                .payload
                .get("current_turn_holder")
                .and_then(|v| v.as_str())
                .unwrap_or("(none)");
            let reason = frame
                .payload
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("[turn] #{} -> {} ({})", room, holder, reason);
        }
        _ => {
            println!("[{:?}] {:?}", frame.frame_type, frame.payload);
        }
    }
}
