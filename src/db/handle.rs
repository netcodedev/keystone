//! A database handle whose connection can be replaced while the service runs.
//!
//! A SurrealDB client cannot be reconnected in place, and its WebSocket engine
//! does not notice a peer that vanished without a FIN/RST (a pod IP that went
//! away): every call on such a connection hangs until the OS gives up, which
//! takes minutes. [`DbHandle`] holds the current client behind a lock, and a
//! watchdog probes it; when the probe keeps failing, the watchdog opens a new
//! connection and swaps it in. Calls started after the swap use the new
//! connection. A call already in flight on the dead one is not cancelled.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};

use axum::extract::FromRef;
use surrealdb::{Surreal, engine::any::Any};
use tokio::time::{Duration, sleep, timeout};

use crate::config::DatabaseConfig;
use crate::db::{DBError, Database};

/// How the watchdog probes the connection and reconnects.
#[derive(Debug, Clone)]
pub struct WatchdogConfig {
    /// Time between two probes.
    pub interval: Duration,
    /// How long a probe may take before it counts as a miss.
    pub probe_timeout: Duration,
    /// Consecutive misses after which the connection is replaced.
    pub misses: u32,
    /// How long one reconnect attempt (connect, sign in, select ns/db) may take.
    pub connect_timeout: Duration,
    /// Upper bound of the delay between failed reconnect attempts.
    pub max_backoff: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        WatchdogConfig {
            interval: Duration::from_secs(5),
            probe_timeout: Duration::from_secs(5),
            misses: 2,
            connect_timeout: Duration::from_secs(10),
            max_backoff: Duration::from_secs(30),
        }
    }
}

struct Inner {
    current: RwLock<Arc<Surreal<Any>>>,
    reconnects: AtomicU64,
}

/// A cheap-to-clone handle to the current database client.
///
/// Take the client with [`DbHandle::get`] for each request or loop cycle and
/// do not keep it: a kept client stays on its connection after a swap. As
/// router state it extracts as `State<Arc<Surreal<Any>>>`, which does exactly
/// that per request.
#[derive(Clone)]
pub struct DbHandle {
    inner: Arc<Inner>,
}

impl DbHandle {
    /// Connect as [`Database::connect`] does and watch the connection with the
    /// default [`WatchdogConfig`].
    pub async fn connect(config: &DatabaseConfig) -> Result<Self, DBError> {
        Self::connect_with(config, WatchdogConfig::default()).await
    }

    /// Connect and watch the connection with `watchdog`. Embedded engines
    /// (`mem://`, `surrealkv://`, ...) have no connection to lose and are not
    /// watched.
    pub async fn connect_with(
        config: &DatabaseConfig,
        watchdog: WatchdogConfig,
    ) -> Result<Self, DBError> {
        let db = connect_bounded(config, watchdog.connect_timeout).await?;
        let handle = DbHandle::fixed(db);
        if is_remote(&config.url) {
            tokio::spawn(run_watchdog(
                Arc::downgrade(&handle.inner),
                config.clone(),
                watchdog,
            ));
        }
        Ok(handle)
    }

    /// A handle on `db` without a watchdog, e.g. for tests or embedded engines.
    pub fn fixed(db: Surreal<Any>) -> Self {
        DbHandle::from(Arc::new(db))
    }

    /// The current client.
    pub fn get(&self) -> Arc<Surreal<Any>> {
        self.inner.current()
    }

    /// How many times the watchdog has replaced the connection.
    pub fn reconnects(&self) -> u64 {
        self.inner.reconnects.load(Ordering::Relaxed)
    }
}

impl From<Arc<Surreal<Any>>> for DbHandle {
    fn from(db: Arc<Surreal<Any>>) -> Self {
        DbHandle {
            inner: Arc::new(Inner {
                current: RwLock::new(db),
                reconnects: AtomicU64::new(0),
            }),
        }
    }
}

impl FromRef<DbHandle> for Arc<Surreal<Any>> {
    fn from_ref(handle: &DbHandle) -> Self {
        handle.get()
    }
}

fn is_remote(url: &str) -> bool {
    ["ws://", "wss://", "http://", "https://"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
}

async fn connect_bounded(
    config: &DatabaseConfig,
    limit: Duration,
) -> Result<Surreal<Any>, DBError> {
    match timeout(limit, Database::connect(config)).await {
        Ok(result) => result,
        Err(_) => Err(DBError::Database(format!(
            "Connecting to SurrealDB at '{}' timed out after {} s",
            config.url,
            limit.as_secs_f32()
        ))),
    }
}

impl Inner {
    fn replace(&self, db: Surreal<Any>) {
        let db = Arc::new(db);
        match self.current.write() {
            Ok(mut current) => *current = db,
            Err(poisoned) => *poisoned.into_inner() = db,
        }
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    fn current(&self) -> Arc<Surreal<Any>> {
        match self.current.read() {
            Ok(db) => db.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// Probe the connection every `interval`; after `misses` failed probes in a
/// row, reconnect (with backoff) and swap the new client in. Ends once every
/// handle is dropped.
async fn run_watchdog(inner: Weak<Inner>, config: DatabaseConfig, cfg: WatchdogConfig) {
    let mut misses = 0u32;
    loop {
        sleep(cfg.interval).await;
        let Some(db) = inner.upgrade().map(|i| i.current()) else {
            return;
        };
        let probe = timeout(cfg.probe_timeout, db.health()).await;
        drop(db);
        match probe {
            Ok(Ok(())) => {
                misses = 0;
                continue;
            }
            Ok(Err(e)) => tracing::warn!("Database probe failed: {e}"),
            Err(_) => tracing::warn!(
                "Database probe got no answer within {} s",
                cfg.probe_timeout.as_secs_f32()
            ),
        }
        misses += 1;
        if misses < cfg.misses {
            continue;
        }

        tracing::warn!(
            "Database connection presumed dead after {misses} failed probes; reconnecting"
        );
        let mut backoff = cfg.interval;
        loop {
            if inner.strong_count() == 0 {
                return;
            }
            match connect_bounded(&config, cfg.connect_timeout).await {
                Ok(db) => {
                    let Some(inner) = inner.upgrade() else {
                        return;
                    };
                    inner.replace(db);
                    tracing::info!("Database connection replaced");
                    misses = 0;
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        "Database reconnect failed: {e}; retrying in {} s",
                        backoff.as_secs_f32()
                    );
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(cfg.max_backoff);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;

    async fn mem() -> Surreal<Any> {
        let db = surrealdb::engine::any::connect("mem://").await.unwrap();
        db.use_ns("t").use_db("t").await.unwrap();
        db
    }

    #[tokio::test]
    async fn get_returns_the_replaced_client() {
        let handle = DbHandle::fixed(mem().await);
        let before = handle.get();
        handle.inner.replace(mem().await);
        assert!(!Arc::ptr_eq(&before, &handle.get()));
        assert_eq!(handle.reconnects(), 1);
    }

    #[tokio::test]
    async fn router_state_extracts_the_current_client() {
        use axum::{Router, body::Body, http::Request, routing::get};
        use tower::ServiceExt;

        async fn ping(State(db): State<Arc<Surreal<Any>>>) -> String {
            let v: Option<i64> = db.query("RETURN 7").await.unwrap().take(0).unwrap();
            v.unwrap().to_string()
        }
        let handle = DbHandle::fixed(mem().await);
        let app: Router = Router::new().route("/", get(ping)).with_state(handle);
        let res = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[test]
    fn only_network_engines_are_watched() {
        assert!(is_remote("ws://db:8000"));
        assert!(is_remote("wss://db"));
        assert!(is_remote("http://db:8000"));
        assert!(!is_remote("mem://"));
        assert!(!is_remote("memory"));
        assert!(!is_remote("surrealkv://data"));
    }
}
