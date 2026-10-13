//! A connection whose peer vanished without a FIN/RST: DbHandle's watchdog
//! must replace it. Runs only with KEYSTONE_TEST_SURREAL_URL set, e.g.
//! `ws://127.0.0.1:8000` for `surreal start --user root --pass root memory`.
//!
//! A proxy between the client and the server black-holes the connection: it
//! stops forwarding in both directions but keeps both sockets open, so the
//! client sees neither a response nor a close, as when the server's pod IP
//! disappears. New connections are forwarded again, as when the server is back
//! on a new IP.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use keystone::config::DatabaseConfig;
use keystone::db::{DbHandle, WatchdogConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A TCP proxy whose current connections can be black-holed.
struct Proxy {
    addr: String,
    blackhole: Arc<AtomicBool>,
}

impl Proxy {
    async fn start(upstream: String) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let blackhole = Arc::new(AtomicBool::new(false));
        let bh = blackhole.clone();
        tokio::spawn(async move {
            loop {
                let (client, _) = listener.accept().await.unwrap();
                if bh.load(Ordering::SeqCst) {
                    // The server is unreachable: refuse new connections, as a
                    // Service without endpoints does.
                    drop(client);
                    continue;
                }
                let up = TcpStream::connect(&upstream).await.unwrap();
                // Every connection gets its own flag: black-holing affects only
                // the connections open at the time.
                let mine = Arc::new(AtomicBool::new(false));
                let (cr, cw) = client.into_split();
                let (ur, uw) = up.into_split();
                tokio::spawn(pipe(cr, uw, mine.clone(), bh.clone()));
                tokio::spawn(pipe(ur, cw, mine, bh.clone()));
            }
        });
        Proxy { addr, blackhole }
    }
}

async fn pipe(
    mut r: tokio::net::tcp::OwnedReadHalf,
    mut w: tokio::net::tcp::OwnedWriteHalf,
    mine: Arc<AtomicBool>,
    global: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if global.load(Ordering::SeqCst) {
            mine.store(true, Ordering::SeqCst);
        }
        if mine.load(Ordering::SeqCst) {
            // Black-holed: hold both halves open for ever, forward nothing.
            std::future::pending::<()>().await;
        }
        let n = match tokio::time::timeout(Duration::from_millis(100), r.read(&mut buf)).await {
            Err(_) => continue,
            Ok(Ok(0)) | Ok(Err(_)) => return,
            Ok(Ok(n)) => n,
        };
        if global.load(Ordering::SeqCst) || mine.load(Ordering::SeqCst) {
            mine.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
        if w.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

fn config(url: String) -> DatabaseConfig {
    DatabaseConfig {
        url,
        username: "root".into(),
        password: "root".into(),
        namespace: "keystone_test".into(),
        database: "dead_peer".into(),
    }
}

async fn ask(db: &surrealdb::Surreal<surrealdb::engine::any::Any>) -> Result<i64, String> {
    let mut r = db.query("RETURN 7").await.map_err(|e| e.to_string())?;
    r.take::<Option<i64>>(0)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no value".into())
}

#[tokio::test]
async fn the_watchdog_replaces_a_black_holed_connection() {
    let Ok(url) = std::env::var("KEYSTONE_TEST_SURREAL_URL") else {
        eprintln!("KEYSTONE_TEST_SURREAL_URL not set; skipping");
        return;
    };
    let upstream = url
        .trim_start_matches("ws://")
        .trim_end_matches('/')
        .to_string();
    let proxy = Proxy::start(upstream).await;
    let watchdog = WatchdogConfig {
        interval: Duration::from_secs(1),
        probe_timeout: Duration::from_secs(1),
        misses: 2,
        connect_timeout: Duration::from_secs(2),
        max_backoff: Duration::from_secs(2),
    };
    let handle = DbHandle::connect_with(&config(format!("ws://{}", proxy.addr)), watchdog)
        .await
        .unwrap();
    assert_eq!(ask(&handle.get()).await, Ok(7));

    // The peer vanishes: the open connection is black-holed and new ones are
    // refused for a while, then the server is back (new connections work).
    proxy.blackhole.store(true, Ordering::SeqCst);
    let dead = handle.get();
    let hung = tokio::time::timeout(Duration::from_secs(3), ask(&dead)).await;
    assert!(
        hung.is_err(),
        "the black-holed connection must hang, got {hung:?}"
    );
    proxy.blackhole.store(false, Ordering::SeqCst);

    let t = Instant::now();
    let back = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(Ok(v)) =
                tokio::time::timeout(Duration::from_secs(1), ask(&handle.get())).await
            {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    eprintln!("recovered: {back:?} after {:?}", t.elapsed());
    assert_eq!(back, Ok(7), "calls through the handle must work again");
    assert!(handle.reconnects() >= 1);
    assert!(!std::sync::Arc::ptr_eq(&dead, &handle.get()));
}
