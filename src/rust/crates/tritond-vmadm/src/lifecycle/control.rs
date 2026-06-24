// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The power/teardown lifecycle verbs: start, stop, reboot, delete. Each
//! is idempotent against the current zone state and addresses the zone by
//! uuid (== the instance id).

use std::time::Duration;

use uuid::Uuid;

use crate::backend::traits::ZoneHost;
use crate::error::{Result, VmadmError};

/// Boot a stopped zone (`JobKind::Start`). A running zone is a no-op
/// (idempotent); a missing zone is [`VmadmError::NotFound`].
pub async fn start<H: ZoneHost>(host: &H, uuid: Uuid) -> Result<()> {
    let info = host
        .zone_info(uuid)
        .await?
        .ok_or(VmadmError::NotFound(uuid))?;
    if info.is_running() {
        return Ok(());
    }
    host.zoneadm_boot(uuid).await
}

/// Stop a running zone. `force` halts immediately; otherwise an OS zone is
/// shut down gracefully in-guest (`zlogin shutdown`), falling back to a
/// halt on timeout. An already-stopped zone is a no-op.
pub async fn stop<H: ZoneHost>(host: &H, uuid: Uuid, force: bool, timeout: Duration) -> Result<()> {
    let info = host
        .zone_info(uuid)
        .await?
        .ok_or(VmadmError::NotFound(uuid))?;
    if !info.is_running() {
        return Ok(());
    }
    if force {
        return host.zoneadm_halt(uuid).await;
    }
    // Graceful: OS zones shut down from inside; bhyve (and a timed-out OS
    // shutdown) fall back to a halt.
    if info.brand == "bhyve" {
        return host.zoneadm_halt(uuid).await;
    }
    match host.zlogin_shutdown(uuid, timeout).await {
        Ok(()) => Ok(()),
        Err(_) => host.zoneadm_halt(uuid).await,
    }
}

/// Reboot a zone. A stopped zone is simply booted (matching node).
pub async fn reboot<H: ZoneHost>(host: &H, uuid: Uuid, _force: bool) -> Result<()> {
    let info = host
        .zone_info(uuid)
        .await?
        .ok_or(VmadmError::NotFound(uuid))?;
    if !info.is_running() {
        return host.zoneadm_boot(uuid).await;
    }
    host.zoneadm_reboot(uuid).await
}

/// Delete a zone and its storage. Idempotent: a missing zone is success
/// (the control plane has already cleared its record). Refuses an
/// indestructible zone (a held `@indestructible` snapshot on the zoneroot,
/// delegated data, or a bhyve boot disk; node `deleteZone`, VM.js:12847)
/// BEFORE touching anything. Otherwise halts a running zone, then `zoneadm
/// uninstall -F` -> `zonecfg delete -F` -> a belt-and-suspenders `zfs
/// destroy -rF` of any residual datasets. Holds the per-zone config lock.
pub async fn delete<H: ZoneHost>(host: &H, uuid: Uuid) -> Result<()> {
    let _lock = crate::lock::ZoneLock::acquire(uuid).await?;
    delete_in(host, uuid).await
}

/// [`delete`] without the lock (tests).
pub async fn delete_in<H: ZoneHost>(host: &H, uuid: Uuid) -> Result<()> {
    let Some(info) = host.zone_info(uuid).await? else {
        return Ok(()); // already gone
    };
    // The zoneroot dataset is the zonepath without its leading slash.
    let zfs_filesystem = info.zonepath.trim_start_matches('/').to_string();
    // Gate on live ZFS holds BEFORE any destructive step (node refuses a
    // delete when indestructible_zoneroot/indestructible_delegated is set,
    // VM.js:12847-12850). Trust the live hold state, not persisted attrs.
    crate::lifecycle::holds::ensure_destructible(
        host,
        uuid,
        &zfs_filesystem,
        info.brand == "bhyve",
    )
    .await?;
    if info.is_running() {
        host.zoneadm_halt(uuid).await?;
    }
    host.zoneadm_uninstall_force(uuid).await?;
    host.zonecfg_delete_force(uuid).await?;
    // Residual datasets — tolerant of already-gone. The zoneroot recursive
    // destroy covers bhyve disks (children under zones/<uuid>) and the
    // zoneroot itself. kvm disks are FLAT SIBLINGS (zones/<uuid>-diskN,
    // VM.js:3174), outside the zoneroot subtree, so they must be enumerated
    // and destroyed separately or they leak (node deleteZone destroys
    // per-VM volumes, VM.js:13039).
    if !zfs_filesystem.is_empty() {
        destroy_sibling_disks(host, &zfs_filesystem).await?;
        host.zfs_destroy_recursive(&zfs_filesystem).await?;
    }
    Ok(())
}

/// Destroy the flat-sibling disk zvols (`<zoneroot>-disk*`) and any of their
/// snapshots. These live OUTSIDE the zoneroot subtree (kvm layout), so the
/// zoneroot recursive destroy never reaches them. Each destroy is tolerant
/// of an already-gone dataset; a brand with no sibling disks (bhyve, OS
/// zones) simply finds none.
async fn destroy_sibling_disks<H: ZoneHost>(host: &H, zfs_filesystem: &str) -> Result<()> {
    let disk_prefix = format!("{zfs_filesystem}-disk");
    for dataset in host.zfs_list_sibling_disks(&disk_prefix).await? {
        host.zfs_destroy_recursive(&dataset).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeHost;
    use crate::backend::traits::ZoneRuntime;

    fn uuid() -> Uuid {
        "11111111-1111-1111-1111-111111111111".parse().unwrap()
    }

    fn running(brand: &str) -> ZoneRuntime {
        ZoneRuntime {
            zoneid: Some(7),
            state: "running".to_string(),
            brand: brand.to_string(),
            zonepath: format!("/zones/{}", uuid()),
            zonedid: Some(7),
        }
    }

    fn stopped(brand: &str) -> ZoneRuntime {
        ZoneRuntime {
            state: "installed".to_string(),
            ..running(brand)
        }
    }

    #[tokio::test]
    async fn start_missing_is_not_found() {
        let host = FakeHost::new();
        assert!(matches!(
            start(&host, uuid()).await,
            Err(VmadmError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn start_stopped_boots_running_noop() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), stopped("joyent-minimal"));
        start(&host, uuid()).await.unwrap();
        assert!(host.ops().iter().any(|o| o.contains("boot")));

        let host2 = FakeHost::new();
        host2.set_zone_info(uuid(), running("joyent-minimal"));
        start(&host2, uuid()).await.unwrap();
        assert!(
            !host2.ops().iter().any(|o| o.contains("boot")),
            "running start is a no-op"
        );
    }

    #[tokio::test]
    async fn stop_graceful_os_then_force_bhyve() {
        let os = FakeHost::new();
        os.set_zone_info(uuid(), running("joyent-minimal"));
        stop(&os, uuid(), false, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(os.ops().iter().any(|o| o.contains("shutdown")));

        let bh = FakeHost::new();
        bh.set_zone_info(uuid(), running("bhyve"));
        stop(&bh, uuid(), false, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(bh.ops().iter().any(|o| o.contains("halt")));
        assert!(!bh.ops().iter().any(|o| o.contains("shutdown")));
    }

    #[tokio::test]
    async fn stop_already_stopped_is_noop() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), stopped("joyent-minimal"));
        stop(&host, uuid(), true, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(!host.ops().iter().any(|o| o.contains("halt")));
    }

    #[tokio::test]
    async fn delete_halts_uninstalls_deletes_destroys() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("bhyve"));
        delete_in(&host, uuid()).await.unwrap();
        let ops = host.ops();
        let order: Vec<usize> = ["halt", "uninstall -F", "delete -F", "zfs destroy -rF"]
            .iter()
            .map(|m| ops.iter().position(|o| o.contains(m)).expect(m))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{ops:?}");
    }

    /// A kvm delete must destroy the flat-sibling disk zvols (and their
    /// snapshots), not just the zoneroot — otherwise the zvols leak.
    #[tokio::test]
    async fn delete_kvm_destroys_sibling_zvols() {
        let host = FakeHost::new();
        let mut info = running("kvm");
        // The zoneroot is /zones/<uuid> -> zfs zones/<uuid>.
        info.zonepath = format!("/zones/{}", uuid());
        host.set_zone_info(uuid(), info);
        let disk0 = format!("zones/{}-disk0", uuid());
        let disk1 = format!("zones/{}-disk1", uuid());
        let snap = format!("zones/{}-disk0@vmsnap-x", uuid());
        // A neighbouring zone's sibling disk must NOT be touched.
        let other = "zones/99999999-9999-9999-9999-999999999999-disk0".to_string();
        host.set_dataset_exists(&disk0);
        host.set_dataset_exists(&disk1);
        host.set_dataset_exists(&snap);
        host.set_dataset_exists(&other);

        delete_in(&host, uuid()).await.unwrap();

        let ops = host.ops();
        // The siblings (and the snapshot) are each destroyed, plus the
        // zoneroot itself.
        assert!(
            ops.iter().any(|o| o == &format!("zfs destroy -rF {disk0}")),
            "disk0 destroyed: {ops:?}"
        );
        assert!(
            ops.iter().any(|o| o == &format!("zfs destroy -rF {disk1}")),
            "disk1 destroyed: {ops:?}"
        );
        assert!(
            ops.iter().any(|o| o == &format!("zfs destroy -rF {snap}")),
            "disk0 snapshot destroyed: {ops:?}"
        );
        assert!(
            ops.iter()
                .any(|o| o == &format!("zfs destroy -rF zones/{}", uuid())),
            "zoneroot destroyed: {ops:?}"
        );
        // The neighbouring zone's disk is left alone.
        assert!(
            !ops.iter().any(|o| o.contains(&other)),
            "must not touch another zone's disk: {ops:?}"
        );
        // And the fake's existence set confirms the leak is gone.
        assert!(!host.dataset_exists(&disk0), "disk0 still exists");
        assert!(!host.dataset_exists(&disk1), "disk1 still exists");
        assert!(!host.dataset_exists(&snap), "snapshot still exists");
        assert!(host.dataset_exists(&other), "neighbour disk wrongly destroyed");
    }

    /// bhyve disks are children of the zoneroot; the sibling enumeration
    /// finds nothing and the zoneroot recursive destroy covers them.
    #[tokio::test]
    async fn delete_bhyve_has_no_sibling_zvols_to_destroy() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("bhyve"));
        delete_in(&host, uuid()).await.unwrap();
        let ops = host.ops();
        // Only the zoneroot recursive destroy (children come with it); no
        // extra sibling destroys beyond the zoneroot.
        let destroys: Vec<_> = ops.iter().filter(|o| o.starts_with("zfs destroy")).collect();
        assert_eq!(destroys.len(), 1, "exactly the zoneroot destroy: {ops:?}");
        assert!(
            destroys[0] == &format!("zfs destroy -rF zones/{}", uuid()),
            "{destroys:?}"
        );
    }

    #[tokio::test]
    async fn delete_missing_is_idempotent_ok() {
        let host = FakeHost::new();
        delete_in(&host, uuid()).await.unwrap();
        assert!(!host.ops().iter().any(|o| o.contains("uninstall")));
    }

    /// A held zoneroot `@indestructible` snapshot blocks delete entirely —
    /// no halt/uninstall/destroy may run (node VM.js:12847).
    #[tokio::test]
    async fn delete_indestructible_zoneroot_refuses_and_runs_nothing() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("joyent-minimal"));
        host.set_holds(format!("zones/{}@indestructible", uuid()), &["do_not_destroy"]);

        assert!(matches!(
            delete_in(&host, uuid()).await,
            Err(VmadmError::Indestructible(_))
        ));
        let ops = host.ops();
        assert!(!ops.iter().any(|o| o.contains("halt")), "{ops:?}");
        assert!(!ops.iter().any(|o| o.contains("uninstall")), "{ops:?}");
        assert!(!ops.iter().any(|o| o.contains("delete -F")), "{ops:?}");
        assert!(!ops.iter().any(|o| o.contains("zfs destroy")), "{ops:?}");
    }

    /// A held delegated `/data@indestructible` snapshot also blocks delete.
    #[tokio::test]
    async fn delete_indestructible_delegated_refuses() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("joyent-minimal"));
        host.set_holds(
            format!("zones/{}/data@indestructible", uuid()),
            &["do_not_destroy"],
        );
        assert!(matches!(
            delete_in(&host, uuid()).await,
            Err(VmadmError::Indestructible(_))
        ));
        assert!(!host.ops().iter().any(|o| o.contains("uninstall")));
    }

    /// For bhyve, a held snapshot of the boot disk blocks delete.
    #[tokio::test]
    async fn delete_indestructible_bhyve_disk0_refuses() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("bhyve"));
        host.set_holds(
            format!("zones/{}/disk0@indestructible", uuid()),
            &["do_not_destroy"],
        );
        assert!(matches!(
            delete_in(&host, uuid()).await,
            Err(VmadmError::Indestructible(_))
        ));
        let ops = host.ops();
        assert!(!ops.iter().any(|o| o.contains("halt")), "{ops:?}");
        assert!(!ops.iter().any(|o| o.contains("uninstall")), "{ops:?}");
    }
}
