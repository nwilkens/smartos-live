// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The `/events` change-stream: record construction + fan-out.
//!
//! Records are newline-delimited JSON (NDJSON), NOT SSE: each event is one
//! `JSON.stringify(obj)` on its own line terminated by `\n`. vminfod
//! serializes each event exactly once and fans it out to every listener; we
//! reproduce that with a single `tokio::sync::broadcast` carrying the
//! already-serialized line. The `ready` record's `vms` field is a
//! JSON-encoded STRING (doubly-encoded) for metadata-agent compatibility
//! (`parseReady:true` JSON.parses it again).

use chrono::SecondsFormat;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::diff::Change;

/// The broadcast capacity. A burst of zone changes is rare; 1024 buffered
/// lines is ample and a lagging slow client simply drops intermediate lines
/// (node's socket buffering has the same effect under backpressure).
const CHANNEL_CAP: usize = 1024;

/// The fan-out hub. The daemon emits create/modify/delete lines here; each
/// `/events` connection subscribes and writes received lines verbatim.
#[derive(Debug, Clone)]
pub struct EventHub {
    tx: broadcast::Sender<String>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    #[must_use]
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAP);
        Self { tx }
    }

    /// A receiver for a new `/events` connection.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    /// Emit one already-serialized event line (no trailing newline; the
    /// writer adds it). Returns the listener count, or 0 if none.
    pub fn emit(&self, line: String) -> usize {
        self.tx.send(line).unwrap_or(0)
    }
}

/// ISO8601 with millisecond precision and a literal `Z`, matching node's
/// `new Date().toISOString()` (and the vmadm-get serializer).
#[must_use]
pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Build a `create` record: `{type, date, zonename, uuid, vm}`.
#[must_use]
pub fn create_record(zonename: &str, uuid: &str, vm: &Value) -> Value {
    json!({
        "type": "create",
        "date": now_iso8601(),
        "zonename": zonename,
        "uuid": uuid,
        "vm": vm,
    })
}

/// Build a `modify` record: `{type, date, zonename, uuid, vm, changes}`.
#[must_use]
pub fn modify_record(zonename: &str, uuid: &str, vm: &Value, changes: &[Change]) -> Value {
    json!({
        "type": "modify",
        "date": now_iso8601(),
        "zonename": zonename,
        "uuid": uuid,
        "vm": vm,
        "changes": changes,
    })
}

/// Build a `delete` record: `{type, date, zonename, uuid}` (no vm, no changes).
#[must_use]
pub fn delete_record(zonename: &str, uuid: &str) -> Value {
    json!({
        "type": "delete",
        "date": now_iso8601(),
        "zonename": zonename,
        "uuid": uuid,
    })
}

/// Build the `ready` record sent once on connect. `vms_json` is the
/// already-serialized `/vms` array string; it is embedded as a JSON STRING
/// (doubly-encoded), which the metadata agent re-parses.
#[must_use]
pub fn ready_record(uuid: &str, date: &str, vms_json: &str) -> Value {
    json!({
        "type": "ready",
        "date": date,
        "uuid": uuid,
        "vms": vms_json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_vms_is_doubly_encoded_string() {
        let rec = ready_record("conn-uuid", "2026-06-24T00:00:00.000Z", "[]");
        // vms must be a JSON string, not an array.
        assert!(rec["vms"].is_string());
        assert_eq!(rec["vms"], json!("[]"));
        assert_eq!(rec["type"], json!("ready"));
        let s = serde_json::to_string(&rec).expect("serialize");
        // Doubly-encoded: the empty array shows up as the escaped string "[]".
        assert!(s.contains(r#""vms":"[]""#), "got {s}");
    }

    #[test]
    fn delete_has_no_vm_or_changes() {
        let rec = delete_record("z", "z");
        assert!(rec.get("vm").is_none());
        assert!(rec.get("changes").is_none());
        assert_eq!(rec["type"], json!("delete"));
    }

    #[test]
    fn modify_carries_changes() {
        let vm = json!({"zonename": "z"});
        let changes = vec![Change {
            pretty_path: "state".to_string(),
            path: vec![json!("state")],
            action: "changed",
            old_value: Some(json!("stopped")),
            new_value: Some(json!("running")),
            ident: None,
        }];
        let rec = modify_record("z", "z", &vm, &changes);
        assert_eq!(rec["type"], json!("modify"));
        assert_eq!(rec["changes"][0]["prettyPath"], json!("state"));
        assert_eq!(rec["changes"][0]["action"], json!("changed"));
    }

    #[tokio::test]
    async fn hub_fans_out_once() {
        let hub = EventHub::new();
        let mut r1 = hub.subscribe();
        let mut r2 = hub.subscribe();
        assert_eq!(hub.emit("line".to_string()), 2);
        assert_eq!(r1.recv().await.expect("r1"), "line");
        assert_eq!(r2.recv().await.expect("r2"), "line");
    }
}
