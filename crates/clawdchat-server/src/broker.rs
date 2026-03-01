use clawdchat_core::{ChatMessage, Frame, FrameType};
use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::connection::AgentConnection;

/// Routes messages to room members and handles @mentions.
pub struct Broker {
    /// All connected agents: agent_id -> AgentConnection
    pub agents: Arc<DashMap<String, AgentConnection>>,
    /// Room membership: room_id -> set of agent_ids
    pub room_members: Arc<DashMap<String, HashSet<String>>>,
}

impl Broker {
    pub fn new(
        agents: Arc<DashMap<String, AgentConnection>>,
        room_members: Arc<DashMap<String, HashSet<String>>>,
    ) -> Self {
        Self {
            agents,
            room_members,
        }
    }

    /// Broadcast a message to all members of a room, except the sender.
    pub fn broadcast_to_room(&self, room_id: &str, sender_id: &str, frame: &Frame) {
        if let Some(members) = self.room_members.get(room_id) {
            for member_id in members.iter() {
                if member_id == sender_id {
                    continue;
                }
                self.send_to_agent(member_id, frame.clone());
            }
        }
    }

    /// Broadcast a frame to ALL members of a room (including sender).
    pub fn broadcast_to_room_all(&self, room_id: &str, frame: &Frame) {
        if let Some(members) = self.room_members.get(room_id) {
            for member_id in members.iter() {
                self.send_to_agent(member_id, frame.clone());
            }
        }
    }

    /// Send a mention notification to specific agents, even if not in the room.
    pub fn send_mentions(&self, mentions: &[String], message: &ChatMessage, room_id: &str) {
        let frame = Frame::event(
            FrameType::Mention,
            serde_json::json!({
                "room_id": room_id,
                "message": message,
            }),
        );

        for agent_id in mentions {
            self.send_to_agent(agent_id, frame.clone());
        }
    }

    /// Send a frame to a specific agent by ID.
    pub fn send_to_agent(&self, agent_id: &str, frame: Frame) {
        if let Some(agent) = self.agents.get(agent_id) {
            if agent.sender.send(frame).is_err() {
                log::warn!("Failed to send to agent {}: channel closed", agent_id);
            }
        }
    }

    /// Check if an agent is in a specific room.
    pub fn is_agent_in_room(&self, agent_id: &str, room_id: &str) -> bool {
        self.room_members
            .get(room_id)
            .map(|members| members.contains(agent_id))
            .unwrap_or(false)
    }

    /// Add an agent to a room.
    pub fn join_room(&self, agent_id: &str, room_id: &str) {
        self.room_members
            .entry(room_id.to_string())
            .or_default()
            .insert(agent_id.to_string());
    }

    /// Remove an agent from a room. Returns true if room is now empty.
    pub fn leave_room(&self, agent_id: &str, room_id: &str) -> bool {
        if let Some(mut members) = self.room_members.get_mut(room_id) {
            members.remove(agent_id);
            members.is_empty()
        } else {
            true
        }
    }

    /// Remove an agent from all rooms. Returns list of (room_id, now_empty) pairs.
    pub fn leave_all_rooms(&self, agent_id: &str) -> Vec<(String, bool)> {
        let mut results = Vec::new();
        let room_ids: Vec<String> = self
            .room_members
            .iter()
            .filter(|entry| entry.value().contains(agent_id))
            .map(|entry| entry.key().clone())
            .collect();

        for room_id in room_ids {
            let now_empty = self.leave_room(agent_id, &room_id);
            results.push((room_id, now_empty));
        }
        results
    }

    /// Get all agent IDs in a room.
    pub fn get_room_members(&self, room_id: &str) -> Vec<String> {
        self.room_members
            .get(room_id)
            .map(|members| members.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Remove the membership entry for a room entirely.
    pub fn remove_room(&self, room_id: &str) {
        self.room_members.remove(room_id);
    }

    /// Get sender channel for a new agent connection.
    pub fn create_agent_channel(&self) -> (mpsc::UnboundedSender<Frame>, mpsc::UnboundedReceiver<Frame>) {
        mpsc::unbounded_channel()
    }
}
