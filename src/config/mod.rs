use std::{env, path::PathBuf};

use config::{Environment, File};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// Define potential config file locations
const ENV_VAR_PATH: &'static str = "CONFIG_PATH";
// Environment variable for the environment (dev / prod)
const ENV_VAR_MODE: &'static str = "ENV";

pub trait Config: Serialize + for<'de> Deserialize<'de> {
    fn get_server_config(&self) -> ServerConfig;
    fn get_database_config(&self) -> DatabaseConfig;
    fn get_security_config(&self) -> SecurityConfig;
    fn get_instance_id(&self) -> Option<String>;

    fn load(default_system_path: &str, local_dev_path: &str) -> Result<Self, ConfigError>
    where
        Self: Sized + for<'de> serde::Deserialize<'de>,
    {
        let mut builder = config::Config::builder();

        // 1. Determine config file path using our helper function
        match Self::get_config_path(default_system_path, local_dev_path) {
            Ok(path) => {
                tracing::debug!("Loading configuration from: {}", path.display());
                builder = builder.add_source(File::from(path));
            }
            Err(ConfigError::NotFound(msg)) => {
                tracing::info!(
                    "No configuration file found ({}). Relying on environment variables.",
                    msg
                );
            }
            Err(e) => return Err(e),
        }

        // 2. Add Environment variables
        // We use "BUMACS" as prefix, so BUMACS_SERVER_PORT maps to server.port
        builder = builder.add_source(Environment::with_prefix("BUMACS").separator("_"));

        // 3. Build and deserialize
        let config = builder.build()?;
        let app_config: Self = config.try_deserialize()?;

        Ok(app_config)
    }

    /// Finds the configuration file path based on environment variable or default locations.
    fn get_config_path(
        default_system_path: &str,
        local_dev_path: &str,
    ) -> Result<PathBuf, ConfigError> {
        // Check environment variable first
        if let Ok(path_str) = env::var(ENV_VAR_PATH) {
            let path = PathBuf::from(path_str);
            if path.exists() {
                tracing::debug!(
                    "Using config path from environment variable {}: {}",
                    ENV_VAR_PATH,
                    path.display()
                );
                return Ok(path);
            } else {
                // Warn if ENV var is set but file doesn't exist, then continue searching
                tracing::warn!(
                    "Config path from env var {}='{}' does not exist. Checking default locations...",
                    ENV_VAR_PATH,
                    path.display()
                );
            }
        }

        // Check default system path
        let system_path = PathBuf::from(default_system_path);
        if system_path.exists() {
            tracing::debug!(
                "Using default system config path: {}",
                system_path.display()
            );
            return Ok(system_path);
        }

        // Check local path (useful for development when running `cargo run`)
        let is_production =
            env::var(ENV_VAR_MODE).map_or(true, |mode| mode.eq_ignore_ascii_case("production"));
        if !is_production {
            let local_path = PathBuf::from(local_dev_path);
            if local_path.exists() {
                tracing::warn!(
                    "Using local development config path: {}. This should not be used in production.",
                    local_path.display()
                );
                return Ok(local_path);
            }
        }

        // If no config file found
        let mut error_msg = format!(
            "Configuration file not found. Set {} or place it at {}",
            ENV_VAR_PATH, default_system_path,
        );
        if !is_production {
            // Only suggest local path if not in production
            error_msg.push_str(&format!(" or {}", local_dev_path));
        }
        Err(ConfigError::NotFound(error_msg))
    }
}

fn default_hsts_max_age() -> u32 {
    31536000 // 1 year
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DatabaseConfig {
    pub url: String,
    pub username: String,
    pub password: String,
    pub namespace: String,
    pub database: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SecurityConfig {
    /// Enable HSTS header (Strict-Transport-Security)
    #[serde(default)]
    pub enable_hsts: bool,

    /// HSTS max-age in seconds (default: 1 year)
    #[serde(default = "default_hsts_max_age")]
    pub hsts_max_age: u32,

    /// Include subdomains in HSTS
    #[serde(default = "default_true")]
    pub hsts_include_subdomains: bool,

    /// Enable HSTS preload
    #[serde(default)]
    pub hsts_preload: bool,

    /// Content Security Policy header value (empty = disabled)
    #[serde(default)]
    pub content_security_policy: Option<String>,

    /// CORS allowed origins (empty = disabled, "*" = all origins)
    #[serde(default)]
    pub cors_origins: Vec<String>,

    /// Enable additional security headers (X-Content-Type-Options, X-Frame-Options, etc.)
    #[serde(default = "default_true")]
    pub enable_security_headers: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            enable_hsts: false, // Default off for self-hosted flexibility
            hsts_max_age: default_hsts_max_age(),
            hsts_include_subdomains: true,
            hsts_preload: false,
            content_security_policy: None,       // Disabled by default
            cors_origins: vec!["*".to_string()], // Permissive default for self-hosted
            enable_security_headers: true,       // Safe defaults always enabled
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to load configuration: {0}")]
    LoadError(String),
    #[error("{0}")]
    NotFound(String),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[derive(Serialize, Deserialize, Debug)]
    struct TestConfig {
        server: ServerConfig,
        database: DatabaseConfig,
        #[serde(default)]
        security: SecurityConfig,
        #[serde(alias = "instanceid")]
        instance_id: Option<String>,
    }

    impl Config for TestConfig {
        fn get_server_config(&self) -> ServerConfig {
            self.server.clone()
        }
        fn get_database_config(&self) -> DatabaseConfig {
            self.database.clone()
        }
        fn get_security_config(&self) -> SecurityConfig {
            self.security.clone()
        }
        fn get_instance_id(&self) -> Option<String> {
            self.instance_id.clone()
        }
    }

    #[test]
    fn test_load_from_env() {
        // Set environment variables
        unsafe {
            env::set_var("BUMACS_SERVER_PORT", "9090");
            env::set_var("BUMACS_SERVER_HOST", "127.0.0.1");
            env::set_var("BUMACS_DATABASE_URL", "ws://test:8000");
            env::set_var("BUMACS_DATABASE_USERNAME", "admin");
            env::set_var("BUMACS_DATABASE_PASSWORD", "secret");
            env::set_var("BUMACS_DATABASE_NAMESPACE", "test_ns");
            env::set_var("BUMACS_DATABASE_DATABASE", "test_db");
            env::set_var("BUMACS_INSTANCEID", "test-instance");
        }

        // Load config (pointing to non-existent files to force env usage)
        let config = TestConfig::load("non_existent.toml", "non_existent_dev.toml")
            .expect("Failed to load config from env");

        // Verify values
        assert_eq!(config.server.port, 9090);
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.database.url, "ws://test:8000");
        assert_eq!(config.instance_id, Some("test-instance".to_string()));

        // Clean up
        unsafe {
            env::remove_var("BUMACS_SERVER_PORT");
            env::remove_var("BUMACS_SERVER_HOST");
            env::remove_var("BUMACS_DATABASE_URL");
            env::remove_var("BUMACS_DATABASE_USERNAME");
            env::remove_var("BUMACS_DATABASE_PASSWORD");
            env::remove_var("BUMACS_DATABASE_NAMESPACE");
            env::remove_var("BUMACS_DATABASE_DATABASE");
            env::remove_var("BUMACS_INSTANCEID");
        }
    }
}
