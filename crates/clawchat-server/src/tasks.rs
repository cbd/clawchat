use chrono::Utc;
use clawchat_core::TaskInfo;
use dashmap::DashMap;
use std::sync::Arc;

/// In-memory task tracking for rooms. Tasks are lightweight work items that
/// agents can assign, update, and query — filling the gap between unstructured
/// chat messages and formal votes.
pub struct TaskManager {
    /// task_id -> TaskInfo
    pub tasks: Arc<DashMap<String, TaskInfo>>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(DashMap::new()),
        }
    }

    /// Create a new task in a room.
    pub fn create_task(
        &self,
        task_id: String,
        room_id: String,
        title: String,
        description: Option<String>,
        assignee: Option<String>,
        created_by: String,
    ) -> TaskInfo {
        let now = Utc::now();
        let task = TaskInfo {
            task_id: task_id.clone(),
            room_id,
            title,
            description,
            status: "pending".to_string(),
            assignee,
            created_by,
            created_at: now,
            updated_at: None,
            note: None,
        };
        self.tasks.insert(task_id, task.clone());
        task
    }

    /// Update a task's status, assignee, and/or note. Returns the updated task.
    pub fn update_task(
        &self,
        task_id: &str,
        status: Option<String>,
        assignee: Option<String>,
        note: Option<String>,
    ) -> Option<TaskInfo> {
        let mut entry = self.tasks.get_mut(task_id)?;
        let task = entry.value_mut();

        if let Some(s) = status {
            task.status = s;
        }
        if let Some(a) = assignee {
            task.assignee = Some(a);
        }
        if let Some(n) = note {
            task.note = Some(n);
        }
        task.updated_at = Some(Utc::now());

        Some(task.clone())
    }

    /// List tasks in a room, optionally filtered by status.
    pub fn list_tasks(&self, room_id: &str, status_filter: Option<&str>) -> Vec<TaskInfo> {
        self.tasks
            .iter()
            .filter(|entry| {
                let t = entry.value();
                t.room_id == room_id
                    && status_filter
                        .map(|s| t.status == s)
                        .unwrap_or(true)
            })
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Get a single task by ID.
    pub fn get_task(&self, task_id: &str) -> Option<TaskInfo> {
        self.tasks.get(task_id).map(|t| t.value().clone())
    }
}
