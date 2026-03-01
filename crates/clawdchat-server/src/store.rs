use chrono::{DateTime, Utc};
use clawdchat_core::{ChatMessage, Room};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.initialize()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();

        // Step 1: Create tables (without new columns — old DBs may already have rooms table)
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS api_keys (
                api_key    TEXT PRIMARY KEY,
                tier       TEXT NOT NULL DEFAULT 'free',
                label      TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );

            CREATE TABLE IF NOT EXISTS rooms (
                room_id     TEXT PRIMARY KEY,
                name        TEXT NOT NULL UNIQUE,
                description TEXT,
                parent_id   TEXT REFERENCES rooms(room_id) ON DELETE SET NULL,
                created_by  TEXT,
                created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                visibility  TEXT NOT NULL DEFAULT 'private',
                owner_key   TEXT,
                CHECK (room_id != parent_id)
            );

            CREATE TABLE IF NOT EXISTS messages (
                message_id       TEXT PRIMARY KEY,
                room_id          TEXT NOT NULL REFERENCES rooms(room_id) ON DELETE CASCADE,
                agent_id         TEXT NOT NULL,
                agent_name       TEXT NOT NULL,
                content          TEXT NOT NULL,
                reply_to_message TEXT REFERENCES messages(message_id) ON DELETE SET NULL,
                metadata         TEXT DEFAULT '{}',
                created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );

            CREATE INDEX IF NOT EXISTS idx_messages_room_time
                ON messages(room_id, created_at DESC);

            CREATE INDEX IF NOT EXISTS idx_messages_reply
                ON messages(reply_to_message) WHERE reply_to_message IS NOT NULL;

            CREATE TABLE IF NOT EXISTS agent_sessions (
                session_id      TEXT PRIMARY KEY,
                agent_id        TEXT NOT NULL,
                agent_name      TEXT NOT NULL,
                capabilities    TEXT DEFAULT '[]',
                connected_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                disconnected_at TEXT
            );

            CREATE TABLE IF NOT EXISTS votes (
                vote_id      TEXT PRIMARY KEY,
                room_id      TEXT NOT NULL,
                title        TEXT NOT NULL,
                description  TEXT,
                options      TEXT NOT NULL DEFAULT '[]',
                created_by   TEXT NOT NULL,
                created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                closes_at    TEXT,
                status       TEXT NOT NULL DEFAULT 'open'
            );

            CREATE TABLE IF NOT EXISTS vote_ballots (
                vote_id      TEXT NOT NULL REFERENCES votes(vote_id) ON DELETE CASCADE,
                agent_id     TEXT NOT NULL,
                agent_name   TEXT NOT NULL,
                option_index INTEGER NOT NULL,
                cast_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                PRIMARY KEY (vote_id, agent_id)
            );
            ",
        )?;

        // Step 2: Run migrations (add columns to tables that may have been created by older versions)
        Self::migrate_add_column(&conn, "rooms", "visibility", "TEXT NOT NULL DEFAULT 'private'");
        Self::migrate_add_column(&conn, "rooms", "owner_key", "TEXT");

        // Step 3: Seed data (runs after migrations so visibility column is guaranteed to exist)
        conn.execute_batch(
            "INSERT OR IGNORE INTO rooms (room_id, name, description, visibility)
                VALUES ('lobby', 'lobby', 'Default room for all agents', 'public');",
        )?;

        // Ensure lobby is public (may have been created before visibility existed)
        conn.execute(
            "UPDATE rooms SET visibility = 'public' WHERE room_id = 'lobby' AND visibility = 'private'",
            [],
        )?;

        Ok(())
    }

    /// Try to add a column to a table; silently ignore if it already exists.
    fn migrate_add_column(conn: &Connection, table: &str, column: &str, col_type: &str) {
        let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, col_type);
        if let Err(e) = conn.execute_batch(&sql) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                log::debug!("Migration {}.{}: {}", table, column, msg);
            }
        }
    }

    // --- Room operations ---

    pub fn create_room(
        &self,
        room_id: &str,
        name: &str,
        description: Option<&str>,
        parent_id: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<Room, StoreError> {
        self.create_room_with_visibility(room_id, name, description, parent_id, created_by, "private", None)
    }

    pub fn create_room_with_visibility(
        &self,
        room_id: &str,
        name: &str,
        description: Option<&str>,
        parent_id: Option<&str>,
        created_by: Option<&str>,
        visibility: &str,
        owner_key: Option<&str>,
    ) -> Result<Room, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO rooms (room_id, name, description, parent_id, created_by, visibility, owner_key) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![room_id, name, description, parent_id, created_by, visibility, owner_key],
        ).map_err(|e| match e {
            rusqlite::Error::SqliteFailure(err, _) if err.extended_code == 2067 => {
                StoreError::RoomNameTaken(name.to_string())
            }
            other => StoreError::Db(other),
        })?;

        // Query the created room inline (avoid deadlock from calling self.get_room)
        query_room_by_id(&conn, room_id)?
            .ok_or_else(|| StoreError::Db(rusqlite::Error::QueryReturnedNoRows))
    }

    pub fn get_room(&self, room_id: &str) -> Result<Option<Room>, StoreError> {
        let conn = self.conn.lock().unwrap();
        query_room_by_id(&conn, room_id)
    }

    pub fn get_room_by_name(&self, name: &str) -> Result<Option<Room>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key FROM rooms WHERE name = ?1",
        )?;

        let room = stmt.query_row(params![name], map_room_row);

        match room {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StoreError::Db(e)),
        }
    }

    pub fn list_rooms(&self, parent_id: Option<&str>) -> Result<Vec<Room>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut rooms = Vec::new();

        match parent_id {
            Some(pid) => {
                let mut stmt = conn.prepare(
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key FROM rooms WHERE parent_id = ?1 ORDER BY name",
                )?;
                let rows = stmt.query_map(params![pid], map_room_row)?;
                for row in rows {
                    rooms.push(row?);
                }
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key FROM rooms ORDER BY name",
                )?;
                let rows = stmt.query_map([], map_room_row)?;
                for row in rows {
                    rooms.push(row?);
                }
            }
        }

        Ok(rooms)
    }

    pub fn delete_room(&self, room_id: &str) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute("DELETE FROM rooms WHERE room_id = ?1", params![room_id])?;
        Ok(affected > 0)
    }

    // --- Message operations ---

    pub fn insert_message(
        &self,
        message_id: &str,
        room_id: &str,
        agent_id: &str,
        agent_name: &str,
        content: &str,
        reply_to_message: Option<&str>,
        metadata: &serde_json::Value,
    ) -> Result<ChatMessage, StoreError> {
        let conn = self.conn.lock().unwrap();
        let metadata_str = serde_json::to_string(metadata).unwrap_or_default();

        conn.execute(
            "INSERT INTO messages (message_id, room_id, agent_id, agent_name, content, reply_to_message, metadata) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![message_id, room_id, agent_id, agent_name, content, reply_to_message, metadata_str],
        )?;

        let created_at: String = conn.query_row(
            "SELECT created_at FROM messages WHERE message_id = ?1",
            params![message_id],
            |row| row.get(0),
        )?;

        Ok(ChatMessage {
            message_id: message_id.to_string(),
            room_id: room_id.to_string(),
            agent_id: agent_id.to_string(),
            agent_name: agent_name.to_string(),
            content: content.to_string(),
            reply_to_message: reply_to_message.map(String::from),
            metadata: metadata.clone(),
            timestamp: parse_timestamp(&created_at),
        })
    }

    pub fn get_history(
        &self,
        room_id: &str,
        limit: u32,
        before: Option<DateTime<Utc>>,
    ) -> Result<Vec<ChatMessage>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut messages = Vec::new();

        match before {
            Some(before_ts) => {
                let ts_str = before_ts.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
                let mut stmt = conn.prepare(
                    "SELECT message_id, room_id, agent_id, agent_name, content, reply_to_message, metadata, created_at
                     FROM messages WHERE room_id = ?1 AND created_at < ?2
                     ORDER BY created_at DESC, rowid DESC LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![room_id, ts_str, limit], map_message_row)?;
                for row in rows {
                    messages.push(row?);
                }
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT message_id, room_id, agent_id, agent_name, content, reply_to_message, metadata, created_at
                     FROM messages WHERE room_id = ?1
                     ORDER BY created_at DESC, rowid DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![room_id, limit], map_message_row)?;
                for row in rows {
                    messages.push(row?);
                }
            }
        }

        // Return in chronological order
        messages.reverse();
        Ok(messages)
    }

    // --- Agent session tracking ---

    pub fn record_session_start(
        &self,
        session_id: &str,
        agent_id: &str,
        agent_name: &str,
        capabilities: &[String],
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let caps_json = serde_json::to_string(capabilities).unwrap_or_default();
        conn.execute(
            "INSERT INTO agent_sessions (session_id, agent_id, agent_name, capabilities) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, agent_id, agent_name, caps_json],
        )?;
        Ok(())
    }

    pub fn record_session_end(&self, session_id: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE agent_sessions SET disconnected_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    // --- Vote operations ---

    pub fn create_vote(
        &self,
        vote_id: &str,
        room_id: &str,
        title: &str,
        description: Option<&str>,
        options: &[String],
        created_by: &str,
        closes_at: Option<DateTime<Utc>>,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let options_json = serde_json::to_string(options).unwrap_or_default();
        let closes_str = closes_at.map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
        conn.execute(
            "INSERT INTO votes (vote_id, room_id, title, description, options, created_by, closes_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![vote_id, room_id, title, description, options_json, created_by, closes_str],
        )?;
        Ok(())
    }

    pub fn cast_vote(
        &self,
        vote_id: &str,
        agent_id: &str,
        agent_name: &str,
        option_index: usize,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();

        // Check vote is open
        let status: String = conn
            .query_row(
                "SELECT status FROM votes WHERE vote_id = ?1",
                params![vote_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::VoteNotFound,
                other => StoreError::Db(other),
            })?;

        if status != "open" {
            return Err(StoreError::VoteClosed);
        }

        conn.execute(
            "INSERT INTO vote_ballots (vote_id, agent_id, agent_name, option_index) VALUES (?1, ?2, ?3, ?4)",
            params![vote_id, agent_id, agent_name, option_index as i64],
        ).map_err(|e| match e {
            rusqlite::Error::SqliteFailure(err, _) if err.extended_code == 1555 => {
                StoreError::AlreadyVoted
            }
            other => StoreError::Db(other),
        })?;

        Ok(())
    }

    pub fn get_vote_ballot_count(&self, vote_id: &str) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM vote_ballots WHERE vote_id = ?1",
            params![vote_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn get_vote_ballots(
        &self,
        vote_id: &str,
    ) -> Result<Vec<(String, String, usize)>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT agent_id, agent_name, option_index FROM vote_ballots WHERE vote_id = ?1 ORDER BY cast_at",
        )?;
        let rows = stmt.query_map(params![vote_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as usize,
            ))
        })?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub fn close_vote(&self, vote_id: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE votes SET status = 'closed' WHERE vote_id = ?1",
            params![vote_id],
        )?;
        Ok(())
    }

    // --- API key operations ---

    pub fn create_api_key(&self, api_key: &str, label: Option<&str>) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO api_keys (api_key, label) VALUES (?1, ?2)",
            params![api_key, label],
        )?;
        Ok(())
    }

    pub fn validate_api_key(&self, api_key: &str) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM api_keys WHERE api_key = ?1",
            params![api_key],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn get_key_tier(&self, api_key: &str) -> Result<String, StoreError> {
        let conn = self.conn.lock().unwrap();
        let tier: String = conn.query_row(
            "SELECT tier FROM api_keys WHERE api_key = ?1",
            params![api_key],
            |row| row.get(0),
        ).map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::Db(e),
            other => StoreError::Db(other),
        })?;
        Ok(tier)
    }

    pub fn count_rooms_for_key(&self, api_key: &str) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM rooms WHERE owner_key = ?1",
            params![api_key],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// List rooms visible to the given API key: all public rooms + private rooms owned by the key.
    pub fn list_rooms_for_key(
        &self,
        api_key: Option<&str>,
        parent_id: Option<&str>,
    ) -> Result<Vec<Room>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut rooms = Vec::new();

        match (parent_id, api_key) {
            (Some(pid), Some(key)) => {
                let mut stmt = conn.prepare(
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key
                     FROM rooms WHERE parent_id = ?1 AND (visibility = 'public' OR owner_key = ?2) ORDER BY name",
                )?;
                let rows = stmt.query_map(params![pid, key], map_room_row)?;
                for row in rows { rooms.push(row?); }
            }
            (Some(pid), None) => {
                let mut stmt = conn.prepare(
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key
                     FROM rooms WHERE parent_id = ?1 AND visibility = 'public' ORDER BY name",
                )?;
                let rows = stmt.query_map(params![pid], map_room_row)?;
                for row in rows { rooms.push(row?); }
            }
            (None, Some(key)) => {
                let mut stmt = conn.prepare(
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key
                     FROM rooms WHERE visibility = 'public' OR owner_key = ?1 ORDER BY name",
                )?;
                let rows = stmt.query_map(params![key], map_room_row)?;
                for row in rows { rooms.push(row?); }
            }
            (None, None) => {
                let mut stmt = conn.prepare(
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key
                     FROM rooms WHERE visibility = 'public' ORDER BY name",
                )?;
                let rows = stmt.query_map([], map_room_row)?;
                for row in rows { rooms.push(row?); }
            }
        }

        Ok(rooms)
    }

    // --- Vote operations (continued) ---

    pub fn get_vote_meta(
        &self,
        vote_id: &str,
    ) -> Result<Option<(String, String, Option<String>, Vec<String>, String, DateTime<Utc>, Option<DateTime<Utc>>, String)>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT room_id, title, description, options, created_by, created_at, closes_at, status FROM votes WHERE vote_id = ?1",
            params![vote_id],
            |row| {
                let options_str: String = row.get(3)?;
                let closes_str: Option<String> = row.get(6)?;
                let created_str: String = row.get(5)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    serde_json::from_str(&options_str).unwrap_or_default(),
                    row.get::<_, String>(4)?,
                    parse_timestamp(&created_str),
                    closes_str.map(|s| parse_timestamp(&s)),
                    row.get::<_, String>(7)?,
                ))
            },
        );

        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StoreError::Db(e)),
        }
    }
}

// --- Internal helpers that take an already-locked connection ---

fn query_room_by_id(conn: &Connection, room_id: &str) -> Result<Option<Room>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key FROM rooms WHERE room_id = ?1",
    )?;

    let room = stmt.query_row(params![room_id], map_room_row);

    match room {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(StoreError::Db(e)),
    }
}

fn map_room_row(row: &rusqlite::Row) -> rusqlite::Result<Room> {
    Ok(Room {
        room_id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        parent_id: row.get(3)?,
        ephemeral: false,
        created_at: parse_timestamp(&row.get::<_, String>(5)?),
        created_by: row.get(4)?,
        visibility: row.get::<_, String>(6).unwrap_or_else(|_| "private".to_string()),
        owner_key: row.get(7)?,
    })
}

fn map_message_row(row: &rusqlite::Row) -> rusqlite::Result<ChatMessage> {
    let metadata_str: String = row.get(6)?;
    let metadata: serde_json::Value =
        serde_json::from_str(&metadata_str).unwrap_or(serde_json::json!({}));
    let ts_str: String = row.get(7)?;

    Ok(ChatMessage {
        message_id: row.get(0)?,
        room_id: row.get(1)?,
        agent_id: row.get(2)?,
        agent_name: row.get(3)?,
        content: row.get(4)?,
        reply_to_message: row.get(5)?,
        metadata,
        timestamp: parse_timestamp(&ts_str),
    })
}

fn parse_timestamp(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.fZ")
                .map(|ndt| ndt.and_utc())
        })
        .unwrap_or_else(|_| Utc::now())
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("room name already taken: {0}")]
    RoomNameTaken(String),

    #[error("vote not found")]
    VoteNotFound,

    #[error("vote is closed")]
    VoteClosed,

    #[error("already voted")]
    AlreadyVoted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initialize_creates_lobby() {
        let store = Store::open_in_memory().unwrap();
        let room = store.get_room("lobby").unwrap();
        assert!(room.is_some());
        assert_eq!(room.unwrap().name, "lobby");
    }

    #[test]
    fn test_create_and_get_room() {
        let store = Store::open_in_memory().unwrap();
        let room = store
            .create_room("test-room", "test-room", Some("A test"), None, Some("agent-1"))
            .unwrap();
        assert_eq!(room.name, "test-room");
        assert_eq!(room.description, Some("A test".into()));

        let fetched = store.get_room("test-room").unwrap().unwrap();
        assert_eq!(fetched.room_id, "test-room");
    }

    #[test]
    fn test_duplicate_room_name() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_room("r1", "same-name", None, None, None)
            .unwrap();
        let result = store.create_room("r2", "same-name", None, None, None);
        assert!(matches!(result, Err(StoreError::RoomNameTaken(_))));
    }

    #[test]
    fn test_insert_and_get_history() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_message(
                "msg-1",
                "lobby",
                "agent-1",
                "Alice",
                "Hello",
                None,
                &serde_json::json!({}),
            )
            .unwrap();
        store
            .insert_message(
                "msg-2",
                "lobby",
                "agent-2",
                "Bob",
                "Hi there",
                Some("msg-1"),
                &serde_json::json!({}),
            )
            .unwrap();

        let history = store.get_history("lobby", 50, None).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "Hello");
        assert_eq!(history[1].content, "Hi there");
        assert_eq!(history[1].reply_to_message, Some("msg-1".into()));
    }

    #[test]
    fn test_list_rooms_with_parent() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_room("parent", "parent-room", None, None, None)
            .unwrap();
        store
            .create_room("child-1", "child-1", None, Some("parent"), None)
            .unwrap();
        store
            .create_room("child-2", "child-2", None, Some("parent"), None)
            .unwrap();

        let children = store.list_rooms(Some("parent")).unwrap();
        assert_eq!(children.len(), 2);

        let all = store.list_rooms(None).unwrap();
        assert!(all.len() >= 4); // lobby + parent + 2 children
    }
}
