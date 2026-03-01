use clawdchat_core::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Server error: {code:?} - {message}")]
    Server { code: ErrorCode, message: String },
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("Request timed out")]
    Timeout,
    #[error("Channel error")]
    Channel,
}

/// An event received from the server (pushed, not in response to a request).
#[derive(Debug, Clone)]
pub struct Event {
    pub frame: Frame,
}

pub struct ClawdChatClient {
    /// Channel to send frames to the writer task.
    write_tx: mpsc::UnboundedSender<Frame>,
    /// Pending request completions: correlation_id -> oneshot sender.
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Frame>>>>,
    /// Broadcast channel for server-pushed events.
    event_tx: broadcast::Sender<Event>,
    /// Agent info after registration.
    pub agent_id: String,
    pub agent_name: String,
}

impl ClawdChatClient {
    /// Connect via Unix domain socket and register.
    pub async fn connect_uds(
        socket_path: &Path,
        key: &str,
        name: &str,
        agent_id: Option<&str>,
        capabilities: Vec<String>,
    ) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket_path).await?;
        let (read_half, write_half) = tokio::io::split(stream);
        Self::setup(read_half, write_half, key, name, agent_id, capabilities).await
    }

    /// Connect via TCP and register.
    pub async fn connect_tcp(
        addr: &str,
        key: &str,
        name: &str,
        agent_id: Option<&str>,
        capabilities: Vec<String>,
    ) -> Result<Self, ClientError> {
        let stream = TcpStream::connect(addr).await?;
        let (read_half, write_half) = tokio::io::split(stream);
        Self::setup(read_half, write_half, key, name, agent_id, capabilities).await
    }

    async fn setup<R, W>(
        read_half: R,
        write_half: W,
        key: &str,
        name: &str,
        agent_id: Option<&str>,
        capabilities: Vec<String>,
    ) -> Result<Self, ClientError>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Frame>();
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Frame>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, _) = broadcast::channel::<Event>(256);

        // Writer task
        let mut write_half = write_half;
        tokio::spawn(async move {
            while let Some(frame) = write_rx.recv().await {
                match frame.to_line() {
                    Ok(line) => {
                        if write_half.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        log::error!("Client frame serialization error: {}", e);
                    }
                }
            }
        });

        // Reader task
        let pending_clone = pending.clone();
        let event_tx_clone = event_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        if let Ok(frame) = Frame::from_line(&line) {
                            // Check if this is a response to a pending request
                            if let Some(reply_to) = &frame.reply_to {
                                let mut pending = pending_clone.lock().await;
                                if let Some(sender) = pending.remove(reply_to) {
                                    let _ = sender.send(frame);
                                    continue;
                                }
                            }
                            // Otherwise it's a pushed event
                            let _ = event_tx_clone.send(Event { frame });
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Register
        let register_frame = Frame {
            id: Some(uuid::Uuid::new_v4().to_string()),
            reply_to: None,
            frame_type: FrameType::Register,
            payload: serde_json::to_value(RegisterPayload {
                key: key.to_string(),
                agent_id: agent_id.map(String::from),
                name: name.to_string(),
                capabilities,
                reconnect: false,
            })
            .unwrap(),
        };

        let req_id = register_frame.id.clone().unwrap();
        let (resp_tx, resp_rx) = oneshot::channel();
        {
            let mut p = pending.lock().await;
            p.insert(req_id.clone(), resp_tx);
        }
        write_tx
            .send(register_frame)
            .map_err(|_| ClientError::Channel)?;

        let response = tokio::time::timeout(std::time::Duration::from_secs(5), resp_rx)
            .await
            .map_err(|_| ClientError::Timeout)?
            .map_err(|_| ClientError::ConnectionClosed)?;

        if response.frame_type == FrameType::Error {
            let err: ErrorPayload = serde_json::from_value(response.payload)
                .unwrap_or(ErrorPayload::new(ErrorCode::InternalError, "Unknown error"));
            return Err(ClientError::Server {
                code: err.code,
                message: err.message,
            });
        }

        let agent_id = response
            .payload
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let agent_name = name.to_string();

        Ok(Self {
            write_tx,
            pending,
            event_tx,
            agent_id,
            agent_name,
        })
    }

    /// Send a request and wait for the response.
    async fn request(
        &self,
        frame_type: FrameType,
        payload: serde_json::Value,
    ) -> Result<Frame, ClientError> {
        let id = uuid::Uuid::new_v4().to_string();
        let frame = Frame {
            id: Some(id.clone()),
            reply_to: None,
            frame_type,
            payload,
        };

        let (resp_tx, resp_rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, resp_tx);
        }

        self.write_tx
            .send(frame)
            .map_err(|_| ClientError::Channel)?;

        let response = tokio::time::timeout(std::time::Duration::from_secs(10), resp_rx)
            .await
            .map_err(|_| ClientError::Timeout)?
            .map_err(|_| ClientError::ConnectionClosed)?;

        if response.frame_type == FrameType::Error {
            let err: ErrorPayload = serde_json::from_value(response.payload)
                .unwrap_or(ErrorPayload::new(ErrorCode::InternalError, "Unknown error"));
            return Err(ClientError::Server {
                code: err.code,
                message: err.message,
            });
        }

        Ok(response)
    }

    // --- High-level API ---

    pub async fn ping(&self) -> Result<(), ClientError> {
        self.request(FrameType::Ping, serde_json::json!({})).await?;
        Ok(())
    }

    pub async fn create_room(
        &self,
        name: &str,
        description: Option<&str>,
        parent_id: Option<&str>,
        ephemeral: bool,
    ) -> Result<Room, ClientError> {
        let resp = self
            .request(
                FrameType::CreateRoom,
                serde_json::to_value(CreateRoomPayload {
                    name: name.to_string(),
                    description: description.map(String::from),
                    parent_id: parent_id.map(String::from),
                    ephemeral,
                    public: false,
                })
                .unwrap(),
            )
            .await?;
        Ok(serde_json::from_value(resp.payload).unwrap())
    }

    pub async fn join_room(&self, room_id: &str) -> Result<(), ClientError> {
        self.request(FrameType::JoinRoom, serde_json::json!({"room_id": room_id}))
            .await?;
        Ok(())
    }

    pub async fn leave_room(&self, room_id: &str) -> Result<(), ClientError> {
        self.request(
            FrameType::LeaveRoom,
            serde_json::json!({"room_id": room_id}),
        )
        .await?;
        Ok(())
    }

    pub async fn send_message(
        &self,
        room_id: &str,
        content: &str,
        reply_to: Option<&str>,
        mentions: Vec<String>,
    ) -> Result<ChatMessage, ClientError> {
        let resp = self
            .request(
                FrameType::SendMessage,
                serde_json::to_value(SendMessagePayload {
                    room_id: room_id.to_string(),
                    content: content.to_string(),
                    reply_to: reply_to.map(String::from),
                    metadata: serde_json::json!({}),
                    mentions,
                })
                .unwrap(),
            )
            .await?;
        Ok(serde_json::from_value(resp.payload).unwrap())
    }

    pub async fn get_history(
        &self,
        room_id: &str,
        limit: u32,
        before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<ChatMessage>, ClientError> {
        self.get_history_since(room_id, limit, before, None).await
    }

    /// Get history with optional `since` filter (returns messages after the given message_id).
    pub async fn get_history_since(
        &self,
        room_id: &str,
        limit: u32,
        before: Option<chrono::DateTime<chrono::Utc>>,
        since: Option<&str>,
    ) -> Result<Vec<ChatMessage>, ClientError> {
        let resp = self
            .request(
                FrameType::GetHistory,
                serde_json::to_value(GetHistoryPayload {
                    room_id: room_id.to_string(),
                    limit,
                    before,
                    since: since.map(String::from),
                })
                .unwrap(),
            )
            .await?;
        let messages: Vec<ChatMessage> = resp
            .payload
            .get("messages")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        Ok(messages)
    }

    pub async fn list_rooms(&self, parent_id: Option<&str>) -> Result<Vec<Room>, ClientError> {
        let resp = self
            .request(
                FrameType::ListRooms,
                serde_json::json!({"parent_id": parent_id}),
            )
            .await?;
        let rooms: Vec<Room> = resp
            .payload
            .get("rooms")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        Ok(rooms)
    }

    pub async fn list_agents(&self, room_id: Option<&str>) -> Result<Vec<AgentInfo>, ClientError> {
        let resp = self
            .request(
                FrameType::ListAgents,
                serde_json::json!({"room_id": room_id}),
            )
            .await?;
        let agents: Vec<AgentInfo> = resp
            .payload
            .get("agents")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        Ok(agents)
    }

    pub async fn room_info(&self, room_id: &str) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .request(FrameType::RoomInfo, serde_json::json!({"room_id": room_id}))
            .await?;
        Ok(resp.payload)
    }

    // --- Voting API ---

    /// Create a sealed-ballot vote in a room.
    pub async fn create_vote(
        &self,
        room_id: &str,
        title: &str,
        description: Option<&str>,
        options: Vec<String>,
        duration_secs: Option<u64>,
    ) -> Result<VoteInfo, ClientError> {
        let resp = self
            .request(
                FrameType::CreateVote,
                serde_json::to_value(CreateVotePayload {
                    room_id: room_id.to_string(),
                    title: title.to_string(),
                    description: description.map(String::from),
                    options,
                    duration_secs,
                })
                .unwrap(),
            )
            .await?;
        Ok(serde_json::from_value(resp.payload).unwrap())
    }

    /// Cast a ballot in an active vote (sealed until vote closes).
    pub async fn cast_vote(
        &self,
        vote_id: &str,
        option_index: usize,
    ) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .request(
                FrameType::CastVote,
                serde_json::to_value(CastVotePayload {
                    vote_id: vote_id.to_string(),
                    option_index,
                })
                .unwrap(),
            )
            .await?;
        Ok(resp.payload)
    }

    /// Get the current status of a vote.
    ///
    /// For open votes this reports counts only. For closed votes it also includes
    /// revealed tally data.
    pub async fn get_vote_status(&self, vote_id: &str) -> Result<VoteInfo, ClientError> {
        let resp = self
            .request(
                FrameType::GetVoteStatus,
                serde_json::to_value(GetVoteStatusPayload {
                    vote_id: vote_id.to_string(),
                })
                .unwrap(),
            )
            .await?;
        Ok(serde_json::from_value(resp.payload).unwrap())
    }

    /// List recent votes for a room (open and/or closed).
    pub async fn list_votes(
        &self,
        room_id: &str,
        limit: u32,
    ) -> Result<Vec<VoteInfo>, ClientError> {
        let resp = self
            .request(
                FrameType::ListVotes,
                serde_json::to_value(ListVotesPayload {
                    room_id: room_id.to_string(),
                    limit,
                })
                .unwrap(),
            )
            .await?;

        let votes: Vec<VoteInfo> = resp
            .payload
            .get("votes")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        Ok(votes)
    }

    // --- Election API ---

    /// Start a leader election in a room.
    pub async fn elect_leader(&self, room_id: &str) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .request(
                FrameType::ElectLeader,
                serde_json::to_value(ElectLeaderPayload {
                    room_id: room_id.to_string(),
                })
                .unwrap(),
            )
            .await?;
        Ok(resp.payload)
    }

    /// Decline an active election (opt out of candidacy).
    pub async fn decline_election(&self, room_id: &str) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .request(
                FrameType::DeclineElection,
                serde_json::to_value(DeclineElectionPayload {
                    room_id: room_id.to_string(),
                })
                .unwrap(),
            )
            .await?;
        Ok(resp.payload)
    }

    /// Issue a decision as the room leader.
    pub async fn send_decision(
        &self,
        room_id: &str,
        content: &str,
        metadata: serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .request(
                FrameType::Decision,
                serde_json::to_value(DecisionPayload {
                    room_id: room_id.to_string(),
                    content: content.to_string(),
                    metadata,
                })
                .unwrap(),
            )
            .await?;
        Ok(resp.payload)
    }

    // --- Presence API ---

    /// Signal that this agent is typing (or stopped typing) in a room.
    pub async fn set_typing(&self, room_id: &str, typing: bool) -> Result<(), ClientError> {
        self.request(
            FrameType::SetTyping,
            serde_json::json!({"room_id": room_id, "typing": typing}),
        )
        .await?;
        Ok(())
    }

    /// Subscribe to server-pushed events (messages, joins, leaves, etc.)
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.event_tx.subscribe()
    }

    /// Wait for the next message in a specific room. Blocks until a message arrives or timeout.
    /// Returns the message, or None on timeout.
    pub async fn wait_for_message(
        &self,
        room_id: &str,
        timeout_secs: u64,
    ) -> Result<Option<ChatMessage>, ClientError> {
        let mut events = self.event_tx.subscribe();
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                _ = &mut deadline => {
                    return Ok(None);
                }
                event = events.recv() => {
                    match event {
                        Ok(evt) => {
                            if evt.frame.frame_type == FrameType::MessageReceived {
                                if let Some(event_room) = evt.frame.payload.get("room_id").and_then(|v| v.as_str()) {
                                    if event_room == room_id {
                                        let msg: ChatMessage = serde_json::from_value(evt.frame.payload)
                                            .map_err(ClientError::Json)?;
                                        return Ok(Some(msg));
                                    }
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Err(ClientError::ConnectionClosed);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            continue;
                        }
                    }
                }
            }
        }
    }
}
