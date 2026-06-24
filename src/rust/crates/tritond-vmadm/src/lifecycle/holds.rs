// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Indestructibility gate shared by the destructive verbs (delete,
//! reprovision).
//!
//! node `VM.js` marks a zone indestructible by holding the
//! `<dataset>@indestructible` snapshot (`makeIndestructible`, VM.js:8020).
//! `vmload` derives `indestructible_zoneroot`/`indestructible_delegated`
//! from a held `@indestructible` snapshot (`userrefs > 0`) on the zoneroot
//! and the delegated `/data` dataset respectively (vmload/index.js:140-148,
//! 359-367), and `deleteZone` (VM.js:12847) / `reprovision` (VM.js:11550)
//! refuse when either flag is set.
//!
//! We do not trust persisted attrs for this: the authoritative signal is
//! the *live* hold state, read with `zfs holds`, so a hold added or
//! released out of band is honoured immediately.

use uuid::Uuid;

use crate::backend::traits::ZoneHost;
use crate::error::{Result, VmadmError};

/// Return [`VmadmError::Indestructible`] if any `@indestructible` hold
/// exists on the zoneroot or the delegated `/data` dataset, and for bhyve
/// if any snapshot of `disk0` is held.
///
/// `zfs_filesystem` is the zoneroot dataset (the zonepath without its
/// leading slash, matching `delete`). Mirrors node's pre-flight checks in
/// `deleteZone` (VM.js:12847) and `reprovision` `checkBhyveHolds`
/// (VM.js:11586/11593) — the gate runs BEFORE any halt/uninstall/destroy.
pub async fn ensure_destructible<H: ZoneHost>(
    host: &H,
    uuid: Uuid,
    zfs_filesystem: &str,
    is_bhyve: bool,
) -> Result<()> {
    // Zoneroot: <zfs_filesystem>@indestructible (vmload indestructible_zoneroot).
    if snapshot_is_held(host, &format!("{zfs_filesystem}@indestructible")).await? {
        return Err(VmadmError::Indestructible(uuid));
    }

    // Delegated data: <zfs_filesystem>/data@indestructible
    // (vmload indestructible_delegated).
    if snapshot_is_held(host, &format!("{zfs_filesystem}/data@indestructible")).await? {
        return Err(VmadmError::Indestructible(uuid));
    }

    // bhyve: the boot disk's contents are replaced/destroyed, so a held
    // snapshot of disk0 must block too (node `checkBhyveHolds`,
    // VM.js:11586). node enumerates *all* snapshots of disk0 and fails on
    // any hold; we read the conventional `@indestructible` plus walk the
    // disk0 snapshot list when the host can enumerate it.
    if is_bhyve {
        let disk0 = format!("{zfs_filesystem}/disk0");
        if snapshot_is_held(host, &format!("{disk0}@indestructible")).await? {
            return Err(VmadmError::Indestructible(uuid));
        }
        for snap in host.zfs_list_snapshots(&disk0).await? {
            if snapshot_is_held(host, &snap.name).await? {
                return Err(VmadmError::Indestructible(uuid));
            }
        }
    }

    Ok(())
}

/// Whether `snapshot` has at least one `zfs hold`. A missing snapshot
/// yields an empty list (the host swallows "does not exist"), so this is a
/// no-op on a never-held dataset (node `_listIndestructibleHolds`,
/// VM.js:8093).
async fn snapshot_is_held<H: ZoneHost>(host: &H, snapshot: &str) -> Result<bool> {
    Ok(!host.zfs_holds(snapshot).await?.is_empty())
}
