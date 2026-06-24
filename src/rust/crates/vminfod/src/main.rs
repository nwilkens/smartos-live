// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `vminfod` daemon entry point.
//!
//! Runs in the foreground (SMF-compatible: SMF's `child` model and the start
//! method polling `/ping` both work with a foreground process). Seeds the VM
//! cache, spawns the zone-state watcher + periodic refresh, then serves HTTP.

use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use tracing::info;
use vminfod::Daemon;
use vminfod::fswatcher::run_fs_watcher;
use vminfod::loader::full_refresh;
use vminfod::server::router;
use vminfod::watcher::{DEFAULT_REFRESH_INTERVAL, run_periodic_refresh, run_zone_watcher};

#[derive(Parser)]
#[command(name = "vminfod", about = "SmartOS VM info caching daemon", version)]
struct Cli {
    /// Bind address (node binds 127.0.0.1:9090). An alternate port is used
    /// for differential testing against the live daemon.
    #[arg(long, default_value = "127.0.0.1:9090")]
    listen: SocketAddr,

    /// Periodic full-refresh interval in seconds (node default: 300).
    #[arg(long, default_value_t = 300)]
    refresh_secs: u64,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let daemon = Arc::new(Daemon::new());

    info!("vminfod starting; binding {}", cli.listen);

    // Seed the cache before we start serving so the first /ping->/vms a
    // client makes already reflects the host (node starts watchers then
    // refreshes; we seed synchronously to avoid an empty /vms window).
    full_refresh(&daemon).await;
    daemon.set_status("running", "working");
    info!(
        "initial cache seeded: {} VMs",
        daemon.with_inner(|i| i.cache.len())
    );

    // Watchers: zone state-change sysevents + per-VM fs watcher (instant
    // mdata visibility) + periodic full refresh.
    tokio::spawn(run_zone_watcher(daemon.clone()));
    tokio::spawn(run_fs_watcher(daemon.clone()));
    let refresh_interval = if cli.refresh_secs == 0 {
        DEFAULT_REFRESH_INTERVAL
    } else {
        std::time::Duration::from_secs(cli.refresh_secs)
    };
    tokio::spawn(run_periodic_refresh(daemon.clone(), refresh_interval));

    let app = router(daemon);

    let listener = match tokio::net::TcpListener::bind(cli.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vminfod: failed to bind {}: {e}", cli.listen);
            return std::process::ExitCode::FAILURE;
        }
    };
    info!("listening on http://{}", cli.listen);

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("vminfod: server error: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
