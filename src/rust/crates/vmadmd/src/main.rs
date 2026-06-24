// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `vmadmd` daemon entry point.
//!
//! Runs in the foreground (SMF `child` model compatible: the binary is the
//! long-lived process under `ctrun -l child -o noorphan`). It binds the
//! control plane on `127.0.0.1:8080` (overridable for differential testing
//! against a live vmadmd via `--port`), discovers the admin IP from
//! `/usr/bin/sysinfo`, runs the autoboot scan, and serves until terminated.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use clap::Parser;
use tritond_cn_platform::smartos::Sysinfo;
use vmadmd::responsibilities::{self, AUTOBOOT_MARKER};
use vmadmd::{Daemon, Registry, VminfoClient, http};

#[derive(Parser)]
#[command(name = "vmadmd", about = "SmartOS vmadmd control daemon (bhyve + joyent-minimal)", version)]
struct Cli {
    /// Control-plane bind host. node hardcodes 127.0.0.1; overridable only for
    /// safety/testing (the listener should never leave loopback).
    #[arg(long, default_value = vmadmd::VMADMD_BIND)]
    host: String,

    /// Control-plane port. node uses 8080; an ALT port (e.g. 8081) lets a test
    /// instance coexist with the live vmadmd without disturbing it.
    #[arg(long, default_value_t = vmadmd::VMADMD_PORT)]
    port: u16,

    /// vminfod host (node `VM.load`/`VM.lookup` source).
    #[arg(long, default_value = "127.0.0.1")]
    vminfo_host: String,

    /// vminfod port.
    #[arg(long, default_value_t = 9090)]
    vminfo_port: u16,

    /// Override the discovered admin IP (for VNC/console listener binds). When
    /// unset, the admin IP comes from `/usr/bin/sysinfo`.
    #[arg(long)]
    admin_ip: Option<IpAddr>,

    /// Skip the autoboot scan at startup (test instances should not start or
    /// reload listeners for VMs the live vmadmd already owns).
    #[arg(long, default_value_t = false)]
    no_autoboot: bool,

    /// Autoboot-marker path (node /tmp/.autoboot_vmadmd). Overridable so a test
    /// instance does not clobber the live marker.
    #[arg(long, default_value = AUTOBOOT_MARKER)]
    autoboot_marker: String,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    // Discover the admin IP unless overridden. node: loadConfig ->
    // netconfig.adminIpFromSysinfo(sysinfo); a missing admin_ip is a warning,
    // not fatal (listeners just cannot bind).
    let admin_ip = match cli.admin_ip {
        Some(ip) => Some(ip),
        None => match Sysinfo::collect().await {
            Ok(si) => match si.admin_ip() {
                Some(ip) => Some(IpAddr::V4(ip)),
                None => {
                    tracing::warn!("could not find admin IP in sysinfo; display/console listeners disabled");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "sysinfo failed; display/console listeners disabled");
                None
            }
        },
    };

    let registry = Registry::new();
    let user_agent = format!("vmadmd/{}", std::process::id());
    let vminfo = VminfoClient::new(cli.vminfo_host.clone(), cli.vminfo_port, user_agent);
    let daemon = Arc::new(Daemon::new(registry.clone(), admin_ip, vminfo.clone()));

    eprintln!(
        "vmadmd: starting control plane on {}:{} (vminfod {}:{}, admin_ip {})",
        cli.host,
        cli.port,
        cli.vminfo_host,
        cli.vminfo_port,
        admin_ip
            .map(|i| i.to_string())
            .unwrap_or_else(|| "none".to_string()),
    );

    // Autoboot scan: start autoboot VMs and spawn proxies for running ones.
    if cli.no_autoboot {
        tracing::info!("autoboot disabled (--no-autoboot)");
    } else {
        match vminfo.lookup_all().await {
            Ok(serde_json::Value::Array(vms)) => {
                let marker_path = std::path::Path::new(&cli.autoboot_marker);
                let marker_present = marker_path.exists();
                responsibilities::take_autoboot_marker(marker_path);
                responsibilities::run_autoboot(&daemon, &registry, admin_ip, &vms, marker_present)
                    .await;
            }
            Ok(_) => tracing::warn!("vminfod /vms did not return an array; skipping autoboot"),
            Err(e) => tracing::warn!(error = %e, "vminfod lookup failed; skipping autoboot"),
        }
    }

    // Bind and serve the control plane (loopback only).
    let bind: SocketAddr = match format!("{}:{}", cli.host, cli.port).parse() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("vmadmd: invalid bind {}:{}: {e}", cli.host, cli.port);
            return std::process::ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vmadmd: failed to bind {bind}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    tracing::info!(%bind, "control plane listening");

    let app = http::router(daemon);
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("vmadmd: server error: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
