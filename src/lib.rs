use std::process::ExitCode;

use include_dir::Dir;
use tracing::info;

use crate::{config::Config, db::{migrations::Migrations, Database}};

pub mod db;
pub mod config;

pub struct WithConfig<C: Config> {
    pub config: C,
}
pub struct WithoutConfig;
pub struct WithMigrations(pub Migrations);
pub struct WithoutMigrations;

pub struct Application<ConfigState = WithoutConfig, MigrationsState = WithoutMigrations> {
    config: ConfigState,
    migrations: MigrationsState,
}

impl<ConfigState, MigrationsState> Application<ConfigState, MigrationsState> {
    pub fn with_config<C: Config>(self, config: C) -> Application<WithConfig<C>, MigrationsState> {
        Application {
            config: WithConfig { config },
            migrations: self.migrations,
        }
    }

    pub fn with_migrations(self, migrations_dir: Dir<'static>) -> Application<ConfigState, WithMigrations> {
        Application {
            config: self.config,
            migrations: WithMigrations(Migrations::new(migrations_dir)),
        }
    }
}

impl Application {
    pub fn new() -> Self {
        Application {
            config: WithoutConfig,
            migrations: WithoutMigrations,
        }
    }
}

impl<C: Config> Application<WithConfig<C>, WithMigrations> {
    pub async fn start(&self) -> ExitCode{
        // ----- Connect to the database -----
        let db_client = match Database::connect(&self.config.config.get_database_config()).await {
            Ok(db) => db,
            Err(err) => {
                tracing::error!("Failed to connect to database: {}", err);
                return ExitCode::FAILURE;
            }
        };

        // ----- Check database migrations -----
        match self.migrations.0.has_pending_migrations(&db_client).await {
            Ok(true) => {
                tracing::error!(
                    "Database is not up to date"
                );
                return ExitCode::FAILURE;
            }
            Err(err) => {
                tracing::error!("Failed to check database migrations: {}", err);
                return ExitCode::FAILURE;
            }
            _ => {}
        }
        ExitCode::SUCCESS
    }

    pub async fn run_migrations(&self) -> Result<(), Box<dyn std::error::Error>> {
        info!("Running migrations...");
        // ----- Connect to the database -----
        let db_client = match Database::connect(&self.config.config.get_database_config()).await {
            Ok(db) => db,
            Err(err) => {
                return Err(err.into());
            }
        };

        self.migrations.0.clone().run_migrations(&db_client, self.config.config.get_instance_id().unwrap_or_else(|| "default".to_string())).await?;
        Ok(())
    }
}

impl<WithConfig, WithoutMigrations> Application<WithConfig, WithoutMigrations> {

}