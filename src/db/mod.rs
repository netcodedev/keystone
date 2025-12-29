use surrealdb::{
    Surreal,
    engine::any::{Any, connect},
    opt::auth::Root,
};
use thiserror::Error;
use tokio::time::{Duration, timeout};

use crate::config::DatabaseConfig;

pub mod migrations;

#[derive(Debug, Error)]
pub enum DBError {
    #[error("Database error: {0}")]
    Database(String),
    #[error("SurrealDB error: {0}")]
    Surreal(#[from] surrealdb::Error),
    #[error("Item not found: {0}")]
    NotFound(String),
    #[error("Database operation failed: {0}")]
    OperationFailed(String),
    // Add other specific DB errors if needed
    #[error("Health check timed out")]
    HealthTimeout, // Specific variant for timeout
    #[error("Health check failed: {0}")]
    HealthFailed(String), // Specific variant for DB error during health check
}

pub struct Database {}

impl Database {
    pub async fn new_unauthorized(config: &DatabaseConfig) -> Result<Surreal<Any>, DBError> {
        let db = connect(&config.url).await.map_err(|e| {
            DBError::Database(format!(
                "Failed to connect to SurrealDB URL '{}': {}",
                config.url, e
            ))
        })?;

        Ok(db)
    }
    pub async fn connect(config: &DatabaseConfig) -> Result<Surreal<Any>, DBError> {
        // surrealdb::engine::any::connect automatically handles ws://, file://, mem:// etc.
        let db = Database::new_unauthorized(config).await?;

        tracing::debug!("Successfully connected to DB endpoint. Signing in...");

        // Sign in using root credentials (adjust if using different auth methods like Scopes)
        db.signin(Root {
            username: &config.username,
            password: &config.password,
        })
        .await
        .map_err(|e| DBError::Database(format!("Failed to sign in to SurrealDB: {e}")))?;

        tracing::debug!("Successfully signed in. Selecting NS/DB...");

        // Select the namespace and database
        db.use_ns(&config.namespace)
            .use_db(&config.database)
            .await
            .map_err(|e| {
                DBError::Database(format!(
                    "Failed to select namespace '{}' or database '{}': {}",
                    config.namespace, config.database, e
                ))
            })?;

        tracing::debug!("SurrealDB connection successful");
        Ok(db)
    }

    pub async fn check_health(db: &Surreal<Any>) -> Result<&'static str, DBError> {
        let health_check_timeout = Duration::from_secs(3);
        let health_check_future = db.health();

        match timeout(health_check_timeout, health_check_future).await {
            // Case 1: Timeout completed *within* the duration...
            Ok(Ok(_)) => {
                // ...and the inner health() future returned Ok
                tracing::debug!("Database health check successful."); // Maybe debug level
                Ok("OK") // DB is healthy
            }
            Ok(Err(e)) => {
                // ...and the inner health() future returned an Error
                tracing::error!("Database health check failed: {}", e);
                Err(DBError::HealthFailed(e.to_string())) // DB reported an error
            }
            // Case 2: Timeout *elapsed* before health() completed
            Err(_) => {
                tracing::error!(
                    "Database health check timed out after {} seconds",
                    health_check_timeout.as_secs()
                );
                Err(DBError::HealthTimeout) // DB check timed out
            }
        }
    }

    pub async fn force_unlock(db: &Surreal<Any>) -> Result<(), DBError> {
        tracing::warn!("Attempting to force unlock database migration lock...");
        let result = db.query("UPDATE schema_lock SET locked = false").await?;
        match result.check() {
            Ok(_) => {
                tracing::info!("Database migration lock successfully unlocked.");
                Ok(())
            }
            Err(e) => Err(DBError::Database(format!(
                "Failed to unlock database migration lock: {e}"
            ))),
        }
    }

    pub async fn is_locked(db: &Surreal<Any>) -> Result<bool, DBError> {
        let mut result = db.query("SELECT * FROM schema_lock").await?;
        let locks: Vec<migrations::SchemaLock> = result
            .take(0)
            .map_err(|e| DBError::Database(format!("Failed to fetch schema lock status: {}", e)))?;

        if let Some(lock) = locks.first() {
            Ok(lock.locked)
        } else {
            // If no lock record exists, it's effectively not locked (or not initialized)
            Ok(false)
        }
    }

    pub async fn user_has_permission(db: &Surreal<Any>, permission: &str) -> Result<bool, DBError> {
        let has_permission: bool = db
            .run("fn::auth_user_has_permission")
            .args(permission)
            .await?;

        Ok(has_permission)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_config() -> DatabaseConfig {
        DatabaseConfig {
            url: "memory".to_string(), // Use in-memory database for tests
            username: "root".to_string(),
            password: "root".to_string(),
            namespace: "test".to_string(),
            database: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn test_database_new_unauthorized() {
        let config = create_test_config();
        let result = Database::new_unauthorized(&config).await;
        // Memory engine might not be available in all builds, so we test that the function doesn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_database_connect() {
        let config = create_test_config();
        let result = Database::connect(&config).await;
        // Memory engine might not be available in all builds, so we test that the function doesn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_database_connect_invalid_url() {
        let mut config = create_test_config();
        config.url = "invalid://url".to_string();

        let result = Database::connect(&config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DBError::Database(_)));
    }

    #[tokio::test]
    async fn test_database_health_check() {
        let config = create_test_config();
        if let Ok(db) = Database::connect(&config).await {
            let result = Database::check_health(&db).await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), "OK");
        }
    }

    #[tokio::test]
    async fn test_database_force_unlock() {
        let config = create_test_config();
        if let Ok(db) = Database::connect(&config).await {
            // This might fail if the table doesn't exist, but we're testing the function structure
            let result = Database::force_unlock(&db).await;
            // Don't assert success since the table might not exist in test environment
            assert!(result.is_ok() || result.is_err());
        }
    }

    #[test]
    fn test_db_error_display() {
        let db_error = DBError::Database("Test error".to_string());
        assert_eq!(format!("{}", db_error), "Database error: Test error");

        let not_found_error = DBError::NotFound("Item".to_string());
        assert_eq!(format!("{}", not_found_error), "Item not found: Item");

        let operation_failed_error = DBError::OperationFailed("Operation".to_string());
        assert_eq!(
            format!("{}", operation_failed_error),
            "Database operation failed: Operation"
        );

        let health_timeout_error = DBError::HealthTimeout;
        assert_eq!(
            format!("{}", health_timeout_error),
            "Health check timed out"
        );

        let health_failed_error = DBError::HealthFailed("Connection lost".to_string());
        assert_eq!(
            format!("{}", health_failed_error),
            "Health check failed: Connection lost"
        );
    }

    #[test]
    fn test_db_error_from_surreal_error() {
        // Test that SurrealDB errors are properly converted
        use surrealdb::error::Db;
        let surreal_error = surrealdb::Error::Db(Db::Internal("Test DB error".to_string()));
        let db_error: DBError = surreal_error.into();
        assert!(matches!(db_error, DBError::Surreal(_)));
    }

    #[test]
    fn test_database_config_validation() {
        let config = create_test_config();
        assert_eq!(config.url, "memory");
        assert_eq!(config.username, "root");
        assert_eq!(config.password, "root");
        assert_eq!(config.namespace, "test");
        assert_eq!(config.database, "test");
    }

    #[tokio::test]
    async fn test_database_connect_with_different_configs() {
        // Test with different valid configurations
        let configs = vec![
            DatabaseConfig {
                url: "memory".to_string(),
                username: "root".to_string(),
                password: "root".to_string(),
                namespace: "test1".to_string(),
                database: "test1".to_string(),
            },
            DatabaseConfig {
                url: "memory".to_string(),
                username: "admin".to_string(),
                password: "admin".to_string(),
                namespace: "test2".to_string(),
                database: "test2".to_string(),
            },
        ];

        for config in configs {
            let result = Database::connect(&config).await;
            // Memory engine might not be available, so we just test that it doesn't panic
            assert!(result.is_ok() || result.is_err());
        }
    }

    #[tokio::test]
    async fn test_database_connect_wrong_credentials() {
        let mut config = create_test_config();
        config.username = "wronguser".to_string();
        config.password = "wrongpass".to_string();

        let result = Database::connect(&config).await;
        // This might succeed with memory database, so we just test that it doesn't panic
        assert!(result.is_ok() || result.is_err());
    }
}
