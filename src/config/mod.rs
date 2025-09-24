use std::{env, fs, path::PathBuf};

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

    fn load(default_system_path: &str, local_dev_path: &str) -> Result<Self, ConfigError> where Self: Sized + for<'de> serde::Deserialize<'de>{
        // 1. Determine config file path using our helper function
        let config_path = Self::get_config_path(default_system_path, local_dev_path)?;

        tracing::debug!("Loading configuration from: {}", config_path.display());

        // 2. Read the file contents into a string
        let config_content = fs::read_to_string(&config_path).map_err(|e| {
            ConfigError::LoadError(format!(
                "Failed to read config file '{}': {}",
                config_path.display(),
                e
            ))
        })?; // Propagate I/O errors

        // 3. Parse the TOML string using toml::from_str
        let config: Self = toml::from_str(&config_content).map_err(|e| {
            ConfigError::LoadError(format!(
                "Failed to parse TOML from '{}': {}",
                config_path.display(),
                e
            ))
        })?; // Propagate TOML parsing errors

        // 4. Optional: Add validation logic here if needed
        //    e.g., check if database.url is a valid format

        Ok(config)
    }

    /// Finds the configuration file path based on environment variable or default locations.
    fn get_config_path(default_system_path: &str, local_dev_path: &str) -> Result<PathBuf, ConfigError> {
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
        let is_production = env::var(ENV_VAR_MODE)
            .map_or(true, |mode| mode.eq_ignore_ascii_case("production"));
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
            ENV_VAR_PATH,
            default_system_path,
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
}
