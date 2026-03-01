use clawdchat_core::*;
use std::sync::Arc;

use crate::broker::Broker;
use crate::store::Store;
use crate::voting::VoteManager;

/// Processes a single frame from a registered agent and returns a response frame.
/// Side effects (broadcast, persistence) happen inline.
pub async fn handle_frame(
    frame: Frame,
    agent_id: &str,
    agent_name: &str,
    broker: &Arc<Broker>,
    store: &Arc<Store>,
    ephemeral_rooms: &Arc<dashmap::DashMap<String, Room>>,
    vote_mgr: &Arc<VoteManager>,
) -> Frame {
    let req_id = frame.id.as_deref();

    match frame.frame_type {
        FrameType::Ping => Frame::pong(req_id),

        FrameType::CreateRoom => handle_create_room(req_id, frame.payload, agent_id, broker, store, ephemeral_rooms).await,
        FrameType::JoinRoom => handle_join_room(req_id, frame.payload, agent_id, agent_name, broker, store, ephemeral_rooms).await,
        FrameType::LeaveRoom => handle_leave_room(req_id, frame.payload, agent_id, broker, ephemeral_rooms).await,
        FrameType::SendMessage => handle_send_message(req_id, frame.payload, agent_id, agent_name, broker, store, ephemeral_rooms).await,
        FrameType::GetHistory => handle_get_history(req_id, frame.payload, store).await,
        FrameType::ListRooms => handle_list_rooms(req_id, frame.payload, store, ephemeral_rooms).await,
        FrameType::ListAgents => handle_list_agents(req_id, frame.payload, broker).await,
        FrameType::RoomInfo => handle_room_info(req_id, frame.payload, store, broker, ephemeral_rooms).await,

        // Voting
        FrameType::CreateVote => handle_create_vote(req_id, frame.payload, agent_id, broker, store, ephemeral_rooms, vote_mgr).await,
        FrameType::CastVote => handle_cast_vote(req_id, frame.payload, agent_id, agent_name, broker, store, vote_mgr).await,
        FrameType::GetVoteStatus => handle_get_vote_status(req_id, frame.payload, vote_mgr, broker).await,

        // Elections
        FrameType::ElectLeader => handle_elect_leader(req_id, frame.payload, agent_id, broker, vote_mgr).await,
        FrameType::DeclineElection => handle_decline_election(req_id, frame.payload, agent_id, vote_mgr).await,
        FrameType::Decision => handle_decision(req_id, frame.payload, agent_id, agent_name, broker, store, ephemeral_rooms, vote_mgr).await,

        _ => Frame::error(
            req_id,
            ErrorPayload::new(ErrorCode::InvalidPayload, "Unknown command"),
        ),
    }
}

async fn handle_create_room(
    req_id: Option<&str>,
    payload: serde_json::Value,
    agent_id: &str,
    broker: &Arc<Broker>,
    store: &Arc<Store>,
    ephemeral_rooms: &Arc<dashmap::DashMap<String, Room>>,
) -> Frame {
    let p: CreateRoomPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    let room_id = uuid::Uuid::new_v4().to_string();

    if p.ephemeral {
        let room = Room {
            room_id: room_id.clone(),
            name: p.name.clone(),
            description: p.description,
            parent_id: p.parent_id,
            ephemeral: true,
            created_at: chrono::Utc::now(),
            created_by: Some(agent_id.to_string()),
        };
        ephemeral_rooms.insert(room_id.clone(), room.clone());

        // Broadcast room creation to all connected agents
        let event = Frame::event(FrameType::RoomCreated, serde_json::to_value(&room).unwrap());
        for entry in broker.agents.iter() {
            broker.send_to_agent(entry.key(), event.clone());
        }

        Frame::ok(req_id, serde_json::to_value(&room).unwrap())
    } else {
        // Validate parent exists if specified
        if let Some(ref pid) = p.parent_id {
            let parent_exists = store.get_room(pid).ok().flatten().is_some()
                || ephemeral_rooms.contains_key(pid);
            if !parent_exists {
                return Frame::error(
                    req_id,
                    ErrorPayload::new(ErrorCode::RoomNotFound, "Parent room not found"),
                );
            }
        }

        match store.create_room(&room_id, &p.name, p.description.as_deref(), p.parent_id.as_deref(), Some(agent_id)) {
            Ok(room) => {
                let event = Frame::event(FrameType::RoomCreated, serde_json::to_value(&room).unwrap());
                for entry in broker.agents.iter() {
                    broker.send_to_agent(entry.key(), event.clone());
                }
                Frame::ok(req_id, serde_json::to_value(&room).unwrap())
            }
            Err(crate::store::StoreError::RoomNameTaken(name)) => Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::RoomNameTaken, format!("Room name '{}' already taken", name)),
            ),
            Err(e) => Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InternalError, e.to_string()),
            ),
        }
    }
}

async fn handle_join_room(
    req_id: Option<&str>,
    payload: serde_json::Value,
    agent_id: &str,
    agent_name: &str,
    broker: &Arc<Broker>,
    store: &Arc<Store>,
    ephemeral_rooms: &Arc<dashmap::DashMap<String, Room>>,
) -> Frame {
    let p: JoinRoomPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    // Check room exists
    let room_exists = store.get_room(&p.room_id).ok().flatten().is_some()
        || ephemeral_rooms.contains_key(&p.room_id);
    if !room_exists {
        return Frame::error(
            req_id,
            ErrorPayload::new(ErrorCode::RoomNotFound, "Room not found"),
        );
    }

    if broker.is_agent_in_room(agent_id, &p.room_id) {
        return Frame::error(
            req_id,
            ErrorPayload::new(ErrorCode::AlreadyInRoom, "Already in this room"),
        );
    }

    broker.join_room(agent_id, &p.room_id);

    // Track room in agent connection
    if let Some(mut agent) = broker.agents.get_mut(agent_id) {
        agent.rooms.insert(p.room_id.clone());
    }

    // Broadcast join to other room members
    let event = Frame::event(
        FrameType::AgentJoined,
        serde_json::json!({
            "room_id": p.room_id,
            "agent": {
                "agent_id": agent_id,
                "name": agent_name,
            }
        }),
    );
    broker.broadcast_to_room(&p.room_id, agent_id, &event);

    Frame::ok(req_id, serde_json::json!({"room_id": p.room_id}))
}

async fn handle_leave_room(
    req_id: Option<&str>,
    payload: serde_json::Value,
    agent_id: &str,
    broker: &Arc<Broker>,
    ephemeral_rooms: &Arc<dashmap::DashMap<String, Room>>,
) -> Frame {
    let p: LeaveRoomPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    if !broker.is_agent_in_room(agent_id, &p.room_id) {
        return Frame::error(
            req_id,
            ErrorPayload::new(ErrorCode::NotInRoom, "Not in this room"),
        );
    }

    // Broadcast leave before removing
    let event = Frame::event(
        FrameType::AgentLeft,
        serde_json::json!({
            "room_id": p.room_id,
            "agent_id": agent_id,
        }),
    );
    broker.broadcast_to_room(&p.room_id, agent_id, &event);

    let room_empty = broker.leave_room(agent_id, &p.room_id);

    // Track in agent connection
    if let Some(mut agent) = broker.agents.get_mut(agent_id) {
        agent.rooms.remove(&p.room_id);
    }

    // Destroy ephemeral room if empty
    if room_empty {
        if let Some((_, _room)) = ephemeral_rooms.remove(&p.room_id) {
            broker.remove_room(&p.room_id);
            let destroy_event = Frame::event(
                FrameType::RoomDestroyed,
                serde_json::json!({"room_id": p.room_id}),
            );
            for entry in broker.agents.iter() {
                broker.send_to_agent(entry.key(), destroy_event.clone());
            }
            log::info!("Ephemeral room {} destroyed (empty)", p.room_id);
        }
    }

    Frame::ok(req_id, serde_json::json!({"room_id": p.room_id}))
}

async fn handle_send_message(
    req_id: Option<&str>,
    payload: serde_json::Value,
    agent_id: &str,
    agent_name: &str,
    broker: &Arc<Broker>,
    store: &Arc<Store>,
    ephemeral_rooms: &Arc<dashmap::DashMap<String, Room>>,
) -> Frame {
    let p: SendMessagePayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    if !broker.is_agent_in_room(agent_id, &p.room_id) {
        return Frame::error(
            req_id,
            ErrorPayload::new(ErrorCode::NotInRoom, "Not in this room"),
        );
    }

    let message_id = uuid::Uuid::new_v4().to_string();
    let is_ephemeral = ephemeral_rooms.contains_key(&p.room_id);

    let message = if is_ephemeral {
        // Ephemeral rooms: don't persist messages
        ChatMessage {
            message_id: message_id.clone(),
            room_id: p.room_id.clone(),
            agent_id: agent_id.to_string(),
            agent_name: agent_name.to_string(),
            content: p.content,
            reply_to_message: p.reply_to,
            metadata: p.metadata,
            timestamp: chrono::Utc::now(),
        }
    } else {
        // Permanent rooms: persist to SQLite
        match store.insert_message(
            &message_id,
            &p.room_id,
            agent_id,
            agent_name,
            &p.content,
            p.reply_to.as_deref(),
            &p.metadata,
        ) {
            Ok(msg) => msg,
            Err(e) => {
                return Frame::error(
                    req_id,
                    ErrorPayload::new(ErrorCode::InternalError, e.to_string()),
                )
            }
        }
    };

    // Broadcast to room members (excluding sender)
    let event = Frame::event(
        FrameType::MessageReceived,
        serde_json::to_value(&message).unwrap(),
    );
    broker.broadcast_to_room(&p.room_id, agent_id, &event);

    // Handle @mentions
    if !p.mentions.is_empty() {
        broker.send_mentions(&p.mentions, &message, &p.room_id);
    }

    Frame::ok(req_id, serde_json::to_value(&message).unwrap())
}

async fn handle_get_history(
    req_id: Option<&str>,
    payload: serde_json::Value,
    store: &Arc<Store>,
) -> Frame {
    let p: GetHistoryPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    match store.get_history(&p.room_id, p.limit, p.before) {
        Ok(messages) => Frame {
            id: Some(uuid::Uuid::new_v4().to_string()),
            reply_to: req_id.map(String::from),
            frame_type: FrameType::HistoryResult,
            payload: serde_json::json!({
                "room_id": p.room_id,
                "messages": messages,
            }),
        },
        Err(e) => Frame::error(
            req_id,
            ErrorPayload::new(ErrorCode::InternalError, e.to_string()),
        ),
    }
}

async fn handle_list_rooms(
    req_id: Option<&str>,
    payload: serde_json::Value,
    store: &Arc<Store>,
    ephemeral_rooms: &Arc<dashmap::DashMap<String, Room>>,
) -> Frame {
    let p: ListRoomsPayload = serde_json::from_value(payload).unwrap_or(ListRoomsPayload {
        parent_id: None,
    });

    let mut rooms = match store.list_rooms(p.parent_id.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InternalError, e.to_string()),
            )
        }
    };

    // Include ephemeral rooms
    for entry in ephemeral_rooms.iter() {
        let room = entry.value();
        let matches = match &p.parent_id {
            Some(pid) => room.parent_id.as_deref() == Some(pid.as_str()),
            None => true,
        };
        if matches {
            rooms.push(room.clone());
        }
    }

    Frame {
        id: Some(uuid::Uuid::new_v4().to_string()),
        reply_to: req_id.map(String::from),
        frame_type: FrameType::RoomList,
        payload: serde_json::json!({"rooms": rooms}),
    }
}

async fn handle_list_agents(
    req_id: Option<&str>,
    payload: serde_json::Value,
    broker: &Arc<Broker>,
) -> Frame {
    let p: ListAgentsPayload = serde_json::from_value(payload).unwrap_or(ListAgentsPayload {
        room_id: None,
    });

    let agents: Vec<AgentInfo> = match &p.room_id {
        Some(room_id) => {
            let member_ids = broker.get_room_members(room_id);
            member_ids
                .iter()
                .filter_map(|id| broker.agents.get(id).map(|a| a.info.clone()))
                .collect()
        }
        None => broker.agents.iter().map(|a| a.info.clone()).collect(),
    };

    Frame {
        id: Some(uuid::Uuid::new_v4().to_string()),
        reply_to: req_id.map(String::from),
        frame_type: FrameType::AgentList,
        payload: serde_json::json!({"agents": agents}),
    }
}

async fn handle_room_info(
    req_id: Option<&str>,
    payload: serde_json::Value,
    store: &Arc<Store>,
    broker: &Arc<Broker>,
    ephemeral_rooms: &Arc<dashmap::DashMap<String, Room>>,
) -> Frame {
    let p: RoomInfoPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    // Find the room (check permanent then ephemeral)
    let room = store
        .get_room(&p.room_id)
        .ok()
        .flatten()
        .or_else(|| ephemeral_rooms.get(&p.room_id).map(|r| r.clone()));

    let room = match room {
        Some(r) => r,
        None => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::RoomNotFound, "Room not found"),
            )
        }
    };

    let members = broker.get_room_members(&p.room_id);
    let agents: Vec<AgentInfo> = members
        .iter()
        .filter_map(|id| broker.agents.get(id).map(|a| a.info.clone()))
        .collect();

    // Get sub-rooms
    let mut sub_rooms = store
        .list_rooms(Some(&p.room_id))
        .unwrap_or_default();
    for entry in ephemeral_rooms.iter() {
        if entry.value().parent_id.as_deref() == Some(&p.room_id) {
            sub_rooms.push(entry.value().clone());
        }
    }

    Frame {
        id: Some(uuid::Uuid::new_v4().to_string()),
        reply_to: req_id.map(String::from),
        frame_type: FrameType::RoomInfoResult,
        payload: serde_json::json!({
            "room": room,
            "agents": agents,
            "sub_rooms": sub_rooms,
        }),
    }
}

// --- Voting handlers ---

async fn handle_create_vote(
    req_id: Option<&str>,
    payload: serde_json::Value,
    agent_id: &str,
    broker: &Arc<Broker>,
    store: &Arc<Store>,
    ephemeral_rooms: &Arc<dashmap::DashMap<String, Room>>,
    vote_mgr: &Arc<VoteManager>,
) -> Frame {
    let p: CreateVotePayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    // Must be in the room
    if !broker.is_agent_in_room(agent_id, &p.room_id) {
        return Frame::error(
            req_id,
            ErrorPayload::new(ErrorCode::NotInRoom, "Must be in room to create a vote"),
        );
    }

    if p.options.len() < 2 {
        return Frame::error(
            req_id,
            ErrorPayload::new(ErrorCode::InvalidPayload, "Need at least 2 options"),
        );
    }

    let vote_id = uuid::Uuid::new_v4().to_string();
    let eligible = broker.get_room_members(&p.room_id).len();
    let is_ephemeral = ephemeral_rooms.contains_key(&p.room_id);

    // Persist to SQLite if not ephemeral
    if !is_ephemeral {
        let closes_at = p.duration_secs.map(|s| chrono::Utc::now() + chrono::Duration::seconds(s as i64));
        if let Err(e) = store.create_vote(
            &vote_id,
            &p.room_id,
            &p.title,
            p.description.as_deref(),
            &p.options,
            agent_id,
            closes_at,
        ) {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InternalError, e.to_string()),
            );
        }
    }

    // Create in-memory tracking (manages deadline timer)
    let vote = vote_mgr.create_vote(
        vote_id.clone(),
        p.room_id.clone(),
        p.title.clone(),
        p.description.clone(),
        p.options.clone(),
        agent_id.to_string(),
        p.duration_secs,
        eligible,
        is_ephemeral,
        broker.clone(),
        store.clone(),
    );

    let info = VoteInfo {
        vote_id: vote.vote_id.clone(),
        room_id: vote.room_id.clone(),
        title: vote.title.clone(),
        description: vote.description.clone(),
        options: vote.options.clone(),
        created_by: vote.created_by.clone(),
        created_at: vote.created_at,
        closes_at: vote.closes_at,
        status: VoteStatus::Open,
        votes_cast: 0,
        eligible_voters: eligible,
    };

    // Broadcast VoteCreated to room
    let event = Frame::event(FrameType::VoteCreated, serde_json::to_value(&info).unwrap());
    broker.broadcast_to_room_all(&p.room_id, &event);

    Frame::ok(req_id, serde_json::to_value(&info).unwrap())
}

async fn handle_cast_vote(
    req_id: Option<&str>,
    payload: serde_json::Value,
    agent_id: &str,
    agent_name: &str,
    broker: &Arc<Broker>,
    store: &Arc<Store>,
    vote_mgr: &Arc<VoteManager>,
) -> Frame {
    let p: CastVotePayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    // Check if vote is active (in-memory)
    let (is_ephemeral, option_count) = match vote_mgr.active_votes.get(&p.vote_id) {
        Some(vote) => (vote.is_ephemeral, Some(vote.options.len())),
        None => (false, None),
    };

    // Validate option index before persisting ballot.
    if let Some(count) = option_count {
        if p.option_index >= count {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidOption, "Invalid option index"),
            );
        }
    }

    // Persist ballot to SQLite if not ephemeral
    if !is_ephemeral {
        if let Err(e) = store.cast_vote(&p.vote_id, agent_id, agent_name, p.option_index) {
            let code = match e {
                crate::store::StoreError::VoteNotFound => ErrorCode::VoteNotFound,
                crate::store::StoreError::VoteClosed => ErrorCode::VoteClosed,
                crate::store::StoreError::AlreadyVoted => ErrorCode::AlreadyVoted,
                _ => ErrorCode::InternalError,
            };
            return Frame::error(req_id, ErrorPayload::new(code, e.to_string()));
        }
    }

    // Record in-memory (may trigger close + broadcast)
    match vote_mgr
        .cast_vote(&p.vote_id, agent_id, agent_name, p.option_index, broker, store)
        .await
    {
        Ok((votes_cast, eligible)) => Frame::ok(
            req_id,
            serde_json::json!({
                "vote_id": p.vote_id,
                "votes_cast": votes_cast,
                "eligible_voters": eligible,
            }),
        ),
        Err(code) => Frame::error(
            req_id,
            ErrorPayload::new(code.clone(), format!("{:?}", code)),
        ),
    }
}

async fn handle_get_vote_status(
    req_id: Option<&str>,
    payload: serde_json::Value,
    vote_mgr: &Arc<VoteManager>,
    _broker: &Arc<Broker>,
) -> Frame {
    let p: GetVoteStatusPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    // Check in-memory active votes first
    if let Some(vote) = vote_mgr.active_votes.get(&p.vote_id) {
        let info = VoteInfo {
            vote_id: vote.vote_id.clone(),
            room_id: vote.room_id.clone(),
            title: vote.title.clone(),
            description: vote.description.clone(),
            options: vote.options.clone(),
            created_by: vote.created_by.clone(),
            created_at: vote.created_at,
            closes_at: vote.closes_at,
            status: VoteStatus::Open,
            votes_cast: vote.ballots.len(),
            eligible_voters: vote.eligible_voters,
        };
        return Frame::ok(req_id, serde_json::to_value(&info).unwrap());
    }

    // Not active -- vote not found (closed votes are already broadcast as VoteResult)
    Frame::error(
        req_id,
        ErrorPayload::new(ErrorCode::VoteNotFound, "Vote not found or already closed"),
    )
}

// --- Election handlers ---

async fn handle_elect_leader(
    req_id: Option<&str>,
    payload: serde_json::Value,
    agent_id: &str,
    broker: &Arc<Broker>,
    vote_mgr: &Arc<VoteManager>,
) -> Frame {
    let p: ElectLeaderPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    if !broker.is_agent_in_room(agent_id, &p.room_id) {
        return Frame::error(
            req_id,
            ErrorPayload::new(ErrorCode::NotInRoom, "Must be in room to start election"),
        );
    }

    let candidates = broker.get_room_members(&p.room_id);
    if candidates.is_empty() {
        return Frame::error(
            req_id,
            ErrorPayload::new(ErrorCode::InvalidPayload, "No candidates in room"),
        );
    }

    match vote_mgr.start_election(&p.room_id, candidates.clone(), agent_id, broker.clone()) {
        Ok(()) => {
            // Broadcast ElectionStarted to room
            let event = Frame::event(
                FrameType::ElectionStarted,
                serde_json::json!({
                    "room_id": p.room_id,
                    "candidates": candidates,
                    "started_by": agent_id,
                    "opt_out_seconds": 2,
                }),
            );
            broker.broadcast_to_room_all(&p.room_id, &event);

            Frame::ok(
                req_id,
                serde_json::json!({
                    "room_id": p.room_id,
                    "candidates": candidates,
                    "message": "Election started. Agents have 2 seconds to decline.",
                }),
            )
        }
        Err(code) => Frame::error(
            req_id,
            ErrorPayload::new(code.clone(), format!("{:?}", code)),
        ),
    }
}

async fn handle_decline_election(
    req_id: Option<&str>,
    payload: serde_json::Value,
    agent_id: &str,
    vote_mgr: &Arc<VoteManager>,
) -> Frame {
    let p: DeclineElectionPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    match vote_mgr.decline_election(&p.room_id, agent_id) {
        Ok(()) => Frame::ok(
            req_id,
            serde_json::json!({
                "room_id": p.room_id,
                "message": "Declined election",
            }),
        ),
        Err(code) => Frame::error(
            req_id,
            ErrorPayload::new(code.clone(), format!("{:?}", code)),
        ),
    }
}

async fn handle_decision(
    req_id: Option<&str>,
    payload: serde_json::Value,
    agent_id: &str,
    agent_name: &str,
    broker: &Arc<Broker>,
    store: &Arc<Store>,
    ephemeral_rooms: &Arc<dashmap::DashMap<String, Room>>,
    vote_mgr: &Arc<VoteManager>,
) -> Frame {
    let p: DecisionPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InvalidPayload, e.to_string()),
            )
        }
    };

    // Must be the room leader
    if !vote_mgr.is_leader(&p.room_id, agent_id) {
        return Frame::error(
            req_id,
            ErrorPayload::new(ErrorCode::NotLeader, "Only the room leader can issue decisions"),
        );
    }

    let message_id = uuid::Uuid::new_v4().to_string();
    let is_ephemeral = ephemeral_rooms.contains_key(&p.room_id);

    // Persist as a special message if not ephemeral
    if !is_ephemeral {
        let decision_metadata = serde_json::json!({"type": "decision", "decided_by": agent_id});
        if let Err(e) = store.insert_message(
            &message_id,
            &p.room_id,
            agent_id,
            agent_name,
            &p.content,
            None,
            &decision_metadata,
        ) {
            return Frame::error(
                req_id,
                ErrorPayload::new(ErrorCode::InternalError, e.to_string()),
            );
        }
    }

    // Broadcast DecisionMade to room
    let event = Frame::event(
        FrameType::DecisionMade,
        serde_json::json!({
            "room_id": p.room_id,
            "message_id": message_id,
            "leader_id": agent_id,
            "leader_name": agent_name,
            "content": p.content,
            "metadata": p.metadata,
            "timestamp": chrono::Utc::now(),
        }),
    );
    broker.broadcast_to_room_all(&p.room_id, &event);

    Frame::ok(
        req_id,
        serde_json::json!({
            "message_id": message_id,
            "room_id": p.room_id,
        }),
    )
}
