// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Per-VM file-system watcher (node vminfod's `FsWatcher`, vminfod.js:855).
//!
//! node watches, for every VM, the exact set of files whose contents feed the
//! reconstructed VM object: `/etc/zones/<uuid>.xml` (zone config),
//! `<zonepath>/lastexited` (exit_status / exit_timestamp),
//! `<zonepath>/config/metadata.json` (customer/internal_metadata),
//! `<zonepath>/config/routes.json` (routes), `<zonepath>/config/tags.json`
//! (tags), plus the global `/tmp/.sysinfo.json`. When any of these changes it
//! refreshes that one VM and emits the resulting modify event — restoring
//! near-instant `mdata-get` visibility after an `mdata-put` (previously only
//! the 5-minute periodic refresh made the change visible).
//!
//! The Rust port watches the directories that contain these files rather than
//! the files themselves: the metadata agent writes atomically (temp file +
//! rename), so an inode-level file watch would go stale after the first write.
//! Watching `<zonepath>`, `<zonepath>/config`, and `/etc/zones` and filtering
//! events by filename matches node's per-file semantics while staying robust
//! against rename-replace.
//!
//! Backend note: the `notify` crate has no native illumos event-port backend
//! (its `RecommendedWatcher` falls back to a 30s `PollWatcher` on illumos), so
//! we instantiate `PollWatcher` directly with a 1s interval. That restores
//! near-instant (<= ~1s) `mdata-get` visibility — far better than the prior
//! 300s periodic-refresh-only behavior — without depending on a notify backend
//! that does not exist on this platform. node's FsWatcher uses illumos event
//! ports directly (`fswatcher.js`); matching that here would mean writing a
//! native port_associate(3C) watcher, which is left as a follow-up.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{Config, Event, PollWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::loader::refresh_one;
use crate::state::Daemon;

/// Shared sysinfo cache file (node `SYSINFO_FILE`).
const SYSINFO_FILE: &str = "/tmp/.sysinfo.json";
/// Directory holding `<uuid>.xml` zone configs.
const ETC_ZONES: &str = "/etc/zones";
/// How often to reconcile the watched directory set with the live cache (a new
/// VM's zonepath must be added; a deleted VM's removed). Cheap: it only diffs
/// path sets and (un)registers the delta.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// PollWatcher poll interval. The notify default is 30s on illumos; 1s keeps
/// mdata visibility near-instant while staying cheap (it stats a small,
/// bounded set of per-VM config dirs).
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Spawn the fs-watcher loop. Runs until the process exits. Errors from the
/// underlying `notify` backend are logged and retried on the next reconcile.
pub async fn run_fs_watcher(daemon: Arc<Daemon>) {
    // notify's callback fires on its own thread; bridge raw events into async
    // via an unbounded channel so the consumer can debounce + refresh.
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    // PollWatcher explicitly: illumos has no native notify backend, and the
    // RecommendedWatcher fallback polls at 30s. A 1s poll keeps mdata visible
    // within ~1s.
    let cfg = Config::default().with_poll_interval(POLL_INTERVAL);
    let mut watcher = match PollWatcher::new(
        move |res: notify::Result<Event>| {
            if let Ok(ev) = res {
                // A closed receiver means the daemon is shutting down; drop.
                let _ = tx.send(ev);
            }
        },
        cfg,
    ) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("fswatcher: cannot create watcher: {e}");
            return;
        }
    };

    // Track which directories we currently watch so reconcile only applies the
    // delta (notify errors if you watch the same path twice or unwatch an
    // unwatched one).
    let mut watched: HashSet<PathBuf> = HashSet::new();

    // Always watch the global, VM-independent paths.
    watch_dir(&mut watcher, &mut watched, Path::new(ETC_ZONES));
    if let Some(parent) = Path::new(SYSINFO_FILE).parent() {
        watch_dir(&mut watcher, &mut watched, parent);
    }

    // Seed the per-VM directory set from the current cache.
    reconcile_watches(&daemon, &mut watcher, &mut watched);

    let mut reconcile = tokio::time::interval(RECONCILE_INTERVAL);
    reconcile.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                match maybe {
                    Some(ev) => handle_event(&daemon, ev).await,
                    None => break, // sender dropped: watcher gone
                }
            }
            _ = reconcile.tick() => {
                reconcile_watches(&daemon, &mut watcher, &mut watched);
            }
        }
    }
}

/// Watch a directory non-recursively, recording it so we never double-watch.
/// A missing directory is skipped silently (it will be retried on the next
/// reconcile once the zone's dataset is mounted).
fn watch_dir<W: Watcher>(watcher: &mut W, watched: &mut HashSet<PathBuf>, dir: &Path) {
    if watched.contains(dir) {
        return;
    }
    match watcher.watch(dir, RecursiveMode::NonRecursive) {
        Ok(()) => {
            watched.insert(dir.to_path_buf());
        }
        Err(e) => {
            tracing::debug!("fswatcher: watch {} failed: {e}", dir.display());
        }
    }
}

/// Reconcile the watched per-VM directory set with the live cache: add
/// `<zonepath>` and `<zonepath>/config` for every cached VM, and drop watches
/// for zones no longer present (excluding the always-on global dirs).
fn reconcile_watches<W: Watcher>(
    daemon: &Arc<Daemon>,
    watcher: &mut W,
    watched: &mut HashSet<PathBuf>,
) {
    // The directories the current cache wants watched, plus the globals.
    let mut wanted: HashSet<PathBuf> = HashSet::new();
    wanted.insert(PathBuf::from(ETC_ZONES));
    if let Some(parent) = Path::new(SYSINFO_FILE).parent() {
        wanted.insert(parent.to_path_buf());
    }
    for zonepath in cached_zonepaths(daemon) {
        let zp = PathBuf::from(&zonepath);
        wanted.insert(zp.join("config"));
        wanted.insert(zp);
    }

    // Add new wanted directories.
    let to_add: Vec<PathBuf> = wanted.difference(watched).cloned().collect();
    for dir in to_add {
        watch_dir(watcher, watched, &dir);
    }
    // Remove directories no longer wanted.
    let to_remove: Vec<PathBuf> = watched.difference(&wanted).cloned().collect();
    for dir in to_remove {
        let _ = watcher.unwatch(&dir);
        watched.remove(&dir);
    }
}

/// The `zonepath` of every VM currently in the cache.
fn cached_zonepaths(daemon: &Arc<Daemon>) -> Vec<String> {
    daemon.with_inner(|i| {
        i.cache
            .zonenames()
            .iter()
            .filter_map(|z| {
                i.cache
                    .get(z)
                    .and_then(|c| c.value.get("zonepath"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect()
    })
}

/// Map a changed path to its zonename and refresh that one VM. Paths we don't
/// recognize (or sysinfo, which has no single VM) trigger nothing per-VM here;
/// sysinfo's facts are reconciled by the periodic full refresh.
async fn handle_event(daemon: &Arc<Daemon>, ev: Event) {
    let mut seen: HashSet<String> = HashSet::new();
    for path in &ev.paths {
        if let Some(zonename) = zonename_for_path(path)
            && seen.insert(zonename.clone())
        {
            refresh_one(daemon, &zonename).await;
        }
    }
}

/// Extract the affected VM's zonename (== uuid) from a changed file path.
/// Recognizes the five per-VM watched files; returns `None` otherwise.
fn zonename_for_path(path: &Path) -> Option<String> {
    let s = path.to_str()?;

    // /etc/zones/<uuid>.xml
    if let Some(rest) = s.strip_prefix("/etc/zones/")
        && let Some(uuid) = rest.strip_suffix(".xml")
        && !uuid.contains('/')
    {
        return Some(uuid.to_string());
    }

    // <zonepath>/lastexited  ->  the zonepath's last component is the uuid.
    if let Some(dir) = s.strip_suffix("/lastexited") {
        return Path::new(dir)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string);
    }

    // <zonepath>/config/{metadata,routes,tags}.json
    if s.ends_with("/config/metadata.json")
        || s.ends_with("/config/routes.json")
        || s.ends_with("/config/tags.json")
    {
        // strip the trailing /config/<file>.json to reach <zonepath>.
        let p = Path::new(s);
        return p
            .parent() // .../config
            .and_then(Path::parent) // <zonepath>
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .map(str::to_string);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_zone_xml_path() {
        assert_eq!(
            zonename_for_path(Path::new(
                "/etc/zones/4256eeee-c8b0-4fed-9e0a-975a31e14571.xml"
            ))
            .as_deref(),
            Some("4256eeee-c8b0-4fed-9e0a-975a31e14571")
        );
    }

    #[test]
    fn maps_lastexited_path() {
        assert_eq!(
            zonename_for_path(Path::new("/zones/4256eeee-c8b0-4fed-9e0a-975a31e14571/lastexited"))
                .as_deref(),
            Some("4256eeee-c8b0-4fed-9e0a-975a31e14571")
        );
    }

    #[test]
    fn maps_config_json_paths() {
        for f in ["metadata.json", "routes.json", "tags.json"] {
            let p = format!("/zones/4256eeee-c8b0-4fed-9e0a-975a31e14571/config/{f}");
            assert_eq!(
                zonename_for_path(Path::new(&p)).as_deref(),
                Some("4256eeee-c8b0-4fed-9e0a-975a31e14571"),
                "path {p}"
            );
        }
    }

    #[test]
    fn ignores_unrelated_paths() {
        assert!(zonename_for_path(Path::new("/tmp/.sysinfo.json")).is_none());
        assert!(zonename_for_path(Path::new("/zones/x/config/other.txt")).is_none());
        assert!(zonename_for_path(Path::new("/etc/zones/index")).is_none());
    }
}
