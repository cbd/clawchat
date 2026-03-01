use clap::{Parser, Subcommand};
use clawdchat_client::{ClawdChatClient, ClientError};
use clawdchat_core::{ErrorCode, FrameType};
use std::io::Write;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Parser)]
#[command(
    name = "clawdchat",
    about = "ClawdChat - Agent-to-agent chat infrastructure"
)]
struct Cli {
    /// Unix socket path to connect to
    #[arg(long, global = true, default_value = default_socket_path())]
    socket: PathBuf,

    /// Use TCP instead of Unix socket
    #[arg(long, global = true)]
    tcp: Option<String>,

    /// API key (reads from ~/.clawdchat/auth.key if not provided)
    #[arg(long, global = true)]
    key: Option<String>,

    /// Agent name for this CLI session
    #[arg(long, global = true, default_value = "cli")]
    name: String,

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
    },
    /// Get room info
    Info {
        /// Room ID
        room_id: String,
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

fn default_data_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".clawdchat"))
        .unwrap_or_else(|| PathBuf::from(".clawdchat"))
}

fn default_socket_path() -> &'static str {
    Box::leak(
        default_data_dir()
            .join("clawdchat.sock")
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
        Err("No API key provided. Use --key or ensure ~/.clawdchat/auth.key exists.".into())
    }
}

async fn connect(cli: &Cli) -> Result<ClawdChatClient, Box<dyn std::error::Error>> {
    let key = load_key(&cli.key)?;

    if let Some(addr) = &cli.tcp {
        Ok(ClawdChatClient::connect_tcp(addr, &key, &cli.name, None, vec![]).await?)
    } else {
        Ok(ClawdChatClient::connect_uds(&cli.socket, &key, &cli.name, None, vec![]).await?)
    }
}

async fn resolve_room_id(
    client: &ClawdChatClient,
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
    print!("clawdchat[{room}]> ");
    let _ = std::io::stdout().flush();
}

async fn run_shell(
    cli: &Cli,
    start_room: &Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = connect(cli).await?;
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
                                        println!("{:<38} {:<20} CAPABILITIES", "AGENT ID", "NAME");
                                        println!("{}", "-".repeat(70));
                                        for agent in agents {
                                            let caps = if agent.capabilities.is_empty() {
                                                "-".to_string()
                                            } else {
                                                agent.capabilities.join(", ")
                                            };
                                            println!("{:<38} {:<20} {}", agent.agent_id, agent.name, caps);
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
                                        println!(
                                            "[{}] {}: {}",
                                            msg.timestamp.format("%H:%M:%S"),
                                            msg.agent_name,
                                            msg.content
                                        );
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
                        print_event(&event.frame);
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
        } => {
            let client = connect(&cli).await?;
            let room_id = resolve_room_id(&client, room).await?;
            client.join_room(&room_id).await?;
            let msg = client
                .send_message(&room_id, message, reply_to.as_deref(), vec![])
                .await?;
            println!(
                "[{}] {}: {}",
                msg.timestamp.format("%H:%M:%S"),
                msg.agent_name,
                msg.content
            );
        }

        Commands::Rooms { action } => {
            let client = connect(&cli).await?;
            match action {
                RoomAction::List { parent } => {
                    let rooms = client.list_rooms(parent.as_deref()).await?;
                    if rooms.is_empty() {
                        println!("No rooms found.");
                    } else {
                        println!("{:<38} {:<20} {:<10} DESCRIPTION", "ID", "NAME", "TYPE");
                        println!("{}", "-".repeat(80));
                        for room in rooms {
                            let room_type = if room.ephemeral {
                                "ephemeral"
                            } else {
                                "permanent"
                            };
                            let desc = room.description.as_deref().unwrap_or("");
                            println!(
                                "{:<38} {:<20} {:<10} {}",
                                room.room_id, room.name, room_type, desc
                            );
                        }
                    }
                }
                RoomAction::Create {
                    name,
                    description,
                    parent,
                    ephemeral,
                } => {
                    let room = client
                        .create_room(name, description.as_deref(), parent.as_deref(), *ephemeral)
                        .await?;
                    println!("Created room: {} ({})", room.name, room.room_id);
                    if room.ephemeral {
                        println!("  Type: ephemeral (auto-deleted when empty)");
                    }
                }
                RoomAction::Info { room_id } => {
                    let info = client.room_info(room_id).await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
            }
        }

        Commands::Agents { room } => {
            let client = connect(&cli).await?;
            let agents = client.list_agents(room.as_deref()).await?;
            if agents.is_empty() {
                println!("No agents connected.");
            } else {
                println!("{:<38} {:<20} CAPABILITIES", "AGENT ID", "NAME");
                println!("{}", "-".repeat(70));
                for agent in agents {
                    let caps = if agent.capabilities.is_empty() {
                        "-".to_string()
                    } else {
                        agent.capabilities.join(", ")
                    };
                    println!("{:<38} {:<20} {}", agent.agent_id, agent.name, caps);
                }
            }
        }

        Commands::History {
            room,
            limit,
            follow,
        } => {
            let client = connect(&cli).await?;

            // Show history
            let messages = client.get_history(room, *limit, None).await?;
            for msg in &messages {
                println!(
                    "[{}] {}: {}",
                    msg.timestamp.format("%H:%M:%S"),
                    msg.agent_name,
                    msg.content
                );
            }

            if *follow {
                // Join the room to receive new messages
                let _ = client.join_room(room).await;
                let mut events = client.subscribe();
                println!("--- streaming new messages (Ctrl+C to stop) ---");
                while let Ok(event) = events.recv().await {
                    if event.frame.frame_type == FrameType::MessageReceived {
                        if let Some(room_id) =
                            event.frame.payload.get("room_id").and_then(|v| v.as_str())
                        {
                            if room_id == room {
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

        Commands::Monitor { room, json } => {
            let client = connect(&cli).await?;

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
                    print_event(&event.frame);
                }
            }
        }

        Commands::Shell { room } => {
            run_shell(&cli, room).await?;
        }

        Commands::Status => {
            let client = connect(&cli).await?;
            let agents = client.list_agents(None).await?;
            let rooms = client.list_rooms(None).await?;
            println!("ClawdChat Server Status");
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
    }

    Ok(())
}

fn print_event(frame: &clawdchat_core::Frame) {
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
            println!("[decision] #{} {} decides: {}", room, leader, content);
        }
        _ => {
            println!("[{:?}] {:?}", frame.frame_type, frame.payload);
        }
    }
}
