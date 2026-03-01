use clawdchat_core::*;
use dashmap::DashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;

use crate::auth;
use crate::broker::Broker;
use crate::connection::AgentConnection;
use crate::handler;
use crate::rate_limit::{RateLimiter, TierLimits};
use crate::reconnect::ReconnectManager;
use crate::store::Store;
use crate::tasks::TaskManager;
use crate::voting::VoteManager;

/// How often the server sends a ping to each connected agent.
const HEARTBEAT_INTERVAL_SECS: u64 = 30;
/// If no data received from an agent within this window, disconnect them.
const HEARTBEAT_TIMEOUT_SECS: u64 = 90;

pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub tcp_addr: Option<String>,
    pub http_addr: Option<String>,
    pub db_path: PathBuf,
    pub auth_key_path: PathBuf,
    pub no_auth: bool,
}

pub struct ClawdChatServer {
    config: ServerConfig,
    broker: Arc<Broker>,
    store: Arc<Store>,
    ephemeral_rooms: Arc<DashMap<String, Room>>,
    vote_mgr: Arc<VoteManager>,
    rate_limiter: Arc<RateLimiter>,
    reconnect_mgr: Arc<ReconnectManager>,
    task_mgr: Arc<TaskManager>,
    api_key: String,
}

impl ClawdChatServer {
    pub fn new(config: ServerConfig) -> Result<Self, Box<dyn std::error::Error>> {
        // Ensure data directories exist
        if let Some(parent) = config.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let store = Arc::new(Store::open(&config.db_path)?);
        let agents: Arc<DashMap<String, AgentConnection>> = Arc::new(DashMap::new());
        let room_members: Arc<DashMap<String, HashSet<String>>> = Arc::new(DashMap::new());
        let broker = Arc::new(Broker::new(agents, room_members));
        let ephemeral_rooms = Arc::new(DashMap::new());
        let vote_mgr = Arc::new(VoteManager::new());
        let rate_limiter = Arc::new(RateLimiter::new());
        let reconnect_mgr = Arc::new(ReconnectManager::new());
        let task_mgr = Arc::new(TaskManager::new());
        let api_key = auth::load_or_create_key(&config.auth_key_path)?;

        log::info!("API key loaded from {:?}", config.auth_key_path);
        log::info!("Database at {:?}", config.db_path);

        Ok(Self {
            config,
            broker,
            store,
            ephemeral_rooms,
            vote_mgr,
            rate_limiter,
            reconnect_mgr,
            task_mgr,
            api_key,
        })
    }

    /// Start the server, listening on UDS, TCP, and/or HTTP (as configured).
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Clean up stale socket file
        if self.config.socket_path.exists() {
            std::fs::remove_file(&self.config.socket_path)?;
        }
        if let Some(parent) = self.config.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let uds_listener = UnixListener::bind(&self.config.socket_path)?;
        log::info!("Listening on UDS: {:?}", self.config.socket_path);

        // Spawn UDS accept loop as a task
        let uds_broker = self.broker.clone();
        let uds_store = self.store.clone();
        let uds_ephemeral = self.ephemeral_rooms.clone();
        let uds_vote_mgr = self.vote_mgr.clone();
        let uds_api_key = self.api_key.clone();
        let uds_no_auth = self.config.no_auth;
        let uds_rate_limiter = self.rate_limiter.clone();
        let uds_reconnect_mgr = self.reconnect_mgr.clone();
        let uds_task_mgr = self.task_mgr.clone();
        let uds_task = tokio::spawn(async move {
            loop {
                match uds_listener.accept().await {
                    Ok((stream, _addr)) => {
                        let (read_half, write_half) = tokio::io::split(stream);
                        let broker = uds_broker.clone();
                        let store = uds_store.clone();
                        let ephemeral = uds_ephemeral.clone();
                        let vote_mgr = uds_vote_mgr.clone();
                        let api_key = uds_api_key.clone();
                        let rate_limiter = uds_rate_limiter.clone();
                        let reconnect_mgr = uds_reconnect_mgr.clone();
                        let task_mgr = uds_task_mgr.clone();
                        tokio::spawn(async move {
                            let _ = connection_loop(
                                read_half, write_half, broker, store, ephemeral,
                                vote_mgr, api_key, uds_no_auth, rate_limiter,
                                reconnect_mgr, task_mgr,
                            ).await;
                        });
                    }
                    Err(e) => {
                        log::error!("UDS accept error: {}", e);
                        break;
                    }
                }
            }
        });

        // Spawn TCP accept loop if configured
        let tcp_task = if let Some(addr) = &self.config.tcp_addr {
            let tcp_listener = TcpListener::bind(addr).await?;
            log::info!("Listening on TCP: {}", addr);
            let tcp_broker = self.broker.clone();
            let tcp_store = self.store.clone();
            let tcp_ephemeral = self.ephemeral_rooms.clone();
            let tcp_vote_mgr = self.vote_mgr.clone();
            let tcp_api_key = self.api_key.clone();
            let tcp_no_auth = self.config.no_auth;
            let tcp_rate_limiter = self.rate_limiter.clone();
            let tcp_reconnect_mgr = self.reconnect_mgr.clone();
            let tcp_task_mgr = self.task_mgr.clone();
            Some(tokio::spawn(async move {
                loop {
                    match tcp_listener.accept().await {
                        Ok((stream, addr)) => {
                            log::info!("TCP connection from {}", addr);
                            let (read_half, write_half) = tokio::io::split(stream);
                            let broker = tcp_broker.clone();
                            let store = tcp_store.clone();
                            let ephemeral = tcp_ephemeral.clone();
                            let vote_mgr = tcp_vote_mgr.clone();
                            let api_key = tcp_api_key.clone();
                            let rate_limiter = tcp_rate_limiter.clone();
                            let reconnect_mgr = tcp_reconnect_mgr.clone();
                            let task_mgr = tcp_task_mgr.clone();
                            tokio::spawn(async move {
                                let _ = connection_loop(
                                    read_half, write_half, broker, store, ephemeral,
                                    vote_mgr, api_key, tcp_no_auth, rate_limiter,
                                    reconnect_mgr, task_mgr,
                                ).await;
                            });
                        }
                        Err(e) => {
                            log::error!("TCP accept error: {}", e);
                            break;
                        }
                    }
                }
            }))
        } else {
            None
        };

        // Spawn HTTP/WebSocket listener if configured
        let http_task = if let Some(addr) = &self.config.http_addr {
            let app_state = crate::web::AppState {
                broker: self.broker.clone(),
                store: self.store.clone(),
                ephemeral_rooms: self.ephemeral_rooms.clone(),
                vote_mgr: self.vote_mgr.clone(),
                rate_limiter: self.rate_limiter.clone(),
                no_auth: self.config.no_auth,
                api_key: self.api_key.clone(),
                reconnect_mgr: self.reconnect_mgr.clone(),
                task_mgr: self.task_mgr.clone(),
            };
            let router = crate::web::router(app_state);
            let listener = tokio::net::TcpListener::bind(addr).await?;
            log::info!("HTTP/WebSocket listening on {}", addr);
            Some(tokio::spawn(async move {
                if let Err(e) = axum::serve(listener, router).await {
                    log::error!("HTTP server error: {}", e);
                }
            }))
        } else {
            None
        };

        // Wait for shutdown signal
        tokio::select! {
            _ = uds_task => {},
            _ = async { if let Some(t) = tcp_task { t.await.ok(); } else { std::future::pending::<()>().await } } => {},
            _ = async { if let Some(t) = http_task { t.await.ok(); } else { std::future::pending::<()>().await } } => {},
            _ = tokio::signal::ctrl_c() => {
                log::info!("Shutting down...");
            }
        }

        Ok(())
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn socket_path(&self) -> &Path {
        &self.config.socket_path
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn broker(&self) -> &Arc<Broker> {
        &self.broker
    }

    pub fn rate_limiter(&self) -> &Arc<RateLimiter> {
        &self.rate_limiter
    }

    pub fn reconnect_mgr(&self) -> &Arc<ReconnectManager> {
        &self.reconnect_mgr
    }

    pub fn task_mgr(&self) -> &Arc<TaskManager> {
        &self.task_mgr
    }
}

impl Drop for ClawdChatServer {
    fn drop(&mut self) {
        // Clean up socket file
        let _ = std::fs::remove_file(&self.config.socket_path);
    }
}

/// Main per-connection loop. Handles registration then dispatches frames.
pub async fn connection_loop<R, W>(
    read_half: R,
    mut write_half: W,
    broker: Arc<Broker>,
    store: Arc<Store>,
    ephemeral_rooms: Arc<DashMap<String, Room>>,
    vote_mgr: Arc<VoteManager>,
    api_key: String,
    no_auth: bool,
    rate_limiter: Arc<RateLimiter>,
    reconnect_mgr: Arc<ReconnectManager>,
    task_mgr: Arc<TaskManager>,
) -> Result<(), Box<dyn std::error::Error>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    // Phase 1: Wait for register command
    let (agent_id, agent_name, agent_capabilities, session_id, agent_api_key, reconnected_rooms) = loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // Connection closed before registration
        }

        let frame = match Frame::from_line(&line) {
            Ok(f) => f,
            Err(e) => {
                let err_frame = Frame::error(
                    None,
                    ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
                );
                write_half.write_all(err_frame.to_line()?.as_bytes()).await?;
                continue;
            }
        };

        if frame.frame_type == FrameType::Ping {
            let pong = Frame::pong(frame.id.as_deref());
            write_half.write_all(pong.to_line()?.as_bytes()).await?;
            continue;
        }

        if frame.frame_type != FrameType::Register {
            let err = Frame::error(
                frame.id.as_deref(),
                ErrorPayload::new(ErrorCode::NotRegistered, "Must register first"),
            );
            write_half.write_all(err.to_line()?.as_bytes()).await?;
            continue;
        }

        let payload: RegisterPayload = match serde_json::from_value(frame.payload) {
            Ok(p) => p,
            Err(e) => {
                let err = Frame::error(
                    frame.id.as_deref(),
                    ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
                );
                write_half.write_all(err.to_line()?.as_bytes()).await?;
                continue;
            }
        };

        // Validate API key
        let authenticated_key = if no_auth
            || payload.key == api_key
            || store.validate_api_key(&payload.key).unwrap_or(false)
        {
            payload.key.clone()
        } else {
            let err = Frame::error(
                frame.id.as_deref(),
                ErrorPayload::new(ErrorCode::Unauthorized, "Invalid API key"),
            );
            write_half.write_all(err.to_line()?.as_bytes()).await?;
            return Ok(());
        };

        // Check rate limit for agent count (skip in no_auth mode)
        if !no_auth && !authenticated_key.is_empty() {
            let tier = store.get_key_tier(&authenticated_key).unwrap_or_else(|_| "free".to_string());
            let limits = TierLimits::for_tier(&tier);
            if !rate_limiter.check_agent_limit(&authenticated_key, &limits) {
                let err = Frame::error(
                    frame.id.as_deref(),
                    ErrorPayload::new(ErrorCode::RateLimitAgents, "Agent limit exceeded for this API key"),
                );
                write_half.write_all(err.to_line()?.as_bytes()).await?;
                return Ok(());
            }
        }

        let agent_id = payload
            .agent_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // === Reconnect logic ===
        // If the agent requests reconnect and we have stashed state, reclaim it.
        let mut reconnected_rooms: Option<HashSet<String>> = None;
        let mut missed_messages: Vec<Frame> = Vec::new();

        if payload.reconnect && reconnect_mgr.is_stashed(&agent_id) {
            if let Some(stashed) = reconnect_mgr.reclaim(&agent_id) {
                reconnected_rooms = Some(stashed.rooms);
                missed_messages = stashed.missed_messages;
            }
        } else if broker.agents.contains_key(&agent_id) {
            // Agent is still "connected" (stale) — reject
            let err = Frame::error(
                frame.id.as_deref(),
                ErrorPayload::new(ErrorCode::AgentIdTaken, "Agent ID already in use"),
            );
            write_half.write_all(err.to_line()?.as_bytes()).await?;
            continue;
        }

        // Build the OK response
        let mut ok_payload = serde_json::json!({
            "agent_id": agent_id,
            "name": payload.name,
        });

        if let Some(ref rooms) = reconnected_rooms {
            let room_list: Vec<&String> = rooms.iter().collect();
            ok_payload["reconnected"] = serde_json::json!(true);
            ok_payload["rooms"] = serde_json::json!(room_list);
            ok_payload["missed_messages"] = serde_json::json!(missed_messages.len());
        }

        let ok = Frame::ok(frame.id.as_deref(), ok_payload);
        write_half.write_all(ok.to_line()?.as_bytes()).await?;

        // Replay missed messages directly to the socket (before channel setup)
        for msg in &missed_messages {
            if let Ok(msg_line) = msg.to_line() {
                write_half.write_all(msg_line.as_bytes()).await?;
            }
        }

        // Track agent in rate limiter
        if !authenticated_key.is_empty() {
            rate_limiter.add_agent(&authenticated_key);
        }

        log::info!(
            "Agent registered: {} ({}) capabilities={:?}{}",
            payload.name,
            agent_id,
            payload.capabilities,
            if reconnected_rooms.is_some() { " [RECONNECTED]" } else { "" },
        );

        // Record session
        let session_id = uuid::Uuid::new_v4().to_string();
        let _ = store.record_session_start(
            &session_id,
            &agent_id,
            &payload.name,
            &payload.capabilities,
        );

        break (agent_id, payload.name, payload.capabilities, session_id, authenticated_key, reconnected_rooms);
    };

    // Phase 2: Set up channel + task pair
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();

    // Send task: drains channel -> writes to socket
    let send_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            match frame.to_line() {
                Ok(line) => {
                    if write_half.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::error!("Frame serialization error: {}", e);
                }
            }
        }
    });

    let agent_info = AgentInfo {
        agent_id: agent_id.clone(),
        name: agent_name.clone(),
        capabilities: agent_capabilities.clone(),
        connected_at: Some(chrono::Utc::now()),
        last_active: Some(chrono::Utc::now()),
    };

    // Store the connection
    let conn = AgentConnection::new(
        agent_info,
        session_id.clone(),
        tx.clone(),
        send_task,
        tokio::spawn(async {}),
        agent_api_key.clone(),
    );
    broker.agents.insert(agent_id.clone(), conn);

    // Restore room memberships if reconnecting
    if let Some(rooms) = reconnected_rooms {
        for room_id in &rooms {
            broker.join_room(&agent_id, room_id);
            if let Some(mut agent) = broker.agents.get_mut(&agent_id) {
                agent.rooms.insert(room_id.clone());
            }

            // Broadcast rejoin event
            let event = Frame::event(
                FrameType::AgentJoined,
                serde_json::json!({
                    "room_id": room_id,
                    "agent": {
                        "agent_id": agent_id,
                        "name": agent_name,
                    },
                    "reconnected": true,
                }),
            );
            broker.broadcast_to_room(room_id, &agent_id, &event);
        }
    }

    // Phase 3: Read and process frames (with heartbeat)
    let heartbeat_tx = tx.clone();
    let heartbeat_agent_id = agent_id.clone();
    let heartbeat_broker = broker.clone();
    let heartbeat_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        loop {
            interval.tick().await;
            // Only send heartbeat if the agent is still connected
            if !heartbeat_broker.agents.contains_key(&heartbeat_agent_id) {
                break;
            }
            let ping = Frame {
                id: Some(uuid::Uuid::new_v4().to_string()),
                reply_to: None,
                frame_type: FrameType::Ping,
                payload: serde_json::json!({"heartbeat": true}),
            };
            if heartbeat_tx.send(ping).is_err() {
                break;
            }
        }
    });

    let mut last_activity = std::time::Instant::now();

    loop {
        line.clear();
        let read_result = tokio::time::timeout(
            Duration::from_secs(HEARTBEAT_TIMEOUT_SECS),
            reader.read_line(&mut line),
        ).await;

        match read_result {
            Ok(Ok(0)) => break, // Connection closed
            Ok(Ok(_)) => {
                last_activity = std::time::Instant::now();

                let frame = match Frame::from_line(&line) {
                    Ok(f) => f,
                    Err(e) => {
                        let err = Frame::error(
                            None,
                            ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
                        );
                        let _ = tx.send(err);
                        continue;
                    }
                };

                // Pong responses don't need processing
                if frame.frame_type == FrameType::Pong {
                    continue;
                }

                let response = handler::handle_frame(
                    frame,
                    &agent_id,
                    &agent_name,
                    &broker,
                    &store,
                    &ephemeral_rooms,
                    &vote_mgr,
                    &agent_api_key,
                    &rate_limiter,
                    no_auth,
                    &task_mgr,
                )
                .await;
                let _ = tx.send(response);
            }
            Ok(Err(e)) => {
                log::warn!(
                    "Read error for {} ({}), treating as disconnect: {}",
                    agent_name, agent_id, e
                );
                break;
            }
            Err(_) => {
                // Timeout — no data received within HEARTBEAT_TIMEOUT_SECS
                log::warn!(
                    "Heartbeat timeout for {} ({}) — last activity {:.0}s ago",
                    agent_name, agent_id, last_activity.elapsed().as_secs_f64()
                );
                break;
            }
        }
    }

    // Stop heartbeat task
    heartbeat_task.abort();

    // Phase 4: Cleanup on disconnect
    log::info!("Agent disconnected: {} ({})", agent_name, agent_id);

    // Remove from rate limiter
    if !agent_api_key.is_empty() {
        rate_limiter.remove_agent(&agent_api_key);
    }

    // Collect room memberships BEFORE leaving them (for stash)
    let agent_rooms: HashSet<String> = broker.agents.get(&agent_id)
        .map(|a| a.rooms.clone())
        .unwrap_or_default();

    // Stash for reconnect (only for permanent rooms — don't stash ephemeral-only agents)
    let has_permanent_rooms = agent_rooms.iter().any(|r| !ephemeral_rooms.contains_key(r));
    if has_permanent_rooms {
        reconnect_mgr.stash(
            agent_id.clone(),
            agent_name.clone(),
            agent_api_key.clone(),
            agent_rooms.clone(),
        );
    }

    // Leave all rooms and clean up ephemeral rooms
    let left_rooms = broker.leave_all_rooms(&agent_id);
    for (room_id, now_empty) in &left_rooms {
        // Broadcast leave event
        let event = Frame::event(
            FrameType::AgentLeft,
            serde_json::json!({
                "room_id": room_id,
                "agent_id": agent_id,
            }),
        );
        broker.broadcast_to_room_all(room_id, &event);

        // Buffer the leave event for stashed agents in this room
        for stashed_id in reconnect_mgr.stashed_members_of_room(room_id) {
            if stashed_id != agent_id {
                reconnect_mgr.buffer_message(&stashed_id, event.clone());
            }
        }

        // Destroy empty ephemeral rooms
        if *now_empty {
            if let Some((_, _)) = ephemeral_rooms.remove(room_id) {
                broker.remove_room(room_id);
                let destroy = Frame::event(
                    FrameType::RoomDestroyed,
                    serde_json::json!({"room_id": room_id}),
                );
                for entry in broker.agents.iter() {
                    broker.send_to_agent(entry.key(), destroy.clone());
                }
                log::info!("Ephemeral room {} destroyed (agent disconnected)", room_id);
            }
        }
    }

    // Clear leadership if this agent was a leader
    vote_mgr.clear_leader_if_agent(&agent_id, &broker);

    // Remove agent connection
    broker.agents.remove(&agent_id);

    // Record session end
    let _ = store.record_session_end(&session_id);

    Ok(())
}
