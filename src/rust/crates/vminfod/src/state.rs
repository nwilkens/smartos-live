// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Shared daemon state: the VM cache, lifecycle/status strings, refresh
//! bookkeeping, and the `/events` listener registry that `/status` reports.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{Map, Value, json};

use crate::cache::VmCache;
use crate::events::EventHub;
use crate::hrtime::Clock;

/// Max records kept in the refresh-error and refresh-log ring buffers
/// (node `DEFAULT_REFRESH_RECORDS`).
const REFRESH_RING: usize = 5;

/// One `/events` listener, as `/status` reports it.
#[derive(Debug, Clone)]
pub struct Listener {
    pub user_agent: Option<String>,
    pub created: Instant,
    pub created_date: String,
}

/// A refresh error record (ring buffer).
#[derive(Debug, Clone)]
pub struct RefreshError {
    pub message: String,
    pub time: Instant,
}

/// A refresh-log record (ring buffer, `?full=true` only).
#[derive(Debug, Clone)]
pub struct RefreshLogEntry {
    pub cache_changes: Vec<Value>,
    pub vm_changes: Vec<Value>,
    pub started: Instant,
    pub ended: Instant,
    pub pretty_delta: String,
}

/// The mutable inner state, behind a single mutex (contention is trivial:
/// reads are cheap snapshots, writes happen on watcher ticks).
#[derive(Debug)]
pub struct Inner {
    pub cache: VmCache,
    /// Daemon lifecycle state: "stopped"/"booting"/"paused"/"running".
    pub state: String,
    /// Free-form sub-status (node `self.status`).
    pub status: String,
    pub last_refresh: Option<Instant>,
    pub refresh_errors: Vec<RefreshError>,
    pub refresh_log: Vec<RefreshLogEntry>,
    pub listeners: BTreeMap<String, Listener>,
}

/// The shared daemon state handed to the HTTP server and the watcher.
#[derive(Debug)]
pub struct Daemon {
    pub clock: Clock,
    pub hub: EventHub,
    inner: Mutex<Inner>,
}

impl Daemon {
    #[must_use]
    pub fn new() -> Self {
        Self {
            clock: Clock::now(),
            hub: EventHub::new(),
            inner: Mutex::new(Inner {
                cache: VmCache::new(),
                state: "booting".to_string(),
                status: "initialized".to_string(),
                last_refresh: None,
                refresh_errors: Vec::new(),
                refresh_log: Vec::new(),
                listeners: BTreeMap::new(),
            }),
        }
    }

    /// Run a closure with the locked inner state. A poisoned lock is
    /// recovered (the data is still consistent; we never hold the lock across
    /// an await), avoiding an `.unwrap()` that clippy denies.
    pub fn with_inner<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }

    pub fn set_status(&self, state: &str, status: &str) {
        self.with_inner(|i| {
            i.state = state.to_string();
            i.status = status.to_string();
        });
    }

    pub fn set_substatus(&self, status: &str) {
        self.with_inner(|i| i.status = status.to_string());
    }

    pub fn register_listener(&self, uuid: String, l: Listener) {
        self.with_inner(|i| {
            i.listeners.insert(uuid, l);
        });
    }

    pub fn remove_listener(&self, uuid: &str) {
        self.with_inner(|i| {
            i.listeners.remove(uuid);
        });
    }

    pub fn record_refresh_error(&self, message: String) {
        self.with_inner(|i| {
            i.refresh_errors.push(RefreshError {
                message,
                time: Instant::now(),
            });
            let len = i.refresh_errors.len();
            if len > REFRESH_RING {
                i.refresh_errors.drain(0..len - REFRESH_RING);
            }
        });
    }

    pub fn record_refresh_log(&self, entry: RefreshLogEntry) {
        self.with_inner(|i| {
            i.last_refresh = Some(entry.ended);
            i.refresh_log.push(entry);
            let len = i.refresh_log.len();
            if len > REFRESH_RING {
                i.refresh_log.drain(0..len - REFRESH_RING);
            }
        });
    }

    /// Render the `/status` JSON object. `full` appends `refreshLog` +
    /// `fswatcher` like node's `?full=true`.
    #[must_use]
    pub fn status_json(&self, full: bool) -> Value {
        let cur_time = self.clock.cur_time();
        self.with_inner(|i| {
            let mut listeners = Map::new();
            for (uuid, l) in &i.listeners {
                listeners.insert(
                    uuid.clone(),
                    json!({
                        "userAgent": l.user_agent,
                        "createdTime": self.clock.stamp(l.created),
                        "createdDate": l.created_date,
                        "createdAgo": Clock::pretty_since(l.created),
                    }),
                );
            }

            let refresh_errors: Vec<Value> = i
                .refresh_errors
                .iter()
                .map(|e| {
                    json!({
                        "err": e.message,
                        "time": self.clock.stamp(e.time),
                        "ago": Clock::pretty_since(e.time),
                    })
                })
                .collect();

            let last_refresh = i
                .last_refresh
                .map_or(Value::Null, |t| Value::String(Clock::pretty_since(t)));

            let mut ret = json!({
                "pid": std::process::id(),
                "uptime": Clock::pretty_since(self.clock.start),
                "memory": memory_usage(),
                "state": i.state,
                "status": i.status,
                "numVms": i.cache.len(),
                "curDate": crate::events::now_iso8601(),
                "curTime": cur_time,
                "queue": {
                    "now": cur_time,
                    "paused": false,
                    "idle": true,
                    "vasync_queue": {
                        "concurrency": 1,
                        "npending": 0,
                        "nqueued": 0,
                        "pending": {},
                        "queued": [],
                    },
                },
                "lastRefresh": last_refresh,
                "refreshErrors": refresh_errors,
                "eventsListeners": Value::Object(listeners),
            });

            if full && let Value::Object(m) = &mut ret {
                let refresh_log: Vec<Value> = i
                    .refresh_log
                    .iter()
                    .map(|rl| {
                        json!({
                            "cacheChanges": rl.cache_changes,
                            "vmChanges": rl.vm_changes,
                            "started": self.clock.stamp(rl.started),
                            "ended": self.clock.stamp(rl.ended),
                            "took": rl.pretty_delta,
                            "endedAgo": Clock::pretty_since(rl.ended),
                        })
                    })
                    .collect();
                m.insert("refreshLog".to_string(), Value::Array(refresh_log));
                // No fswatcher subprocess in the Rust port: report a
                // compatible stand-in dump (the vminfo CLI guards on field
                // presence). A periodic full refresh + zoneevent stream
                // replaces the fswatcher's role.
                m.insert(
                    "fswatcher".to_string(),
                    json!({
                        "running": true,
                        "watcher_pid": std::process::id(),
                        "watching": [],
                        "not_yet_watching": [],
                        "pending_actions": {},
                    }),
                );
            }

            ret
        })
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

/// The `memory` object. node emits `process.memoryUsage()`
/// (rss/heapTotal/heapUsed). A Rust process has no V8 heap; only `rss` is
/// load-bearing (the vminfo CLI reads `memory.rss`). Report the process RSS
/// from the kernel and zero the heap counters.
fn memory_usage() -> Value {
    json!({
        "rss": process_rss_bytes(),
        "heapTotal": 0,
        "heapUsed": 0,
    })
}

/// Resident set size in bytes from the illumos `/proc/self/psinfo` `pr_rssize`
/// (KiB). Falls back to 0 if unreadable (e.g. on a non-illumos dev host).
fn process_rss_bytes() -> u64 {
    // psinfo_t layout: pr_rssize is a size_t at a fixed offset. Rather than
    // depend on the exact struct, read the human-readable `/proc/self/usage`
    // is not available portably; use psinfo's documented pr_rssize field.
    // On illumos, psinfo_t.pr_rssize sits at offset 0x2c (44) on both ILP32
    // and LP64 for the 32-bit-int fields preceding it. To stay robust we
    // instead parse the prpsinfo via the `ps`-free path below.
    read_rss_via_psinfo().unwrap_or(0)
}

#[cfg(target_os = "illumos")]
fn read_rss_via_psinfo() -> Option<u64> {
    // pr_rssize is a size_t (KiB). Its offset in psinfo_t is stable in the
    // illumos ABI: the leading fields are pr_flag(int), pr_nlwp(int),
    // pr_pid(pid_t=int), pr_ppid(int), pr_pgid(int), pr_sid(int),
    // pr_uid(uid_t=int), pr_euid(int), pr_gid(int), pr_egid(int) = 10 ints
    // (40 bytes), then pr_addr (void* — 8 on LP64), then pr_size (size_t — 8
    // on LP64), then pr_rssize (size_t — 8 on LP64). So on LP64:
    // offset = 40 + 8 + 8 = 56, in KiB.
    let bytes = std::fs::read("/proc/self/psinfo").ok()?;
    const OFF: usize = 56;
    if bytes.len() < OFF + 8 {
        return None;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[OFF..OFF + 8]);
    let rss_kib = u64::from_ne_bytes(buf);
    Some(rss_kib.saturating_mul(1024))
}

#[cfg(not(target_os = "illumos"))]
fn read_rss_via_psinfo() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_shape_has_required_keys() {
        let d = Daemon::new();
        let v = d.status_json(false);
        for k in [
            "pid", "uptime", "memory", "state", "status", "numVms", "curDate", "curTime",
            "queue", "lastRefresh", "refreshErrors", "eventsListeners",
        ] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
        // queue sub-shape the vminfo CLI reads.
        assert!(v["queue"]["paused"].is_boolean());
        assert!(v["queue"]["idle"].is_boolean());
        assert!(v["queue"]["vasync_queue"]["npending"].is_number());
        assert!(v["queue"]["vasync_queue"]["nqueued"].is_number());
        assert!(v["memory"]["rss"].is_number());
        // lastRefresh is null before first refresh.
        assert!(v["lastRefresh"].is_null());
    }

    #[test]
    fn full_status_adds_refreshlog_and_fswatcher() {
        let d = Daemon::new();
        let v = d.status_json(true);
        assert!(v.get("refreshLog").is_some());
        assert!(v["fswatcher"]["running"].is_boolean());
        assert!(v["fswatcher"]["watching"].is_array());
    }

    #[test]
    fn listeners_reported() {
        let d = Daemon::new();
        d.register_listener(
            "abc".to_string(),
            Listener {
                user_agent: Some("curl/8".to_string()),
                created: Instant::now(),
                created_date: "2026-06-24T00:00:00.000Z".to_string(),
            },
        );
        let v = d.status_json(false);
        assert_eq!(v["eventsListeners"]["abc"]["userAgent"], json!("curl/8"));
        assert!(v["eventsListeners"]["abc"]["createdAgo"].is_string());
        d.remove_listener("abc");
        let v = d.status_json(false);
        assert!(v["eventsListeners"].as_object().expect("obj").is_empty());
    }
}
