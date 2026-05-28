use clawchat_core::Frame;
use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long we keep a disconnected agent's state before discarding it.
const RECONNECT_WINDOW_SECS: u64 = 120;

/// Maximum messages buffered per disconnected agent.
const MAX_BUFFERED_MESSAGES: usize = 200;

/// State stashed when an agent disconnects, allowing seamless reconnect.
pub struct StashedAgent {
    pub agent_id: String,
    pub name: String,
    pub api_key: String,
    pub rooms: HashSet<String>,
    pub missed_messages: Vec<Frame>,
    pub disconnected_at: Instant,
}

/// Manages reconnect state for recently disconnected agents.
pub struct ReconnectManager {
    stashed: Arc<DashMap<String, StashedAgent>>,
}

impl ReconnectManager {
    pub fn new() -> Self {
        let mgr = Self {
            stashed: Arc::new(DashMap::new()),
        };

        // Spawn background cleanup task
        let stashed = mgr.stashed.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let cutoff = Duration::from_secs(RECONNECT_WINDOW_SECS);
                stashed.retain(|_, v| v.disconnected_at.elapsed() < cutoff);
            }
        });

        mgr
    }

    /// Stash an agent's state on disconnect. Returns immediately.
    pub fn stash(
        &self,
        agent_id: String,
        name: String,
        api_key: String,
        rooms: HashSet<String>,
    ) {
        log::info!(
            "Stashing reconnect state for {} ({}) — {} rooms",
            name,
            agent_id,
            rooms.len()
        );
        self.stashed.insert(
            agent_id.clone(),
            StashedAgent {
                agent_id,
                name,
                api_key,
                rooms,
                missed_messages: Vec::new(),
                disconnected_at: Instant::now(),
            },
        );
    }

    /// Try to reclaim a stashed agent. Returns the stashed state if found and
    /// the reconnect window hasn't expired.
    pub fn reclaim(&self, agent_id: &str) -> Option<StashedAgent> {
        if let Some((_, stashed)) = self.stashed.remove(agent_id) {
            if stashed.disconnected_at.elapsed() < Duration::from_secs(RECONNECT_WINDOW_SECS) {
                log::info!(
                    "Agent {} ({}) reclaimed after {:.1}s",
                    stashed.name,
                    agent_id,
                    stashed.disconnected_at.elapsed().as_secs_f64()
                );
                return Some(stashed);
            }
            log::debug!("Stashed state for {} expired", agent_id);
        }
        None
    }

    /// Buffer a message for a disconnected agent (e.g. room messages they're missing).
    pub fn buffer_message(&self, agent_id: &str, frame: Frame) {
        if let Some(mut stashed) = self.stashed.get_mut(agent_id) {
            if stashed.missed_messages.len() < MAX_BUFFERED_MESSAGES {
                stashed.missed_messages.push(frame);
            }
        }
    }

    /// Check if an agent_id is stashed (recently disconnected, awaiting reconnect).
    pub fn is_stashed(&self, agent_id: &str) -> bool {
        self.stashed.contains_key(agent_id)
    }

    /// Get the set of stashed agent IDs that are members of a given room.
    pub fn stashed_members_of_room(&self, room_id: &str) -> Vec<String> {
        self.stashed
            .iter()
            .filter(|entry| entry.value().rooms.contains(room_id))
            .map(|entry| entry.key().clone())
            .collect()
    }
}
