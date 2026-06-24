// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The agent orchestrator: seed the cache, watch `/events`, and run one
//! per-zone server task whose lifecycle tracks zone create/delete.
//!
//! Mirrors `agent.js` `.start()`: seed from `/vms`, then for each event
//! create -> start a server; delete -> stop it; modify -> refresh the cached
//! object (and for HVM, reconnect on boot/reboot). Each zone task owns a
//! shutdown watch so delete can stop it cleanly.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::cache::VmCache;
use crate::handler::MetadataWriter;
use crate::server;
use crate::vminfo_client::VminfoClient;

/// Configuration for the agent.
#[derive(Clone)]
pub struct Config {
    /// vminfod host.
    pub vminfo_host: String,
    /// vminfod port.
    pub vminfo_port: u16,
    /// OS-zone socket base directory (default `/var/zonecontrol`). Overridable
    /// for differential testing against a scratch path.
    pub zonecontrol_dir: PathBuf,
    /// Restrict serving to a single zone uuid (test mode); None serves all.
    pub only_zone: Option<String>,
    /// Override the OS-zone socket path entirely (test mode, single zone).
    pub socket_override: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            vminfo_host: "127.0.0.1".to_string(),
            vminfo_port: 9090,
            zonecontrol_dir: PathBuf::from(server::ZONECONTROL_DIR),
            only_zone: None,
            socket_override: None,
        }
    }
}

/// A handle to a running per-zone server task.
struct ZoneTask {
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

/// The agent. Holds the cache and live per-zone tasks.
pub struct Agent<W: MetadataWriter + Send + Sync + 'static> {
    cfg: Config,
    cache: VmCache,
    client: VminfoClient,
    writer: Arc<W>,
    zones: HashMap<String, ZoneTask>,
}

impl<W: MetadataWriter + Send + Sync + 'static> Agent<W> {
    /// Create an agent. `writer` applies PUT/DELETE (use ReadOnlyWriter if the
    /// update path is not wired).
    #[must_use]
    pub fn new(cfg: Config, writer: Arc<W>) -> Self {
        let ua = user_agent();
        let client = VminfoClient::new(cfg.vminfo_host.clone(), cfg.vminfo_port, ua);
        Self {
            cfg,
            cache: VmCache::new(),
            client,
            writer,
            zones: HashMap::new(),
        }
    }

    /// The shared cache (exposed for tests/inspection).
    #[must_use]
    pub fn cache(&self) -> VmCache {
        self.cache.clone()
    }

    /// Run forever: seed the cache, start zone servers, then process events.
    /// Reconnects to `/events` on stream loss (resyncing the cache each time).
    pub async fn run(&mut self) -> anyhow::Result<()> {
        self.seed().await?;
        self.start_initial_zones();

        loop {
            if let Err(e) = self.watch_events().await {
                error!("metadata: events stream error: {e}; resyncing in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
                // Re-seed so we don't miss create/delete during the gap.
                if let Err(se) = self.seed().await {
                    warn!("metadata: reseed failed: {se}");
                } else {
                    self.reconcile_zones();
                }
            }
        }
    }

    /// Seed the cache from `GET /vms`.
    async fn seed(&self) -> anyhow::Result<()> {
        let vms = self
            .client
            .get_json("/vms")
            .await
            .map_err(|e| anyhow::anyhow!("GET /vms: {e}"))?;
        let n = self.cache.replace_from_vms(&vms);
        info!("metadata: cache seeded with {n} VMs");
        Ok(())
    }

    /// Start a server for every cached zone we should serve.
    fn start_initial_zones(&mut self) {
        for zn in self.cache.zonenames() {
            if self.should_serve(&zn) {
                self.start_zone(&zn);
            }
        }
    }

    /// Whether `zonename` is in scope for this agent (test single-zone mode).
    fn should_serve(&self, zonename: &str) -> bool {
        self.cfg
            .only_zone
            .as_ref()
            .is_none_or(|only| only == zonename)
    }

    /// Start (or restart) the per-zone server task.
    fn start_zone(&mut self, zonename: &str) {
        if self.zones.contains_key(zonename) {
            return;
        }
        let Some(brand) = self.cache.brand(zonename) else {
            warn!("metadata: no brand for zone {zonename}; not serving");
            return;
        };

        let (tx, rx) = watch::channel(false);
        let cache = self.cache.clone();
        let writer = self.writer.clone();
        let zn = zonename.to_string();

        // A `--socket` override always forces the OS unix-server transport,
        // even for HVM brands. Resolution is transport-independent, so this
        // lets a test instance validate any zone's bytes over a scratch socket
        // without binding the real serial path. In production (no override) an
        // HVM brand uses the com2 serial client path.
        let handle = if server::is_hvm_brand(&brand) && self.cfg.socket_override.is_none() {
            let Some(zonepath) = self.cache.zonepath(zonename) else {
                warn!("metadata: no zonepath for HVM zone {zonename}; not serving");
                return;
            };
            let sock = server::hvm_socket_path(&zonepath);
            tokio::spawn(async move {
                server::serve_hvm_zone(sock, zn, cache, writer, rx).await;
            })
        } else {
            let sock = self.cfg.socket_override.clone().unwrap_or_else(|| {
                server::os_socket_path(&self.cfg.zonecontrol_dir, zonename)
            });
            tokio::spawn(async move {
                server::serve_os_zone(sock, zn, cache, writer, rx).await;
            })
        };

        self.zones
            .insert(zonename.to_string(), ZoneTask { shutdown: tx, handle });
        debug!("metadata: started server for zone {zonename} (brand {brand})");
    }

    /// Stop and remove a per-zone server task.
    fn stop_zone(&mut self, zonename: &str) {
        if let Some(task) = self.zones.remove(zonename) {
            let _ = task.shutdown.send(true);
            task.handle.abort();
            debug!("metadata: stopped server for zone {zonename}");
        }
    }

    /// After a re-seed, start servers for new zones and stop ones that vanished.
    fn reconcile_zones(&mut self) {
        let live: Vec<String> = self
            .cache
            .zonenames()
            .into_iter()
            .filter(|z| self.should_serve(z))
            .collect();
        for zn in &live {
            if !self.zones.contains_key(zn) {
                self.start_zone(zn);
            }
        }
        let running: Vec<String> = self.zones.keys().cloned().collect();
        for zn in running {
            if !live.contains(&zn) {
                self.stop_zone(&zn);
            }
        }
    }

    /// Subscribe to `/events` and process records until the stream ends.
    async fn watch_events(&mut self) -> anyhow::Result<()> {
        use tokio::io::AsyncBufReadExt;

        let reader = self
            .client
            .open_events("/events")
            .await
            .map_err(|e| anyhow::anyhow!("GET /events: {e}"))?;
        info!("metadata: watching /events");

        let mut lines = reader.lines();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| anyhow::anyhow!("events read: {e}"))?
        {
            if line.trim().is_empty() {
                continue;
            }
            let ev: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    warn!("metadata: bad event line: {e}");
                    continue;
                }
            };
            self.handle_event(&ev);
        }
        Err(anyhow::anyhow!("events stream closed"))
    }

    /// Apply one `/events` record to the cache + zone tasks.
    fn handle_event(&mut self, ev: &Value) {
        let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "ready" => self.handle_ready(ev),
            "create" => {
                if let Some(vm) = ev.get("vm") {
                    self.cache.upsert(vm);
                    if let Some(zn) = vm.get("zonename").and_then(Value::as_str)
                        && self.should_serve(zn) {
                            self.start_zone(zn);
                        }
                }
            }
            "modify" => {
                if let Some(vm) = ev.get("vm") {
                    self.cache.upsert(vm);
                    self.handle_modify(ev, vm);
                }
            }
            "delete" => {
                if let Some(zn) = ev.get("zonename").and_then(Value::as_str) {
                    self.stop_zone(zn);
                    self.cache.remove(zn);
                }
            }
            other => debug!("metadata: ignoring event type {other}"),
        }
    }

    /// The `ready` frame carries a doubly-encoded `vms` JSON string; re-parse
    /// it and resync the cache (covers events that fired before we connected).
    fn handle_ready(&mut self, ev: &Value) {
        let Some(vms_str) = ev.get("vms").and_then(Value::as_str) else {
            return;
        };
        let Ok(vms) = serde_json::from_str::<Value>(vms_str) else {
            warn!("metadata: ready.vms did not parse");
            return;
        };
        self.cache.replace_from_vms(&vms);
        self.reconcile_zones();
    }

    /// For HVM zones, a guest boot/reboot needs the serial reconnected. The
    /// serial task exits when the guest closes the stream, so a `state`->
    /// running transition (or init_restarts increment) restarts the task. For
    /// OS zones, modify only refreshes the cached object (already done).
    fn handle_modify(&mut self, ev: &Value, vm: &Value) {
        let Some(zn) = vm.get("zonename").and_then(Value::as_str) else {
            return;
        };
        if !self.should_serve(zn) {
            return;
        }
        let brand = vm.get("brand").and_then(Value::as_str).unwrap_or("");
        if !server::is_hvm_brand(brand) {
            return;
        }

        // Reconnect on a transition to running, or an init_restarts change
        // (guest reboot), if the serial task is not currently running.
        let mut reconnect = false;
        if let Some(changes) = ev.get("changes").and_then(Value::as_array) {
            for ch in changes {
                let path = ch
                    .get("path")
                    .and_then(Value::as_array)
                    .map(|p| {
                        p.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(".")
                    })
                    .unwrap_or_default();
                let new = ch.get("newValue");
                if path == "state" && new.and_then(Value::as_str) == Some("running") {
                    reconnect = true;
                }
                if path == "init_restarts" {
                    reconnect = true;
                }
            }
        }
        if reconnect && !self.zones.contains_key(zn) {
            self.start_zone(zn);
        }
    }
}

/// node makeUserAgent shape for the vminfod connection.
fn user_agent() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
    format!("metadata - {host}/{} (metadata)", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::ReadOnlyWriter;

    #[test]
    fn config_default() {
        let c = Config::default();
        assert_eq!(c.vminfo_port, 9090);
        assert_eq!(c.zonecontrol_dir, PathBuf::from("/var/zonecontrol"));
        assert!(c.only_zone.is_none());
    }

    #[test]
    fn should_serve_respects_only_zone() {
        let mut cfg = Config::default();
        cfg.only_zone = Some("z1".to_string());
        let agent = Agent::new(cfg, Arc::new(ReadOnlyWriter));
        assert!(agent.should_serve("z1"));
        assert!(!agent.should_serve("z2"));
    }
}
