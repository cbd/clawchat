use clawdchat_core::*;
use dashmap::DashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;

use crate::auth;
use crate::broker::Broker;
use crate::connection::AgentConnection;
use crate::handler;
use crate::store::Store;
use crate::voting::VoteManager;

pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub tcp_addr: Option<String>,
    pub db_path: PathBuf,
    pub auth_key_path: PathBuf,
}

pub struct ClawdChatServer {
    config: ServerConfig,
    broker: Arc<Broker>,
    store: Arc<Store>,
    ephemeral_rooms: Arc<DashMap<String, Room>>,
    vote_mgr: Arc<VoteManager>,
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
        let api_key = auth::load_or_create_key(&config.auth_key_path)?;

        log::info!("API key loaded from {:?}", config.auth_key_path);
        log::info!("Database at {:?}", config.db_path);

        Ok(Self {
            config,
            broker,
            store,
            ephemeral_rooms,
            vote_mgr,
            api_key,
        })
    }

    /// Start the server, listening on both UDS and TCP (if configured).
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

        match &self.config.tcp_addr {
            Some(addr) => {
                let tcp_listener = TcpListener::bind(addr).await?;
                log::info!("Listening on TCP: {}", addr);

                tokio::select! {
                    result = self.accept_uds_loop(uds_listener) => result,
                    result = self.accept_tcp_loop(tcp_listener) => result,
                    _ = tokio::signal::ctrl_c() => {
                        log::info!("Shutting down...");
                        Ok(())
                    }
                }
            }
            None => {
                tokio::select! {
                    result = self.accept_uds_loop(uds_listener) => result,
                    _ = tokio::signal::ctrl_c() => {
                        log::info!("Shutting down...");
                        Ok(())
                    }
                }
            }
        }
    }

    async fn accept_uds_loop(
        &self,
        listener: UnixListener,
    ) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let (stream, _addr) = listener.accept().await?;
            let (read_half, write_half) = tokio::io::split(stream);
            self.handle_connection(read_half, write_half).await;
        }
    }

    async fn accept_tcp_loop(
        &self,
        listener: TcpListener,
    ) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let (stream, addr) = listener.accept().await?;
            log::info!("TCP connection from {}", addr);
            let (read_half, write_half) = tokio::io::split(stream);
            self.handle_connection(read_half, write_half).await;
        }
    }

    /// Handle a new connection: wait for registration, then process frames.
    async fn handle_connection<R, W>(&self, read_half: R, write_half: W)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let broker = self.broker.clone();
        let store = self.store.clone();
        let ephemeral_rooms = self.ephemeral_rooms.clone();
        let vote_mgr = self.vote_mgr.clone();
        let api_key = self.api_key.clone();

        tokio::spawn(async move {
            if let Err(e) =
                connection_loop(read_half, write_half, broker, store, ephemeral_rooms, vote_mgr, api_key)
                    .await
            {
                log::error!("Connection error: {}", e);
            }
        });
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn socket_path(&self) -> &Path {
        &self.config.socket_path
    }
}

impl Drop for ClawdChatServer {
    fn drop(&mut self) {
        // Clean up socket file
        let _ = std::fs::remove_file(&self.config.socket_path);
    }
}

/// Main per-connection loop. Handles registration then dispatches frames.
async fn connection_loop<R, W>(
    read_half: R,
    mut write_half: W,
    broker: Arc<Broker>,
    store: Arc<Store>,
    ephemeral_rooms: Arc<DashMap<String, Room>>,
    vote_mgr: Arc<VoteManager>,
    api_key: String,
) -> Result<(), Box<dyn std::error::Error>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    // Phase 1: Wait for register command
    let (agent_id, agent_name, agent_capabilities, session_id) = loop {
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
        if payload.key != api_key {
            let err = Frame::error(
                frame.id.as_deref(),
                ErrorPayload::new(ErrorCode::Unauthorized, "Invalid API key"),
            );
            write_half.write_all(err.to_line()?.as_bytes()).await?;
            return Ok(()); // Close connection on auth failure
        }

        let agent_id = payload
            .agent_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // Check if agent_id is already taken
        if broker.agents.contains_key(&agent_id) {
            let err = Frame::error(
                frame.id.as_deref(),
                ErrorPayload::new(ErrorCode::AgentIdTaken, "Agent ID already in use"),
            );
            write_half.write_all(err.to_line()?.as_bytes()).await?;
            continue;
        }

        // Send OK before setting up the channel (so it goes directly on the socket)
        let ok = Frame::ok(
            frame.id.as_deref(),
            serde_json::json!({
                "agent_id": agent_id,
                "name": payload.name,
            }),
        );
        write_half.write_all(ok.to_line()?.as_bytes()).await?;

        log::info!(
            "Agent registered: {} ({}) capabilities={:?}",
            payload.name,
            agent_id,
            payload.capabilities
        );

        // Record session
        let session_id = uuid::Uuid::new_v4().to_string();
        let _ = store.record_session_start(
            &session_id,
            &agent_id,
            &payload.name,
            &payload.capabilities,
        );

        break (agent_id, payload.name, payload.capabilities, session_id);
    };

    // Phase 2: Set up channel + task pair (the chat-notifier pattern)
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
    };

    // Store the connection (receive_task placeholder - we'll update it)
    let conn = AgentConnection::new(
        agent_info,
        session_id.clone(),
        tx.clone(),
        send_task,
        // Placeholder task that immediately completes
        tokio::spawn(async {}),
    );
    broker.agents.insert(agent_id.clone(), conn);

    // Phase 3: Read and process frames
    loop {
        line.clear();
        let n = match reader.read_line(&mut line).await {
            Ok(n) => n,
            Err(e) => {
                log::warn!(
                    "Read error for {} ({}), treating as disconnect: {}",
                    agent_name,
                    agent_id,
                    e
                );
                break;
            }
        };
        if n == 0 {
            break; // Connection closed
        }

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

        let response =
            handler::handle_frame(frame, &agent_id, &agent_name, &broker, &store, &ephemeral_rooms, &vote_mgr)
                .await;
        let _ = tx.send(response);
    }

    // Phase 4: Cleanup on disconnect
    log::info!("Agent disconnected: {} ({})", agent_name, agent_id);

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
