use include_dir::include_dir;
use keystone::{config::{Config, DatabaseConfig, SecurityConfig, ServerConfig}, Application};
use serde::{Deserialize, Serialize};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Serialize, Deserialize)]
struct AppConfig {
    server: ServerConfig,
    database: DatabaseConfig,
    security: SecurityConfig,
    instance_id: Option<String>,
}

impl Config for AppConfig {
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

#[tokio::main]
async fn main() {
    // ----- Initialize logging -----
    {
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer())
            .with(tracing_subscriber::filter::EnvFilter::new(
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
            ))
            .init();

        let name = env!("CARGO_PKG_NAME").to_string();
        let version = env!("CARGO_PKG_VERSION").to_string();
        let target_os = std::env::consts::OS;
        let target_arch = std::env::consts::ARCH;
        tracing::info!(
            "Running {} v{} for {} on {}",
            name,
            version,
            target_os,
            target_arch
        );
    }

    let config = AppConfig::load("/etc/keystone/config.toml", "config.toml").unwrap();
    let app = Application::new()
        .with_config(config)
        .with_migrations(include_dir!("$CARGO_MANIFEST_DIR/migrations"));
    if let Err(err) = app.run_migrations().await {
        tracing::error!("Failed to run migrations: {}", err);
        std::process::exit(1);
    }
    // app.start().await;
}
