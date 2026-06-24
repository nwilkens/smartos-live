// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `metadata` daemon entry point.
//!
//! Runs in the foreground (SMF `child` model compatible). By default it serves
//! every zone on its real `/var/zonecontrol/<uuid>/metadata.sock` (OS) or
//! com2 serial socket (HVM). The `--socket`/`--zone` flags run a single-zone
//! TEST instance on an alternate socket path, used for differential testing
//! against the live service without binding the real sockets.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use metadata::handler::ReadOnlyWriter;
use metadata::{Agent, Config};

#[derive(Parser)]
#[command(name = "metadata", about = "SmartOS metadata agent", version)]
struct Cli {
    /// vminfod host.
    #[arg(long, default_value = "127.0.0.1")]
    vminfo_host: String,

    /// vminfod port.
    #[arg(long, default_value_t = 9090)]
    vminfo_port: u16,

    /// OS-zone socket base directory (production: /var/zonecontrol).
    #[arg(long, default_value = "/var/zonecontrol")]
    zonecontrol_dir: PathBuf,

    /// TEST MODE: serve only this zone uuid.
    #[arg(long)]
    zone: Option<String>,

    /// TEST MODE: bind this exact OS-zone socket path (with --zone), instead
    /// of the real /var/zonecontrol path. Leaves the live service untouched.
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    let cfg = Config {
        vminfo_host: cli.vminfo_host,
        vminfo_port: cli.vminfo_port,
        zonecontrol_dir: cli.zonecontrol_dir,
        only_zone: cli.zone,
        socket_override: cli.socket,
    };

    eprintln!(
        "metadata: starting (vminfod {}:{}{})",
        cfg.vminfo_host,
        cfg.vminfo_port,
        cfg.only_zone
            .as_ref()
            .map(|z| format!(", test zone {z}"))
            .unwrap_or_default()
    );

    // PUT/DELETE write-back is not yet wired to the vmadm update path; reads
    // (GET/KEYS) are fully served. A write returns a FAILURE with a clear
    // message rather than silently succeeding.
    let writer = Arc::new(ReadOnlyWriter);
    let mut agent = Agent::new(cfg, writer);

    if let Err(e) = agent.run().await {
        eprintln!("metadata: fatal: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
