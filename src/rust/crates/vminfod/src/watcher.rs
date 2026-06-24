// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The cache watchers: a zone state-change sysevent subscriber (the same
//! `com.sun:zones:status` channel the P2 `zoneevent` crate reads) and a
//! periodic full refresh that reconciles drift.
//!
//! On a zone transition we recompute just that zone via [`loader::refresh_one`]
//! and emit the diff; the periodic refresh catches anything the sysevent
//! stream missed (config/metadata/zfs changes that don't raise a zone-status
//! event), matching vminfod's "watchers + periodic reset" design.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::loader::{full_refresh, refresh_one};
use crate::state::Daemon;

const SYSEVENT: &str = "/usr/sbin/sysevent";
const CHANNEL: &str = "com.sun:zones:status";
const CLASS: &str = "status";
const SUBCLASS: &str = "change";

/// Default periodic full-refresh interval (node default: 5 minutes).
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Restart delay after the sysevent stream dies (matches zoneevent's retry).
const RETRY: Duration = Duration::from_millis(1000);

/// One line of `sysevent -rj` output (only the fields we act on).
#[derive(Deserialize)]
struct SysEvent {
    #[serde(rename = "type")]
    kind: String,
    data: Option<Map<String, Value>>,
}

/// Spawn the zone-status sysevent watcher loop. On each transition it
/// recomputes the affected zone and emits the diff. Runs until the process
/// exits.
pub async fn run_zone_watcher(daemon: Arc<Daemon>) {
    loop {
        if let Err(e) = watch_once(&daemon).await {
            tracing::warn!("zone watcher: {e:#}");
        }
        tokio::time::sleep(RETRY).await;
    }
}

async fn watch_once(daemon: &Arc<Daemon>) -> anyhow::Result<()> {
    let mut child = Command::new(SYSEVENT)
        .args(["-rj", "-c", CHANNEL, CLASS, SUBCLASS])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("sysevent child has no stdout"))?;
    let mut lines = BufReader::new(stdout).lines();

    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }
        if let Some(zonename) = zonename_from_event(&line) {
            // Recompute just this zone; reconcile + emit handled inside.
            refresh_one(daemon, &zonename).await;
        }
    }

    let _ = child.wait().await;
    anyhow::bail!("sysevent stream ended");
}

/// Extract the affected zonename from a sysevent line, or None for non-event
/// lines (the `ready` frame) or events without a zonename.
fn zonename_from_event(line: &str) -> Option<String> {
    let ev: SysEvent = serde_json::from_str(line).ok()?;
    if ev.kind != "event" {
        return None;
    }
    ev.data?
        .get("zonename")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Spawn the periodic full-refresh loop.
pub async fn run_periodic_refresh(daemon: Arc<Daemon>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    // Skip the immediate first tick: the initial seed already ran.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        full_refresh(&daemon).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_event_has_no_zonename() {
        let line = r#"{"date":"2026-06-23T23:32:06.226Z","type":"ready"}"#;
        assert!(zonename_from_event(line).is_none());
    }

    #[test]
    fn transition_yields_zonename() {
        let line = r#"{"type":"event","data":{"zonename":"4256eeee-c8b0-4fed-9e0a-975a31e14571","newstate":"running","oldstate":"ready","zoneid":3}}"#;
        assert_eq!(
            zonename_from_event(line).as_deref(),
            Some("4256eeee-c8b0-4fed-9e0a-975a31e14571")
        );
    }

    #[test]
    fn malformed_line_is_skipped() {
        assert!(zonename_from_event("not json").is_none());
        assert!(zonename_from_event(r#"{"type":"event"}"#).is_none());
    }
}
