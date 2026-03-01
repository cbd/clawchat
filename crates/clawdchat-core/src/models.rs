use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub agent_id: String,
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    pub room_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub ephemeral: bool,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub message_id: String,
    pub room_id: String,
    pub agent_id: String,
    pub agent_name: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub timestamp: DateTime<Utc>,
}

// --- Command payloads (client -> server) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterPayload {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRoomPayload {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub ephemeral: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRoomPayload {
    pub room_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaveRoomPayload {
    pub room_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessagePayload {
    pub room_id: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub mentions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetHistoryPayload {
    pub room_id: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<DateTime<Utc>>,
}

fn default_limit() -> u32 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRoomsPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListAgentsPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomInfoPayload {
    pub room_id: String,
}

// --- Voting payloads ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateVotePayload {
    pub room_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub options: Vec<String>,
    /// Deadline in seconds from now. If None, vote stays open until all members vote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CastVotePayload {
    pub vote_id: String,
    /// Index into the options list (0-based).
    pub option_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetVoteStatusPayload {
    pub vote_id: String,
}

/// Summary of a vote (returned on creation and status queries).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteInfo {
    pub vote_id: String,
    pub room_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub options: Vec<String>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closes_at: Option<DateTime<Utc>>,
    pub status: VoteStatus,
    /// Number of ballots cast (not WHO voted or WHAT they voted).
    pub votes_cast: usize,
    /// Total eligible voters (room members at time of query).
    pub eligible_voters: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoteStatus {
    Open,
    Closed,
}

/// Revealed vote results, broadcast when vote closes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteResultPayload {
    pub vote_id: String,
    pub room_id: String,
    pub title: String,
    pub options: Vec<String>,
    /// Tally: option_index -> count.
    pub tally: Vec<VoteTally>,
    /// Individual ballots revealed.
    pub ballots: Vec<BallotEntry>,
    pub total_votes: usize,
    pub eligible_voters: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteTally {
    pub option_index: usize,
    pub option_text: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BallotEntry {
    pub agent_id: String,
    pub agent_name: String,
    pub option_index: usize,
}

// --- Election payloads ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElectLeaderPayload {
    pub room_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclineElectionPayload {
    pub room_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionPayload {
    pub room_id: String,
    pub content: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}
