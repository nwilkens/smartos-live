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
/// (the control plane has already cleared its record). Halts a running
/// zone first, then `zoneadm uninstall -F` -> `zonecfg delete -F` -> a
/// belt-and-suspenders `zfs destroy -rf` of any residual datasets. Holds
/// the per-zone config lock.
pub async fn delete<H: ZoneHost>(host: &H, uuid: Uuid) -> Result<()> {
    let _lock = crate::lock::ZoneLock::acquire(uuid).await?;
    delete_in(host, uuid).await
}

/// [`delete`] without the lock (tests).
pub async fn delete_in<H: ZoneHost>(host: &H, uuid: Uuid) -> Result<()> {
    let Some(info) = host.zone_info(uuid).await? else {
        return Ok(()); // already gone
    };
    if info.is_running() {
        host.zoneadm_halt(uuid).await?;
    }
    host.zoneadm_uninstall_force(uuid).await?;
    host.zonecfg_delete_force(uuid).await?;
    // Residual datasets (bhyve zvols under zones/<uuid>) — tolerant of
    // already-gone. The dataset is the zonepath without its leading slash.
    let dataset = info.zonepath.trim_start_matches('/');
    if !dataset.is_empty() {
        host.zfs_destroy_recursive(dataset).await?;
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
        let order: Vec<usize> = ["halt", "uninstall -F", "delete -F", "zfs destroy -rf"]
            .iter()
            .map(|m| ops.iter().position(|o| o.contains(m)).expect(m))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{ops:?}");
    }

    #[tokio::test]
    async fn delete_missing_is_idempotent_ok() {
        let host = FakeHost::new();
        delete_in(&host, uuid()).await.unwrap();
        assert!(!host.ops().iter().any(|o| o.contains("uninstall")));
    }
}
