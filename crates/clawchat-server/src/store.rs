use chrono::{DateTime, Utc};
use clawchat_core::{ChatMessage, Room};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct VoteMeta {
    pub vote_id: String,
    pub room_id: String,
    pub title: String,
    pub description: Option<String>,
    pub options: Vec<String>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub closes_at: Option<DateTime<Utc>>,
    pub status: String,
    pub eligible_voters: usize,
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
                encrypted   INTEGER NOT NULL DEFAULT 0,
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
                created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                seq              INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_messages_room_time
                ON messages(room_id, created_at DESC);

            CREATE INDEX IF NOT EXISTS idx_messages_reply
                ON messages(reply_to_message) WHERE reply_to_message IS NOT NULL;
            -- idx_messages_room_seq is created in Step 2, after the seq migration runs,
            -- because old DBs may not have the seq column at this point.

            CREATE TABLE IF NOT EXISTS agent_sessions (
                session_id      TEXT PRIMARY KEY,
                agent_id        TEXT NOT NULL,
                agent_name      TEXT NOT NULL,
                capabilities    TEXT DEFAULT '[]',
                connected_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                disconnected_at TEXT
            );

            CREATE TABLE IF NOT EXISTS votes (
                vote_id         TEXT PRIMARY KEY,
                room_id         TEXT NOT NULL,
                title           TEXT NOT NULL,
                description     TEXT,
                options         TEXT NOT NULL DEFAULT '[]',
                created_by      TEXT NOT NULL,
                created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                closes_at       TEXT,
                status          TEXT NOT NULL DEFAULT 'open',
                eligible_voters INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS vote_ballots (
                vote_id      TEXT NOT NULL REFERENCES votes(vote_id) ON DELETE CASCADE,
                agent_id     TEXT NOT NULL,
                agent_name   TEXT NOT NULL,
                option_index INTEGER NOT NULL,
                cast_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                PRIMARY KEY (vote_id, agent_id)
            );

            CREATE TABLE IF NOT EXISTS subscriptions (
                subscription_id     TEXT PRIMARY KEY,
                room_id             TEXT NOT NULL,
                owner_key           TEXT NOT NULL,
                webhook_url         TEXT NOT NULL,
                secret              TEXT NOT NULL,
                kinds               TEXT,         -- JSON array; NULL = match all kinds
                only_from           TEXT,
                not_from            TEXT,
                exclude_thinking    INTEGER NOT NULL DEFAULT 0,
                since_seq           INTEGER NOT NULL DEFAULT 0,
                last_delivered_seq  INTEGER NOT NULL DEFAULT 0,
                status              TEXT NOT NULL DEFAULT 'active',  -- active | failed | disabled
                failure_count       INTEGER NOT NULL DEFAULT 0,
                created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            CREATE INDEX IF NOT EXISTS idx_subscriptions_room ON subscriptions(room_id);
            CREATE INDEX IF NOT EXISTS idx_subscriptions_status ON subscriptions(status);

            CREATE TABLE IF NOT EXISTS subscription_deliveries (
                delivery_id      TEXT PRIMARY KEY,
                subscription_id  TEXT NOT NULL REFERENCES subscriptions(subscription_id) ON DELETE CASCADE,
                message_seq      INTEGER NOT NULL,
                message_id       TEXT NOT NULL,
                next_attempt_at  TEXT NOT NULL,
                attempts         INTEGER NOT NULL DEFAULT 0,
                last_error       TEXT,
                UNIQUE(subscription_id, message_seq)
            );
            CREATE INDEX IF NOT EXISTS idx_deliveries_pending ON subscription_deliveries(next_attempt_at);
            ",
        )?;

        // Step 2: Run migrations (add columns to tables that may have been created by older versions)
        Self::migrate_add_column(&conn, "rooms", "visibility", "TEXT NOT NULL DEFAULT 'private'");
        Self::migrate_add_column(&conn, "rooms", "owner_key", "TEXT");
        Self::migrate_add_column(&conn, "rooms", "encrypted", "INTEGER NOT NULL DEFAULT 0");
        ensure_column_exists(&conn, "votes", "eligible_voters", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column_exists(&conn, "messages", "seq", "INTEGER NOT NULL DEFAULT 0")?;
        // Backfill seq for any existing rows that predate the column. rowid order
        // approximates insertion order, so this gives stable per-room seqs.
        backfill_message_seq(&conn)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_messages_room_seq ON messages(room_id, seq);",
        )?;

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
        self.create_room_with_visibility(room_id, name, description, parent_id, created_by, "private", None, false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_room_with_visibility(
        &self,
        room_id: &str,
        name: &str,
        description: Option<&str>,
        parent_id: Option<&str>,
        created_by: Option<&str>,
        visibility: &str,
        owner_key: Option<&str>,
        encrypted: bool,
    ) -> Result<Room, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO rooms (room_id, name, description, parent_id, created_by, visibility, owner_key, encrypted) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![room_id, name, description, parent_id, created_by, visibility, owner_key, encrypted],
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
            "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key, encrypted FROM rooms WHERE name = ?1",
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
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key, encrypted FROM rooms WHERE parent_id = ?1 ORDER BY name",
                )?;
                let rows = stmt.query_map(params![pid], map_room_row)?;
                for row in rows {
                    rooms.push(row?);
                }
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key, encrypted FROM rooms ORDER BY name",
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

        // The seq subquery + INSERT runs as a single SQLite statement under a write
        // lock, so concurrent inserts in the same room can't collide on seq.
        conn.execute(
            "INSERT INTO messages (message_id, room_id, agent_id, agent_name, content, reply_to_message, metadata, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                 COALESCE((SELECT MAX(seq) FROM messages WHERE room_id = ?2), 0) + 1)",
            params![message_id, room_id, agent_id, agent_name, content, reply_to_message, metadata_str],
        )?;

        let (created_at, seq): (String, i64) = conn.query_row(
            "SELECT created_at, seq FROM messages WHERE message_id = ?1",
            params![message_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
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
            seq,
        })
    }

    /// Returns the latest seq for a room, or 0 if the room has no persisted messages.
    pub fn room_tip(&self, room_id: &str) -> Result<i64, StoreError> {
        let conn = self.conn.lock().unwrap();
        let seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM messages WHERE room_id = ?1",
            params![room_id],
            |row| row.get(0),
        )?;
        Ok(seq)
    }

    pub fn get_history(
        &self,
        room_id: &str,
        limit: u32,
        before: Option<DateTime<Utc>>,
    ) -> Result<Vec<ChatMessage>, StoreError> {
        self.get_history_filtered(room_id, limit, before, None, None)
    }

    /// Backwards-compatible wrapper for the message_id-based `since` filter.
    pub fn get_history_since(
        &self,
        room_id: &str,
        limit: u32,
        before: Option<DateTime<Utc>>,
        since: Option<&str>,
    ) -> Result<Vec<ChatMessage>, StoreError> {
        self.get_history_filtered(room_id, limit, before, since, None)
    }

    /// Get message history with optional filters.
    /// `since_seq` takes precedence over `since` (message_id), which takes precedence over `before`.
    /// All ASC-ordered queries return chronological order; `before` returns the latest N before
    /// the cutoff, also chronologically ordered.
    pub fn get_history_filtered(
        &self,
        room_id: &str,
        limit: u32,
        before: Option<DateTime<Utc>>,
        since: Option<&str>,
        since_seq: Option<i64>,
    ) -> Result<Vec<ChatMessage>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut messages = Vec::new();

        if let Some(seq_floor) = since_seq {
            let mut stmt = conn.prepare(
                "SELECT message_id, room_id, agent_id, agent_name, content, reply_to_message, metadata, created_at, seq
                 FROM messages WHERE room_id = ?1 AND seq > ?2
                 ORDER BY seq ASC LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![room_id, seq_floor, limit], map_message_row)?;
            for row in rows {
                messages.push(row?);
            }
            return Ok(messages);
        }

        if let Some(since_id) = since {
            // Get the rowid of the since message, then return messages after it
            let mut stmt = conn.prepare(
                "SELECT message_id, room_id, agent_id, agent_name, content, reply_to_message, metadata, created_at, seq
                 FROM messages WHERE room_id = ?1 AND rowid > (
                     SELECT rowid FROM messages WHERE message_id = ?2
                 )
                 ORDER BY created_at ASC, rowid ASC LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![room_id, since_id, limit], map_message_row)?;
            for row in rows {
                messages.push(row?);
            }
            // Already in chronological order
            return Ok(messages);
        }

        match before {
            Some(before_ts) => {
                let ts_str = before_ts.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
                let mut stmt = conn.prepare(
                    "SELECT message_id, room_id, agent_id, agent_name, content, reply_to_message, metadata, created_at, seq
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
                    "SELECT message_id, room_id, agent_id, agent_name, content, reply_to_message, metadata, created_at, seq
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
        eligible_voters: usize,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let options_json = serde_json::to_string(options).unwrap_or_default();
        let closes_str = closes_at.map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
        conn.execute(
            "INSERT INTO votes (vote_id, room_id, title, description, options, created_by, closes_at, eligible_voters) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![vote_id, room_id, title, description, options_json, created_by, closes_str, eligible_voters as i64],
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
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key, encrypted
                     FROM rooms WHERE parent_id = ?1 AND (visibility = 'public' OR owner_key = ?2) ORDER BY name",
                )?;
                let rows = stmt.query_map(params![pid, key], map_room_row)?;
                for row in rows { rooms.push(row?); }
            }
            (Some(pid), None) => {
                let mut stmt = conn.prepare(
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key, encrypted
                     FROM rooms WHERE parent_id = ?1 AND visibility = 'public' ORDER BY name",
                )?;
                let rows = stmt.query_map(params![pid], map_room_row)?;
                for row in rows { rooms.push(row?); }
            }
            (None, Some(key)) => {
                let mut stmt = conn.prepare(
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key, encrypted
                     FROM rooms WHERE visibility = 'public' OR owner_key = ?1 ORDER BY name",
                )?;
                let rows = stmt.query_map(params![key], map_room_row)?;
                for row in rows { rooms.push(row?); }
            }
            (None, None) => {
                let mut stmt = conn.prepare(
                    "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key, encrypted
                     FROM rooms WHERE visibility = 'public' ORDER BY name",
                )?;
                let rows = stmt.query_map([], map_room_row)?;
                for row in rows { rooms.push(row?); }
            }
        }

        Ok(rooms)
    }

    // --- Vote operations (continued) ---

    pub fn get_vote_meta(&self, vote_id: &str) -> Result<Option<VoteMeta>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT vote_id, room_id, title, description, options, created_by, created_at, closes_at, status, eligible_voters FROM votes WHERE vote_id = ?1",
            params![vote_id],
            map_vote_meta_row,
        );

        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StoreError::Db(e)),
        }
    }

    pub fn list_votes(&self, room_id: &str, limit: u32) -> Result<Vec<VoteMeta>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT vote_id, room_id, title, description, options, created_by, created_at, closes_at, status, eligible_voters
             FROM votes
             WHERE room_id = ?1
             ORDER BY created_at DESC, vote_id DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![room_id, limit as i64], map_vote_meta_row)?;
        let mut votes = Vec::new();
        for row in rows {
            votes.push(row?);
        }
        Ok(votes)
    }

    // --- Subscription operations ---

    /// Insert a new subscription. `subscription_id` and `created_at` are caller-provided
    /// so we keep insert/read symmetric; callers should generate a UUID and `Utc::now()`.
    #[allow(clippy::too_many_arguments)]
    pub fn create_subscription(
        &self,
        subscription_id: &str,
        room_id: &str,
        owner_key: &str,
        webhook_url: &str,
        secret: &str,
        kinds: &[String],
        only_from: Option<&str>,
        not_from: Option<&str>,
        exclude_thinking: bool,
        since_seq: i64,
    ) -> Result<clawchat_core::Subscription, StoreError> {
        let conn = self.conn.lock().unwrap();
        let kinds_json = if kinds.is_empty() {
            None
        } else {
            Some(serde_json::to_string(kinds).unwrap_or_else(|_| "[]".to_string()))
        };
        let now = chrono::Utc::now();
        let now_str = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        conn.execute(
            "INSERT INTO subscriptions
             (subscription_id, room_id, owner_key, webhook_url, secret, kinds,
              only_from, not_from, exclude_thinking, since_seq, last_delivered_seq,
              status, failure_count, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, 'active', 0, ?11)",
            params![
                subscription_id,
                room_id,
                owner_key,
                webhook_url,
                secret,
                kinds_json,
                only_from,
                not_from,
                exclude_thinking as i64,
                since_seq,
                now_str,
            ],
        )?;
        Ok(clawchat_core::Subscription {
            subscription_id: subscription_id.to_string(),
            room_id: room_id.to_string(),
            webhook_url: webhook_url.to_string(),
            kinds: kinds.to_vec(),
            only_from: only_from.map(String::from),
            not_from: not_from.map(String::from),
            exclude_thinking,
            since_seq,
            last_delivered_seq: since_seq,
            status: "active".to_string(),
            failure_count: 0,
            created_at: now,
        })
    }

    pub fn get_subscription(
        &self,
        subscription_id: &str,
    ) -> Result<Option<(clawchat_core::Subscription, String, String)>, StoreError> {
        // Returns (sub, owner_key, secret) — secret is needed by the delivery worker
        // but not exposed to clients.
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT subscription_id, room_id, owner_key, webhook_url, secret, kinds,
                    only_from, not_from, exclude_thinking, since_seq, last_delivered_seq,
                    status, failure_count, created_at
             FROM subscriptions WHERE subscription_id = ?1",
        )?;
        let mut rows = stmt.query(params![subscription_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(map_subscription_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_subscriptions(
        &self,
        owner_key: &str,
        room_id: Option<&str>,
    ) -> Result<Vec<clawchat_core::Subscription>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let (sql, with_room) = match room_id {
            Some(_) => (
                "SELECT subscription_id, room_id, owner_key, webhook_url, secret, kinds,
                        only_from, not_from, exclude_thinking, since_seq, last_delivered_seq,
                        status, failure_count, created_at
                 FROM subscriptions
                 WHERE owner_key = ?1 AND room_id = ?2
                 ORDER BY created_at DESC",
                true,
            ),
            None => (
                "SELECT subscription_id, room_id, owner_key, webhook_url, secret, kinds,
                        only_from, not_from, exclude_thinking, since_seq, last_delivered_seq,
                        status, failure_count, created_at
                 FROM subscriptions
                 WHERE owner_key = ?1
                 ORDER BY created_at DESC",
                false,
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = if with_room {
            stmt.query(params![owner_key, room_id.unwrap()])?
        } else {
            stmt.query(params![owner_key])?
        };
        let mut out = Vec::new();
        let mut rows = rows;
        while let Some(row) = rows.next()? {
            let (sub, _owner, _secret) = map_subscription_row(row)?;
            out.push(sub);
        }
        Ok(out)
    }

    /// Return ALL active subscriptions for a room (used by the delivery enqueue path).
    pub fn list_active_subscriptions_for_room(
        &self,
        room_id: &str,
    ) -> Result<Vec<(clawchat_core::Subscription, String, String)>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT subscription_id, room_id, owner_key, webhook_url, secret, kinds,
                    only_from, not_from, exclude_thinking, since_seq, last_delivered_seq,
                    status, failure_count, created_at
             FROM subscriptions WHERE room_id = ?1 AND status = 'active'",
        )?;
        let mut rows = stmt.query(params![room_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(map_subscription_row(row)?);
        }
        Ok(out)
    }

    pub fn delete_subscription(
        &self,
        subscription_id: &str,
        owner_key: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM subscriptions WHERE subscription_id = ?1 AND owner_key = ?2",
            params![subscription_id, owner_key],
        )?;
        Ok(n > 0)
    }

    pub fn set_subscription_status(
        &self,
        subscription_id: &str,
        status: &str,
        failure_count: Option<i64>,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        match failure_count {
            Some(n) => {
                conn.execute(
                    "UPDATE subscriptions SET status = ?1, failure_count = ?2 WHERE subscription_id = ?3",
                    params![status, n, subscription_id],
                )?;
            }
            None => {
                conn.execute(
                    "UPDATE subscriptions SET status = ?1 WHERE subscription_id = ?2",
                    params![status, subscription_id],
                )?;
            }
        }
        Ok(())
    }

    pub fn advance_subscription_cursor(
        &self,
        subscription_id: &str,
        new_last_delivered_seq: i64,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE subscriptions SET last_delivered_seq = ?1, failure_count = 0
             WHERE subscription_id = ?2 AND ?1 > last_delivered_seq",
            params![new_last_delivered_seq, subscription_id],
        )?;
        Ok(())
    }

    // --- Subscription delivery queue ---

    pub fn enqueue_delivery(
        &self,
        delivery_id: &str,
        subscription_id: &str,
        message_seq: i64,
        message_id: &str,
        next_attempt_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, StoreError> {
        // Returns true if a new row was inserted, false if a duplicate (sub, seq)
        // was rejected by the UNIQUE constraint (idempotent enqueue).
        let conn = self.conn.lock().unwrap();
        let ts = next_attempt_at
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        match conn.execute(
            "INSERT INTO subscription_deliveries
             (delivery_id, subscription_id, message_seq, message_id, next_attempt_at, attempts)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![delivery_id, subscription_id, message_seq, message_id, ts],
        ) {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Load pending deliveries due to fire by `now`. Limit caps batch size.
    pub fn load_due_deliveries(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        limit: usize,
    ) -> Result<Vec<PendingDelivery>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let ts = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let mut stmt = conn.prepare(
            "SELECT delivery_id, subscription_id, message_seq, message_id, attempts
             FROM subscription_deliveries
             WHERE next_attempt_at <= ?1
             ORDER BY next_attempt_at ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![ts, limit as i64], |row| {
            Ok(PendingDelivery {
                delivery_id: row.get(0)?,
                subscription_id: row.get(1)?,
                message_seq: row.get(2)?,
                message_id: row.get(3)?,
                attempts: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Earliest `next_attempt_at` of any pending delivery, or None if the
    /// queue is empty. The delivery worker uses this to sleep precisely until
    /// the next retry instead of polling.
    pub fn earliest_pending_attempt(
        &self,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT next_attempt_at FROM subscription_deliveries
             ORDER BY next_attempt_at ASC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let s: String = row.get(0)?;
            let dt = chrono::DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .ok();
            Ok(dt)
        } else {
            Ok(None)
        }
    }

    pub fn delete_delivery(&self, delivery_id: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM subscription_deliveries WHERE delivery_id = ?1",
            params![delivery_id],
        )?;
        Ok(())
    }

    pub fn reschedule_delivery(
        &self,
        delivery_id: &str,
        next_attempt_at: chrono::DateTime<chrono::Utc>,
        attempts: i64,
        last_error: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let ts = next_attempt_at
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        conn.execute(
            "UPDATE subscription_deliveries
             SET next_attempt_at = ?1, attempts = ?2, last_error = ?3
             WHERE delivery_id = ?4",
            params![ts, attempts, last_error, delivery_id],
        )?;
        Ok(())
    }

    /// Fetch a single message by id for the delivery worker.
    pub fn get_message(&self, message_id: &str) -> Result<Option<ChatMessage>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT message_id, room_id, agent_id, agent_name, content, reply_to_message, metadata, created_at, seq
             FROM messages WHERE message_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![message_id], map_message_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingDelivery {
    pub delivery_id: String,
    pub subscription_id: String,
    pub message_seq: i64,
    pub message_id: String,
    pub attempts: i64,
}

fn map_subscription_row(
    row: &rusqlite::Row<'_>,
) -> Result<(clawchat_core::Subscription, String, String), rusqlite::Error> {
    let subscription_id: String = row.get(0)?;
    let room_id: String = row.get(1)?;
    let owner_key: String = row.get(2)?;
    let webhook_url: String = row.get(3)?;
    let secret: String = row.get(4)?;
    let kinds_json: Option<String> = row.get(5)?;
    let only_from: Option<String> = row.get(6)?;
    let not_from: Option<String> = row.get(7)?;
    let exclude_thinking: i64 = row.get(8)?;
    let since_seq: i64 = row.get(9)?;
    let last_delivered_seq: i64 = row.get(10)?;
    let status: String = row.get(11)?;
    let failure_count: i64 = row.get(12)?;
    let created_at_str: String = row.get(13)?;
    let kinds: Vec<String> = kinds_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());
    Ok((
        clawchat_core::Subscription {
            subscription_id,
            room_id,
            webhook_url,
            kinds,
            only_from,
            not_from,
            exclude_thinking: exclude_thinking != 0,
            since_seq,
            last_delivered_seq,
            status,
            failure_count,
            created_at,
        },
        owner_key,
        secret,
    ))
}

// --- Internal helpers that take an already-locked connection ---

fn query_room_by_id(conn: &Connection, room_id: &str) -> Result<Option<Room>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT room_id, name, description, parent_id, created_by, created_at, visibility, owner_key, encrypted FROM rooms WHERE room_id = ?1",
    )?;

    let room = stmt.query_row(params![room_id], map_room_row);

    match room {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(StoreError::Db(e)),
    }
}

fn ensure_column_exists(
    conn: &Connection,
    table: &str,
    column: &str,
    column_sql: &str,
) -> Result<(), rusqlite::Error> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;

    let mut exists = false;
    for row in rows {
        if row?.eq_ignore_ascii_case(column) {
            exists = true;
            break;
        }
    }

    if !exists {
        let alter = format!("ALTER TABLE {table} ADD COLUMN {column} {column_sql}");
        conn.execute(&alter, [])?;
    }

    Ok(())
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
        last_activity: None,
        member_count: None,
        encrypted: row.get::<_, bool>(8).unwrap_or(false),
    })
}

fn map_vote_meta_row(row: &rusqlite::Row) -> rusqlite::Result<VoteMeta> {
    let options_str: String = row.get(4)?;
    let closes_str: Option<String> = row.get(7)?;
    let created_str: String = row.get(6)?;

    Ok(VoteMeta {
        vote_id: row.get(0)?,
        room_id: row.get(1)?,
        title: row.get(2)?,
        description: row.get(3)?,
        options: serde_json::from_str(&options_str).unwrap_or_default(),
        created_by: row.get(5)?,
        created_at: parse_timestamp(&created_str),
        closes_at: closes_str.map(|s| parse_timestamp(&s)),
        status: row.get(8)?,
        eligible_voters: row.get::<_, i64>(9)? as usize,
    })
}

fn map_message_row(row: &rusqlite::Row) -> rusqlite::Result<ChatMessage> {
    let metadata_str: String = row.get(6)?;
    let metadata: serde_json::Value =
        serde_json::from_str(&metadata_str).unwrap_or(serde_json::json!({}));
    let ts_str: String = row.get(7)?;
    let seq: i64 = row.get(8).unwrap_or(0);

    Ok(ChatMessage {
        message_id: row.get(0)?,
        room_id: row.get(1)?,
        agent_id: row.get(2)?,
        agent_name: row.get(3)?,
        content: row.get(4)?,
        reply_to_message: row.get(5)?,
        metadata,
        timestamp: parse_timestamp(&ts_str),
        seq,
    })
}

/// One-time migration: assign per-room seq to any rows that still have seq=0.
/// Uses rowid order as a stand-in for insertion order. Idempotent.
fn backfill_message_seq(conn: &Connection) -> Result<(), rusqlite::Error> {
    // Skip if there are no zero-seq rows.
    let zero_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE seq = 0",
        [],
        |row| row.get(0),
    )?;
    if zero_count == 0 {
        return Ok(());
    }

    conn.execute_batch(
        "WITH numbered AS (
            SELECT rowid, ROW_NUMBER() OVER (PARTITION BY room_id ORDER BY rowid) AS rn
            FROM messages WHERE seq = 0
        )
        UPDATE messages
            SET seq = (SELECT rn FROM numbered WHERE numbered.rowid = messages.rowid)
            WHERE rowid IN (SELECT rowid FROM numbered);",
    )?;
    Ok(())
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
            .create_room(
                "test-room",
                "test-room",
                Some("A test"),
                None,
                Some("agent-1"),
            )
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
        // seq should be 1, 2 in insertion order, per-room.
        assert_eq!(history[0].seq, 1);
        assert_eq!(history[1].seq, 2);
        assert_eq!(store.room_tip("lobby").unwrap(), 2);
    }

    #[test]
    fn test_seq_is_per_room() {
        let store = Store::open_in_memory().unwrap();
        store.create_room("r1", "r1", None, None, None).unwrap();
        store.create_room("r2", "r2", None, None, None).unwrap();
        let m1 = store
            .insert_message("a", "r1", "ag", "A", "hi", None, &serde_json::json!({}))
            .unwrap();
        let m2 = store
            .insert_message("b", "r2", "ag", "A", "hi", None, &serde_json::json!({}))
            .unwrap();
        let m3 = store
            .insert_message("c", "r1", "ag", "A", "hi2", None, &serde_json::json!({}))
            .unwrap();
        assert_eq!((m1.seq, m2.seq, m3.seq), (1, 1, 2));
        assert_eq!(store.room_tip("r1").unwrap(), 2);
        assert_eq!(store.room_tip("r2").unwrap(), 1);
        assert_eq!(store.room_tip("nonexistent").unwrap(), 0);
    }

    #[test]
    fn test_history_since_seq() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            store
                .insert_message(
                    &format!("m{i}"),
                    "lobby",
                    "ag",
                    "A",
                    &format!("msg{i}"),
                    None,
                    &serde_json::json!({}),
                )
                .unwrap();
        }
        let history = store
            .get_history_filtered("lobby", 50, None, None, Some(2))
            .unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].seq, 3);
        assert_eq!(history[2].seq, 5);
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
