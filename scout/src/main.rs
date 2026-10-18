//! Scout entry point.

use scout::config::Config;
use scout::server;
use std::sync::atomic::Ordering;

fn main() {
    // `scout mcp`: Model Context Protocol over stdio. The HTTP server still
    // runs, on a private loopback port, and the bridge talks to it.
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Scout could not start the async runtime: {e}");
                std::process::exit(1);
            }
        };
        if let Err(e) = runtime.block_on(scout::mcp::run()) {
            eprintln!("Scout mcp bridge error: {e}");
            std::process::exit(1);
        }
        return;
    }

    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Scout configuration error: {e}");
            // EX_CONFIG, matching the conventional exit code for a bad configuration.
            std::process::exit(78);
        }
    };
    scout::logger::init(&cfg.log_level);

    let dev_mode = cfg.dev_mode;
    let no_keys = cfg.api_keys.is_empty();
    let log_level = cfg.log_level.clone();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads())
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Scout could not start the async runtime: {e}");
            std::process::exit(1);
        }
    };

    runtime.block_on(async move {
        let (scout, server_task, addr) = match server::start(cfg).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Scout could not start: {e}");
                std::process::exit(1);
            }
        };

        scout::logger::info(
            "scout listening",
            serde_json::json!({
                "port": addr.port(),
                "devMode": dev_mode,
                "concurrency": scout.cfg.concurrency,
                "maxQueued": scout.cfg.max_queued,
                "workers": scout.workers.worker_count(),
            }),
        );
        if dev_mode && no_keys {
            scout::logger::warn(
                "SCOUT_DEV_MODE is on: authentication is disabled when no API keys are set",
                serde_json::json!({ "logLevel": log_level }),
            );
        }

        let sweeper = scout::scout::spawn_session_sweeper(scout.clone());

        shutdown_signal().await;
        scout::logger::info("shutting down", serde_json::json!({ "signal": "terminate" }));

        // Stop accepting new work, then let in-flight work finish within the
        // grace period before cancelling what remains.
        scout
            .shutting_down_flag()
            .store(true, Ordering::SeqCst);
        let grace = scout.cfg.shutdown_grace_ms;
        let quick = tokio::time::timeout(
            std::time::Duration::from_millis(grace),
            scout.close(),
        )
        .await;
        if quick.is_err() {
            scout::logger::warn("shutdown grace period elapsed", serde_json::json!({}));
        }
        sweeper.abort();
        server_task.abort();
    });
}

/// Keep the async pool small: the target deployment has 0.1 CPU, and the page
/// workers (which do the heavy lifting) run on their own threads.
fn worker_threads() -> usize {
    std::env::var("SCOUT_ASYNC_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
        .clamp(1, 8)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
