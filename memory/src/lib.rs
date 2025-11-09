use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub content: String,
    pub memory_type: MemoryType,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    Command,    // Executed commands
    Chat,       // Chat history
    System,     // System events
    Snapshot,   // System snapshots
}

impl ToString for MemoryType {
    fn to_string(&self) -> String {
        match self {
            MemoryType::Command => "command".to_string(),
            MemoryType::Chat => "chat".to_string(),
            MemoryType::System => "system".to_string(),
            MemoryType::Snapshot => "snapshot".to_string(),
        }
    }
}

pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    /// Create or open memory database
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        // Create parent directories
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path.as_ref())
            .context("Failed to open memory database")?;

        let store = Self { conn };
        store.initialize()?;

        Ok(store)
    }

    /// Open default memory database
    pub fn default() -> Result<Self> {
        let path = Self::default_path();
        Self::new(path)
    }

    /// Get default database path
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("/var/lib"))
            .join("aios")
            .join("memory.db")
    }

    /// Initialize database schema
    fn initialize(&self) -> Result<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                memory_type TEXT NOT NULL,
                metadata TEXT,
                created_at TEXT NOT NULL
            )",
            [],
        )?;

        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_memories_type
             ON memories(memory_type)",
            [],
        )?;

        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_memories_created
             ON memories(created_at DESC)",
            [],
        )?;

        self.conn.execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
             USING fts5(id, content, content=memories)",
            [],
        )?;

        Ok(())
    }

    /// Create a new memory
    pub fn create(&self, memory: &Memory) -> Result<()> {
        self.conn.execute(
            "INSERT INTO memories (id, content, memory_type, metadata, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                memory.id,
                memory.content,
                memory.memory_type.to_string(),
                serde_json::to_string(&memory.metadata)?,
                memory.created_at,
            ],
        )?;

        // Update FTS index
        self.conn.execute(
            "INSERT INTO memories_fts (id, content) VALUES (?1, ?2)",
            params![memory.id, memory.content],
        )?;

        Ok(())
    }

    /// Get memory by ID
    pub fn get(&self, id: &str) -> Result<Memory> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, memory_type, metadata, created_at
             FROM memories WHERE id = ?1"
        )?;

        let memory = stmt.query_row(params![id], |row| {
            Ok(Memory {
                id: row.get(0)?,
                content: row.get(1)?,
                memory_type: match row.get::<_, String>(2)?.as_str() {
                    "command" => MemoryType::Command,
                    "chat" => MemoryType::Chat,
                    "system" => MemoryType::System,
                    "snapshot" => MemoryType::Snapshot,
                    _ => MemoryType::System,
                },
                metadata: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or(serde_json::json!({})),
                created_at: row.get(4)?,
            })
        })?;

        Ok(memory)
    }

    /// List recent memories
    pub fn list(&self, memory_type: Option<MemoryType>, limit: usize) -> Result<Vec<Memory>> {
        let query = match memory_type {
            Some(_) => {
                "SELECT id, content, memory_type, metadata, created_at
                 FROM memories WHERE memory_type = ?1
                 ORDER BY created_at DESC LIMIT ?2"
            }
            None => {
                "SELECT id, content, memory_type, metadata, created_at
                 FROM memories
                 ORDER BY created_at DESC LIMIT ?1"
            }
        };

        let mut stmt = self.conn.prepare(query)?;

        let rows = match memory_type {
            Some(mt) => stmt.query_map(params![mt.to_string(), limit], Self::row_to_memory)?,
            None => stmt.query_map(params![limit], Self::row_to_memory)?,
        };

        let memories: Result<Vec<Memory>, _> = rows.collect();
        Ok(memories?)
    }

    /// Search memories by content
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.content, m.memory_type, m.metadata, m.created_at
             FROM memories m
             JOIN memories_fts fts ON m.id = fts.id
             WHERE memories_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2"
        )?;

        let rows = stmt.query_map(params![query, limit], Self::row_to_memory)?;
        let memories: Result<Vec<Memory>, _> = rows.collect();
        Ok(memories?)
    }

    /// Delete old memories
    pub fn cleanup(&self, keep_days: i64) -> Result<usize> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(keep_days);
        let cutoff_str = cutoff.to_rfc3339();

        let deleted = self.conn.execute(
            "DELETE FROM memories WHERE created_at < ?1",
            params![cutoff_str],
        )?;

        // Optimize FTS index
        self.conn.execute("INSERT INTO memories_fts(memories_fts) VALUES('optimize')", [])?;

        Ok(deleted)
    }

    fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
        Ok(Memory {
            id: row.get(0)?,
            content: row.get(1)?,
            memory_type: match row.get::<_, String>(2)?.as_str() {
                "command" => MemoryType::Command,
                "chat" => MemoryType::Chat,
                "system" => MemoryType::System,
                "snapshot" => MemoryType::Snapshot,
                _ => MemoryType::System,
            },
            metadata: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or(serde_json::json!({})),
            created_at: row.get(4)?,
        })
    }
}

impl Memory {
    pub fn new(content: impl Into<String>, memory_type: MemoryType) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            content: content.into(),
            memory_type,
            metadata: serde_json::json!({}),
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}
