use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub description: String,
    pub created_at: String,
    pub metadata: serde_json::Value,
}

pub struct RecoveryManager {
    snapshots_dir: PathBuf,
    max_snapshots: usize,
}

impl RecoveryManager {
    pub fn new(snapshots_dir: PathBuf, max_snapshots: usize) -> Result<Self> {
        std::fs::create_dir_all(&snapshots_dir)
            .context("Failed to create snapshots directory")?;

        Ok(Self {
            snapshots_dir,
            max_snapshots,
        })
    }

    pub fn new_with_defaults() -> Result<Self> {
        let snapshots_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("/var/lib"))
            .join("aios")
            .join("snapshots");

        Self::new(snapshots_dir, 24)
    }

    /// Create a new snapshot
    pub async fn create_snapshot(&self, description: impl Into<String>) -> Result<Snapshot> {
        let snapshot = Snapshot {
            id: ulid::Ulid::new().to_string(),
            description: description.into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: serde_json::json!({}),
        };

        let snapshot_path = self.snapshots_dir.join(&snapshot.id);
        std::fs::create_dir_all(&snapshot_path)?;

        // Save snapshot metadata
        let metadata_path = snapshot_path.join("metadata.json");
        let metadata_json = serde_json::to_string_pretty(&snapshot)?;
        tokio::fs::write(metadata_path, metadata_json).await?;

        // Backup AIOS configuration and data
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("/var/lib"))
            .join("aios");

        let data_path = snapshot_path.join("data");
        std::fs::create_dir_all(&data_path)?;

        // Backup config.toml if exists
        let config_file = config_dir.join("config.toml");
        if config_file.exists() {
            let dest = data_path.join("config.toml");
            tokio::fs::copy(&config_file, &dest).await?;
            tracing::debug!("Backed up config.toml");
        }

        // Backup memory.db if exists
        let memory_db = config_dir.join("memory.db");
        if memory_db.exists() {
            let dest = data_path.join("memory.db");
            tokio::fs::copy(&memory_db, &dest).await?;
            tracing::debug!("Backed up memory.db");
        }

        // Backup memory.db-wal if exists (SQLite WAL file)
        let memory_wal = config_dir.join("memory.db-wal");
        if memory_wal.exists() {
            let dest = data_path.join("memory.db-wal");
            tokio::fs::copy(&memory_wal, &dest).await?;
        }

        // Backup memory.db-shm if exists (SQLite shared memory file)
        let memory_shm = config_dir.join("memory.db-shm");
        if memory_shm.exists() {
            let dest = data_path.join("memory.db-shm");
            tokio::fs::copy(&memory_shm, &dest).await?;
        }

        tracing::info!("Created snapshot: {}", snapshot.id);

        // Cleanup old snapshots
        self.cleanup_old_snapshots().await?;

        Ok(snapshot)
    }

    /// List all snapshots
    pub async fn list_snapshots(&self) -> Result<Vec<Snapshot>> {
        let mut entries = tokio::fs::read_dir(&self.snapshots_dir).await?;
        let mut snapshots = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }

            let metadata_path = entry.path().join("metadata.json");
            if !metadata_path.exists() {
                continue;
            }

            let metadata_json = tokio::fs::read_to_string(&metadata_path).await?;
            if let Ok(snapshot) = serde_json::from_str::<Snapshot>(&metadata_json) {
                snapshots.push(snapshot);
            }
        }

        // Sort by created_at descending
        snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        Ok(snapshots)
    }

    /// Get a snapshot by ID
    pub async fn get_snapshot(&self, id: &str) -> Result<Snapshot> {
        let metadata_path = self.snapshots_dir.join(id).join("metadata.json");
        let metadata_json = tokio::fs::read_to_string(&metadata_path)
            .await
            .context("Snapshot not found")?;

        let snapshot: Snapshot = serde_json::from_str(&metadata_json)?;
        Ok(snapshot)
    }

    /// Delete a snapshot
    pub async fn delete_snapshot(&self, id: &str) -> Result<()> {
        let snapshot_path = self.snapshots_dir.join(id);
        tokio::fs::remove_dir_all(snapshot_path).await?;

        tracing::info!("Deleted snapshot: {}", id);
        Ok(())
    }

    /// Restore from a snapshot
    pub async fn restore_snapshot(&self, id: &str) -> Result<()> {
        let snapshot = self.get_snapshot(id).await?;

        tracing::info!("Restoring from snapshot: {}", snapshot.id);

        let snapshot_path = self.snapshots_dir.join(id);
        let data_path = snapshot_path.join("data");

        if !data_path.exists() {
            anyhow::bail!("Snapshot data not found for: {}", id);
        }

        // Get AIOS config directory
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("/var/lib"))
            .join("aios");

        std::fs::create_dir_all(&config_dir)?;

        // Restore config.toml if exists in snapshot
        let config_snapshot = data_path.join("config.toml");
        if config_snapshot.exists() {
            let dest = config_dir.join("config.toml");
            tokio::fs::copy(&config_snapshot, &dest).await?;
            tracing::info!("Restored config.toml");
        }

        // Restore memory.db if exists in snapshot
        let memory_snapshot = data_path.join("memory.db");
        if memory_snapshot.exists() {
            let dest = config_dir.join("memory.db");
            tokio::fs::copy(&memory_snapshot, &dest).await?;
            tracing::info!("Restored memory.db");
        }

        // Restore WAL file if exists
        let wal_snapshot = data_path.join("memory.db-wal");
        if wal_snapshot.exists() {
            let dest = config_dir.join("memory.db-wal");
            tokio::fs::copy(&wal_snapshot, &dest).await?;
        }

        // Restore SHM file if exists
        let shm_snapshot = data_path.join("memory.db-shm");
        if shm_snapshot.exists() {
            let dest = config_dir.join("memory.db-shm");
            tokio::fs::copy(&shm_snapshot, &dest).await?;
        }

        tracing::info!("Successfully restored snapshot: {}", snapshot.id);

        Ok(())
    }

    /// Cleanup old snapshots
    async fn cleanup_old_snapshots(&self) -> Result<()> {
        let snapshots = self.list_snapshots().await?;

        if snapshots.len() > self.max_snapshots {
            let to_delete = snapshots.len() - self.max_snapshots;

            for snapshot in snapshots.iter().rev().take(to_delete) {
                self.delete_snapshot(&snapshot.id).await?;
            }

            tracing::info!("Cleaned up {} old snapshots", to_delete);
        }

        Ok(())
    }

    /// Create a snapshot before executing a dangerous command
    pub async fn safe_execute<F, T>(&self, description: &str, operation: F) -> Result<T>
    where
        F: FnOnce() -> Result<T>,
    {
        // Create snapshot
        let snapshot = self.create_snapshot(description).await?;

        tracing::info!("Created safety snapshot: {}", snapshot.id);

        // Execute operation
        let result = operation();

        // If operation failed, suggest rollback
        if result.is_err() {
            tracing::error!(
                "Operation failed. You can rollback using snapshot: {}",
                snapshot.id
            );
        }

        result
    }
}

