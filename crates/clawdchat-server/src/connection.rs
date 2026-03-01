use clawdchat_core::{AgentInfo, Frame};
use std::collections::HashSet;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Represents a connected agent's server-side state.
pub struct AgentConnection {
    pub info: AgentInfo,
    pub session_id: String,
    pub sender: mpsc::UnboundedSender<Frame>,
    pub send_task: JoinHandle<()>,
    pub receive_task: JoinHandle<()>,
    pub rooms: HashSet<String>,
}

impl AgentConnection {
    pub fn new(
        info: AgentInfo,
        session_id: String,
        sender: mpsc::UnboundedSender<Frame>,
        send_task: JoinHandle<()>,
        receive_task: JoinHandle<()>,
    ) -> Self {
        Self {
            info,
            session_id,
            sender,
            send_task,
            receive_task,
            rooms: HashSet::new(),
        }
    }
}

impl Drop for AgentConnection {
    fn drop(&mut self) {
        self.send_task.abort();
        self.receive_task.abort();
    }
}
