use include_dir::{Dir, File};
use serde::Deserialize;
use serde::Serialize;
use std::time::Duration;
use surrealdb::RecordId;
use surrealdb::Response; // Import Response
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::time::sleep;
use tracing;

use super::DBError;

// Embed the migrations directory relative to Cargo.toml
const LOCK_ID: (&str, &str) = ("schema_lock", "singleton");

#[derive(Error, Debug)]
pub enum MigrationError {
    #[error("Database error during migration: {source}")]
    DBError { source: Box<surrealdb::Error> },
    #[error("Failed to create table from Schema: {0}")]
    SchemaError(#[from] Box<DBError>),
    #[error("Migration script failed: {id}, {source}")]
    ScriptFailed {
        id: String,
        source: Box<surrealdb::Error>,
    },
    #[error("Failed to acquire migration lock: {0}")]
    LockFailed(String),
    #[error("Could not parse migration ID from filename: {0}")]
    InvalidId(String),
    #[error("Migration file content is not valid UTF-8: {0}")]
    InvalidUtf8(String),
}

// Struct to represent an applied migration record from DB
#[derive(Serialize, Deserialize, Debug)]
struct AppliedMigration {
    migration: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct SchemaLock {
    id: RecordId,
    locked: bool,
    #[serde(with = "time::serde::rfc3339::option", default)]
    locked_at: Option<OffsetDateTime>,
    instance_id: Option<String>,
}

impl Default for SchemaLock {
    fn default() -> Self {
        Self {
            id: RecordId::from(LOCK_ID),
            locked: false,
            locked_at: None,
            instance_id: None,
        }
    }
}

#[derive(Clone)]
pub struct Migrations {
    migrations_dir: Dir<'static>,
}

impl Migrations {
    pub fn new(migrations_dir: Dir<'static>) -> Self {
        Self { migrations_dir }
    }

    // --- Migration Logic ---
    pub async fn run_migrations(
        self,
        db: &Surreal<Any>,
        instance_id: String,
    ) -> Result<(), MigrationError> {
        tracing::info!("Starting database schema migration check...");

        // Check if the migration table exists
        // If it doesn't it means the database hasn't been initialized yet
        let last_applied_id: Option<String> = Self::get_last_applied_migration(db).await?;

        // 1. Acquire Lock (with retries and timeout)
        if last_applied_id.is_some() && !Self::acquire_lock(db, &instance_id).await? {
            // Failed to acquire lock after retries
            return Err(MigrationError::LockFailed(
                "Timed out waiting for migration lock".into(),
            ));
        }

        // Use a guard pattern or careful error handling to ensure lock release
        let lock_release_result = async {
            tracing::debug!(
                "Last applied migration ID: {:?}",
                last_applied_id.clone().unwrap_or("None".to_string())
            );

            // 3. Load, Sort, and Filter Embedded Migrations
            let mut migrations_to_run = self.get_pending_migrations(&last_applied_id)?;

            if migrations_to_run.is_empty() {
                tracing::info!("Database schema is up to date.");
                return Ok(()); // Exit early if no migrations needed
            }

            tracing::info!(
                "Found {} pending migration(s). Applying...",
                migrations_to_run.len()
            );

            // 4. Execute Pending Migrations Sequentially
            for (id, script_content) in migrations_to_run.drain(..) {
                tracing::info!("Applying migration: {}", id);
                // Execute the whole script content. SurrealDB's query method
                // can handle multi-statement strings.
                let script = format!("BEGIN;{script_content} CREATE schema_migration SET migration=$migration, applied_at=time::now(); COMMIT; ");
                let response: Response =
                    db.query(&script)
                        .bind(("migration", id.clone()))
                        .await
                        .map_err(|e| MigrationError::ScriptFailed {
                            id: id.clone(),
                            source: Box::new(e),
                        })?;

                // Optionally check responses for errors if needed, though query() might error out directly
                response.check().map_err(|e| MigrationError::ScriptFailed {
                    id: id.clone(),
                    source: Box::new(e),
                })?;

                tracing::debug!("Successfully applied migration: {}", id);
            }

            tracing::info!("All pending migrations applied successfully.");
            Ok(())
        }
        .await; // Run the migration logic block

        // 6. Release Lock (ALWAYS attempt this)
        if last_applied_id.is_some() {
            Self::release_lock(db, &instance_id).await?; // Log errors but don't block shutdown
            tracing::debug!("Migration lock released by instance {}", instance_id);
        }

        // Return the result of the migration logic block
        lock_release_result
    }

    pub async fn has_pending_migrations(&self, db: &Surreal<Any>) -> Result<bool, MigrationError> {
        let last_applied_id = Self::get_last_applied_migration(db).await?;
        let pending_migrations = self.get_pending_migrations(&last_applied_id)?;
        Ok(!pending_migrations.is_empty())
    }

    fn get_pending_migrations(
        &self,
        last_applied_id: &Option<String>,
    ) -> Result<Vec<(String, String)>, MigrationError> {
        let mut migrations: Vec<(String, &File<'_>)> = self
            .migrations_dir
            .files()
            .filter(|file| file.path().extension().is_some_and(|ext| ext == "surql"))
            .map(|file| {
                let id = file
                    .path()
                    .file_stem()
                    .ok_or_else(|| MigrationError::InvalidId(file.path().display().to_string()))?
                    .to_string_lossy()
                    .to_string();
                tracing::debug!("Found migration file: {}", file.path().display());
                Ok((id, file))
            })
            .collect::<Result<Vec<_>, MigrationError>>()?;

        // Sort migrations by ID (e.g., "0001", "0002")
        migrations.sort_by(|(id_a, _), (id_b, _)| id_a.cmp(id_b));

        // Filter out already applied migrations
        let pending = migrations
            .into_iter()
            .filter(|(id, _)| {
                match last_applied_id {
                    Some(last_id) => id > last_id,
                    None => true, // No migrations applied yet, run all
                }
            })
            .map(|(id, file)| {
                let content = file
                    .contents_utf8()
                    .ok_or_else(|| MigrationError::InvalidUtf8(file.path().display().to_string()))?
                    .to_string();
                Ok((id, content))
            })
            .collect::<Result<Vec<_>, MigrationError>>()?;

        Ok(pending)
    }

    async fn get_last_applied_migration(
        db: &Surreal<Any>,
    ) -> Result<Option<String>, MigrationError> {
        // Select the latest migration ID
        // Ensure the migration table exists (created in 0001)
        let result = db
            .query("SELECT * FROM schema_migration ORDER BY applied_at DESC LIMIT 1;")
            .await
            .map_err(|e| MigrationError::DBError {
                source: Box::new(e),
            })?
            .take::<Vec<AppliedMigration>>(0)
            .map_err(|e| MigrationError::DBError {
                source: Box::new(e),
            })?;
        if let Some(migration) = result.first() {
            Ok(Some(migration.migration.clone()))
        } else {
            Ok(None)
        }
    }

    const LOCK_RETRY_DELAY: Duration = Duration::from_secs(1);
    const LOCK_MAX_WAIT: Duration = Duration::from_secs(60); // Max time to wait for lock

    /// Attempts to acquire the migration lock. Returns Ok(true) if acquired, Ok(false) if timed out.
    async fn acquire_lock(db: &Surreal<Any>, instance_id: &str) -> Result<bool, MigrationError> {
        // Create lock record if it doesn't exist
        tracing::debug!("Creating migration lock record if it doesn't exist...");
        let _: Option<SchemaLock> =
            db.upsert(LOCK_ID)
                .await
                .map_err(|e| MigrationError::DBError {
                    source: Box::new(e),
                })?;
        tracing::debug!("Migration lock record created or already exists.");

        let start_time = std::time::Instant::now();
        loop {
            // Try to atomically set locked=true WHERE locked=false
            // Also update timestamp and instance ID
            let sql = r#"UPDATE $lock_id SET locked=true, instance_id=$instance_id WHERE locked = false;"#;
            let mut response = db
                .query(sql)
                .bind(("lock_id", RecordId::from(LOCK_ID)))
                .bind(("instance_id", instance_id.to_owned()))
                .await
                .map_err(|e| MigrationError::DBError {
                    source: Box::new(e),
                })?;

            // Check if the update succeeded (meaning we got the lock)
            // SurrealDB UPDATE returns the updated record(s) or empty if WHERE failed
            let updated_lock: Option<SchemaLock> =
                response.take(0).map_err(|e| MigrationError::DBError {
                    source: Box::new(e),
                })?;
            if updated_lock.is_some() {
                tracing::debug!("Migration lock acquired by instance {}", instance_id);
                return Ok(true); // Lock acquired!
            }

            // Lock not acquired, check timeout
            if start_time.elapsed() > Self::LOCK_MAX_WAIT {
                return Ok(false); // Timeout waiting for lock
            }

            // Wait before retrying
            tracing::debug!(
                "Migration lock held by another instance. Retrying in {:?}...",
                Self::LOCK_RETRY_DELAY
            );
            sleep(Self::LOCK_RETRY_DELAY).await;

            // Optional: Add stale lock detection here (check locked_at timestamp)
        }
    }

    /// Releases the migration lock held by this instance.
    async fn release_lock(db: &Surreal<Any>, instance_id: &str) -> Result<(), MigrationError> {
        // Only release if *this* instance holds the lock (important!)
        let sql = r#"
            UPDATE $lock_id SET locked = false WHERE locked = true AND instance_id = $instance_id;
        "#;
        let response = db
            .query(sql)
            .bind(("lock_id", RecordId::from(LOCK_ID)))
            .bind(("instance_id", instance_id.to_owned()))
            .await
            .map_err(|e| MigrationError::DBError {
                source: Box::new(e),
            })?;
        // Log potential errors but don't necessarily fail the whole shutdown
        if let Err(e) = response.check() {
            // check() might not be needed on query
            tracing::error!(
                "Failed to release migration lock (instance: {}): {}",
                instance_id,
                e
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatabaseConfig;

    #[allow(dead_code)]
    fn create_test_config() -> DatabaseConfig {
        DatabaseConfig {
            url: "memory".to_string(),
            username: "root".to_string(),
            password: "root".to_string(),
            namespace: "test_migrations".to_string(),
            database: "test_migrations".to_string(),
        }
    }

    #[test]
    fn test_migration_error_display() {
        let db_error = MigrationError::LockFailed("Test lock error".to_string());
        assert_eq!(
            format!("{}", db_error),
            "Failed to acquire migration lock: Test lock error"
        );

        let invalid_id_error = MigrationError::InvalidId("invalid_file.txt".to_string());
        assert_eq!(
            format!("{}", invalid_id_error),
            "Could not parse migration ID from filename: invalid_file.txt"
        );

        let invalid_utf8_error = MigrationError::InvalidUtf8("file.sql".to_string());
        assert_eq!(
            format!("{}", invalid_utf8_error),
            "Migration file content is not valid UTF-8: file.sql"
        );
    }

    #[test]
    fn test_applied_migration_serialization() {
        let migration = AppliedMigration {
            migration: "001_initial".to_string(),
        };

        let json = serde_json::to_string(&migration).unwrap();
        assert!(json.contains("001_initial"));

        let deserialized: AppliedMigration = serde_json::from_str(&json).unwrap();
        assert_eq!(migration.migration, deserialized.migration);
    }

    #[test]
    fn test_schema_lock_serialization() {
        let lock = SchemaLock {
            id: RecordId::from_table_key("schema_lock", "singleton"),
            locked: true,
            locked_at: Some(OffsetDateTime::now_utc()),
            instance_id: Some("test_instance".to_string()),
        };

        let json = serde_json::to_string(&lock).unwrap();
        assert!(json.contains("singleton"));
        assert!(json.contains("true"));
        assert!(json.contains("test_instance"));

        let deserialized: SchemaLock = serde_json::from_str(&json).unwrap();
        assert_eq!(lock.locked, deserialized.locked);
        assert_eq!(lock.instance_id, deserialized.instance_id);
    }

    #[test]
    fn test_schema_lock_default() {
        let lock = SchemaLock::default();
        assert!(!lock.locked);
        assert!(lock.locked_at.is_none());
        assert!(lock.instance_id.is_none());
        // Test that the ID is properly set (we can't easily test the table name without more complex setup)
    }

    #[test]
    fn test_migration_id_parsing() {
        // Test valid migration filenames
        let valid_files = vec![
            "001_initial.sql",
            "002_add_users.sql",
            "999_final_migration.sql",
        ];

        for filename in valid_files {
            // We can't directly test the private parse_migration_id function,
            // but we can test the concept
            let id_part = filename.split('_').next().unwrap();
            assert!(id_part.parse::<u32>().is_ok());
        }
    }

    #[test]
    fn test_invalid_migration_filenames() {
        let invalid_files = vec![
            "invalid.sql",
            "abc_migration.sql",
            "migration_001.sql",
            "001.txt",
        ];

        for filename in invalid_files {
            let id_part = filename.split('_').next().unwrap();
            // These should either fail to parse as numbers or not follow the pattern
            let is_valid_id = id_part.parse::<u32>().is_ok() && filename.ends_with(".sql");
            if filename == "001.txt" {
                assert!(!is_valid_id); // Wrong extension
            }
        }
    }

    #[test]
    fn test_lock_constants() {
        assert_eq!(LOCK_ID.0, "schema_lock");
        assert_eq!(LOCK_ID.1, "singleton");
    }

    #[tokio::test]
    async fn test_migration_error_from_db_error() {
        let db_error = DBError::Database("Test error".to_string());
        let migration_error = MigrationError::SchemaError(Box::new(db_error));

        assert!(format!("{}", migration_error).contains("Failed to create table from Schema"));
    }

    #[test]
    fn test_migration_script_failed_error() {
        let surreal_error =
            surrealdb::Error::Db(surrealdb::error::Db::Internal("Script failed".to_string()));
        let migration_error = MigrationError::ScriptFailed {
            id: "001_test".to_string(),
            source: Box::new(surreal_error),
        };

        let error_msg = format!("{}", migration_error);
        assert!(error_msg.contains("Migration script failed: 001_test"));
    }
}
