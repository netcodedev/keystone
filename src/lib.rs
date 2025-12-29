use std::process::ExitCode;

use axum::{Router, routing::MethodRouter};
use include_dir::Dir;
use tokio::net::TcpListener;
use tracing::info;

use crate::{
    config::Config,
    db::{Database, migrations::Migrations},
};

pub mod config;
pub mod db;

pub struct WithConfig<C: Config> {
    pub config: C,
}
pub struct WithoutConfig;
pub struct WithMigrations(pub Migrations);
pub struct WithoutMigrations;
pub struct WithRouter(pub Router);
pub struct WithoutRouter;

pub struct Application<
    ConfigState = WithoutConfig,
    MigrationsState = WithoutMigrations,
    RouterState = WithoutRouter,
> {
    config: ConfigState,
    migrations: MigrationsState,
    router: RouterState,
}

impl<ConfigState, MigrationsState, RouterState>
    Application<ConfigState, MigrationsState, RouterState>
{
    pub fn with_config<C: Config>(
        self,
        config: C,
    ) -> Application<WithConfig<C>, MigrationsState, RouterState> {
        Application {
            config: WithConfig { config },
            migrations: self.migrations,
            router: self.router,
        }
    }

    pub fn with_migrations(
        self,
        migrations_dir: Dir<'static>,
    ) -> Application<ConfigState, WithMigrations, RouterState> {
        Application {
            config: self.config,
            migrations: WithMigrations(Migrations::new(migrations_dir)),
            router: self.router,
        }
    }
}

impl Application {
    pub fn new() -> Self {
        Application {
            config: WithoutConfig,
            migrations: WithoutMigrations,
            router: WithoutRouter,
        }
    }
}

impl<C: Config> Application<WithConfig<C>, WithMigrations, WithoutRouter> {
    pub async fn start(&self) -> ExitCode {
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
                tracing::error!("Database is not up to date");
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
}

impl<C: Config, RouterState> Application<WithConfig<C>, WithMigrations, RouterState> {
    pub async fn run_migrations(&self) -> Result<(), Box<dyn std::error::Error>> {
        info!("Running migrations...");
        // ----- Connect to the database -----
        let db_client = match Database::connect(&self.config.config.get_database_config()).await {
            Ok(db) => db,
            Err(err) => {
                return Err(err.into());
            }
        };

        if let Some(instance_id) = self.config.config.get_instance_id() {
            self.migrations
                .0
                .clone()
                .run_migrations(&db_client, instance_id)
                .await?;
        }
        Ok(())
    }

    pub async fn unlock(&self, force: bool) -> Result<(), Box<dyn std::error::Error>> {
        // ----- Connect to the database -----
        let db_client = match Database::connect(&self.config.config.get_database_config()).await {
            Ok(db) => db,
            Err(err) => {
                return Err(err.into());
            }
        };

        if Database::is_locked(&db_client).await? {
            if force {
                info!("Unlocking database (FORCE)...");
                Database::force_unlock(&db_client).await?;
            } else {
                return Err("Database is locked. Use --force to unlock.".into());
            }
        } else {
            info!("Database is not locked.");
        }
        Ok(())
    }
}

impl<C: Config> Application<WithConfig<C>, WithoutMigrations, WithoutRouter> {
    pub async fn start(&self) -> ExitCode {
        // ----- Connect to the database -----
        match Database::connect(&self.config.config.get_database_config()).await {
            Ok(_) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!("Failed to connect to database: {}", err);
                ExitCode::FAILURE
            }
        }
    }
}

impl<ConfigState, MigrationsState> Application<ConfigState, MigrationsState, WithoutRouter> {
    pub fn route(
        self,
        path: &str,
        method_router: MethodRouter,
    ) -> Application<ConfigState, MigrationsState, WithRouter> {
        let router = Router::new().route(path, method_router);
        Application {
            config: self.config,
            migrations: self.migrations,
            router: WithRouter(router),
        }
    }
}

impl<ConfigState, MigrationsState> Application<ConfigState, MigrationsState, WithRouter> {
    pub fn route(
        self,
        path: &str,
        method_router: MethodRouter,
    ) -> Application<ConfigState, MigrationsState, WithRouter> {
        let router = self.router.0.route(path, method_router);
        Application {
            config: self.config,
            migrations: self.migrations,
            router: WithRouter(router),
        }
    }
}

impl<C: Config> Application<WithConfig<C>, WithMigrations, WithRouter> {
    pub async fn start(&self) -> ExitCode {
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
                tracing::error!("Database is not up to date");
                return ExitCode::FAILURE;
            }
            Err(err) => {
                tracing::error!("Failed to check database migrations: {}", err);
                return ExitCode::FAILURE;
            }
            _ => {}
        }

        // ----- Start Server -----
        let server_config = self.config.config.get_server_config();
        let addr = format!("{}:{}", server_config.host, server_config.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("Failed to bind to address {}: {}", addr, e);
                return ExitCode::FAILURE;
            }
        };
        tracing::info!("Listening on {}", addr);

        if let Err(e) = axum::serve(listener, self.router.0.clone()).await {
            tracing::error!("Server error: {}", e);
            return ExitCode::FAILURE;
        }

        ExitCode::SUCCESS
    }
}

impl<C: Config> Application<WithConfig<C>, WithoutMigrations, WithRouter> {
    pub async fn start(&self) -> ExitCode {
        // ----- Connect to the database -----
        match Database::connect(&self.config.config.get_database_config()).await {
            Ok(_) => {}
            Err(err) => {
                tracing::error!("Failed to connect to database: {}", err);
                return ExitCode::FAILURE;
            }
        }

        // ----- Start Server -----
        let server_config = self.config.config.get_server_config();
        let addr = format!("{}:{}", server_config.host, server_config.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("Failed to bind to address {}: {}", addr, e);
                return ExitCode::FAILURE;
            }
        };
        tracing::info!("Listening on {}", addr);

        if let Err(e) = axum::serve(listener, self.router.0.clone()).await {
            tracing::error!("Server error: {}", e);
            return ExitCode::FAILURE;
        }

        ExitCode::SUCCESS
    }
}
