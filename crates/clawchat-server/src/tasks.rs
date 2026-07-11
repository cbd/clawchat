use chrono::Utc;
use clawchat_core::TaskInfo;
use dashmap::DashMap;
use std::sync::Arc;

use crate::store::Store;

/// In-memory task tracking for rooms. Tasks are lightweight work items that
/// agents can assign, update, and query — filling the gap between unstructured
/// chat messages and formal votes.
pub struct TaskManager {
    /// task_id -> TaskInfo
    pub tasks: Arc<DashMap<String, TaskInfo>>,
    store: Arc<Store>,
}

impl TaskManager {
    pub fn new(store: Arc<Store>) -> Self {
        let tasks = Arc::new(DashMap::new());
        if let Ok(persisted) = store.load_tasks() {
            for task in persisted {
                tasks.insert(task.task_id.clone(), task);
            }
        }
        Self { tasks, store }
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
        if let Err(error) = self.store.save_task(&task) {
            log::warn!("task persistence failed: {error}");
        }
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

        let updated = task.clone();
        if let Err(error) = self.store.save_task(&updated) {
            log::warn!("task persistence failed: {error}");
        }
        Some(updated)
    }

    /// List tasks in a room, optionally filtered by status.
    pub fn list_tasks(&self, room_id: &str, status_filter: Option<&str>) -> Vec<TaskInfo> {
        self.tasks
            .iter()
            .filter(|entry| {
                let t = entry.value();
                t.room_id == room_id && status_filter.map(|s| t.status == s).unwrap_or(true)
            })
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Get a single task by ID.
    pub fn get_task(&self, task_id: &str) -> Option<TaskInfo> {
        self.tasks.get(task_id).map(|t| t.value().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_survive_manager_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("tasks.db")).unwrap());
        let manager = TaskManager::new(store.clone());
        manager.create_task(
            "task-1".into(),
            "lobby".into(),
            "Persist me".into(),
            None,
            Some("agent".into()),
            "creator".into(),
        );
        manager.update_task("task-1", Some("in_progress".into()), None, None);
        drop(manager);

        let restored = TaskManager::new(store);
        let task = restored.get_task("task-1").expect("task must reload");
        assert_eq!(task.status, "in_progress");
        assert_eq!(task.assignee.as_deref(), Some("agent"));
    }
}
