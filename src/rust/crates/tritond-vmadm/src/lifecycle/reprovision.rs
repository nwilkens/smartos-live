// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The reprovision orchestrator: re-image a zone in place while keeping its
//! identity and configuration (uuid, NICs, metadata, resource caps). This
//! reproduces node `VM.js` `reprovisionZone` / `reprovisionBhyve`: halt,
//! replace the boot disk's contents from a new image, boot.
//!
//! WHY two brand paths: for joyent-minimal the zoneroot *is* the image
//! clone, so reprovision uninstalls and re-installs the zoneroot from the
//! new image (`zoneadm install -t`); the zone's `config/` directory under
//! the zonepath (metadata, tags, routes) survives because `uninstall` only
//! removes `root/`. For bhyve the identity lives in the persistent zone
//! config and NICs, so we only swap the contents of `disk0` (destroy +
//! re-clone from the new image snapshot) and leave the zone config intact.

use uuid::Uuid;

use crate::backend::traits::{InstallArgs, ZoneHost};
use crate::error::{Result, VmadmError};

/// Re-image the zone `uuid` from `new_image_uuid`, preserving its identity
/// and configuration. Holds the per-zone config lock for the whole
/// operation (parity with create/delete).
pub async fn reprovision<H: ZoneHost>(
    host: &H,
    uuid: Uuid,
    new_image_uuid: Uuid,
    zpool: &str,
) -> Result<()> {
    let _lock = crate::lock::ZoneLock::acquire(uuid).await?;
    reprovision_in(host, uuid, new_image_uuid, zpool).await
}

/// [`reprovision`] without the lock (tests).
pub async fn reprovision_in<H: ZoneHost>(
    host: &H,
    uuid: Uuid,
    new_image_uuid: Uuid,
    zpool: &str,
) -> Result<()> {
    // The zone must already exist: reprovision keeps an *existing*
    // identity, it does not create one.
    let info = host
        .zone_info(uuid)
        .await?
        .ok_or(VmadmError::NotFound(uuid))?;

    // Always halt first. A running zone cannot have its zoneroot or boot
    // zvol replaced underneath it. `zoneadm_halt` is tolerant of an
    // already-halted zone, so the guard is the cheap state read, not a
    // strict precondition.
    if info.is_running() {
        host.zoneadm_halt(uuid).await?;
    }

    match info.brand.as_str() {
        "joyent-minimal" => reprovision_joyent_minimal(host, uuid, new_image_uuid).await,
        "bhyve" => reprovision_bhyve(host, uuid, new_image_uuid, zpool).await,
        other => Err(VmadmError::UnsupportedBrand(other.to_string())),
    }
}

/// JOYENT-MINIMAL: the zoneroot is the image clone, so re-imaging means
/// uninstalling the zoneroot and re-installing it from the new image.
/// `uninstall -F` removes only `root/`; the `config/` directory under the
/// zonepath (metadata, tags, routes) persists, so the zone keeps its
/// identity.
async fn reprovision_joyent_minimal<H: ZoneHost>(
    host: &H,
    uuid: Uuid,
    new_image_uuid: Uuid,
) -> Result<()> {
    // Preserve the existing zoneroot quota across the re-install. `zoneadm
    // install -q` takes GiB; the persisted `quota` property is bytes, so
    // convert (and fall back to the OS default when absent/unparsable).
    let quota_gib = read_quota_gib(host, &dataset_from_zonepath(host, uuid).await?).await;

    // `uninstall` is tolerant of an already-uninstalled zoneroot, so a
    // retried reprovision after a crash between uninstall and install still
    // proceeds.
    host.zoneadm_uninstall_force(uuid).await?;

    host.zoneadm_install(&InstallArgs {
        zonename: uuid.to_string(),
        quota_gib,
        image_uuid: Some(new_image_uuid),
        no_dataset: true,
    })
    .await?;

    host.zoneadm_boot(uuid).await
}

/// BHYVE: replace `disk0`'s contents in place. The persistent zone config
/// (NICs, CPU/RAM, device list) is untouched; only the bytes behind the
/// boot zvol change. Destroy the old `disk0` then re-clone it from the new
/// image snapshot, matching node `reprovisionBhyve`.
async fn reprovision_bhyve<H: ZoneHost>(
    host: &H,
    uuid: Uuid,
    new_image_uuid: Uuid,
    zpool: &str,
) -> Result<()> {
    // Locate the boot zvol from the live zone config rather than guessing,
    // so a non-conventional layout (boot disk not literally `disk0`) still
    // works. Falls back to the `zones/<uuid>/disk0` convention.
    let export = host.zonecfg_export(uuid).await?;
    let boot_ds = boot_disk_dataset(&export).unwrap_or_else(|| default_boot_dataset(zpool, uuid));

    // Resolve the image snapshot before the destructive step, mirroring
    // node's ordering (verify the source exists, *then* destroy the old
    // disk) so a missing image leaves the VM intact. Prefer the shared
    // `@final` snapshot; fall back to a VM-specific snapshot of the image
    // dataset when `@final` is absent.
    let mut snapshot = format!("{zpool}/{new_image_uuid}@final");
    if !host.zfs_snapshot_exists(&snapshot).await? {
        snapshot = format!("{zpool}/{new_image_uuid}@{uuid}");
        host.zfs_snapshot(&snapshot).await?;
    }

    // Destroy the old boot zvol (tolerant of already-gone), then re-clone
    // from the image. `volsize`/`volblocksize` are inherited from the
    // snapshot and must not be overridden; `refreservation=auto` lets ZFS
    // recompute the reservation for the new contents.
    host.zfs_destroy_recursive(&boot_ds).await?;
    host.zfs_clone(
        &snapshot,
        &boot_ds,
        &[("refreservation".to_string(), "auto".to_string())],
    )
    .await?;

    host.zoneadm_boot(uuid).await
}

/// The conventional boot-zvol dataset for a bhyve VM (`<zpool>/<uuid>/disk0`).
fn default_boot_dataset(zpool: &str, uuid: Uuid) -> String {
    format!("{zpool}/{uuid}/disk0")
}

/// Parse a `zonecfg export` and return the dataset backing the boot disk:
/// the `device` block carrying `(name=boot, value="true")`, with its
/// `match=` raw path stripped of the `/dev/zvol/{r,}dsk/` prefix. Returns
/// `None` when no bootable device is present (the caller then falls back to
/// the `disk0` convention).
fn boot_disk_dataset(export: &str) -> Option<String> {
    let mut in_device = false;
    let mut match_path: Option<String> = None;
    let mut is_boot = false;

    for raw in export.lines() {
        let line = raw.trim();
        if line == "add device" {
            in_device = true;
            match_path = None;
            is_boot = false;
            continue;
        }
        if !in_device {
            continue;
        }
        if line == "end" {
            if is_boot && let Some(path) = match_path.take() {
                return Some(dataset_from_raw_path(&path));
            }
            in_device = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("set match=") {
            match_path = Some(rest.trim().to_string());
        } else if device_property_is_boot_true(line) {
            is_boot = true;
        }
    }
    None
}

/// Whether a zonecfg device-property line is `(name=boot, value="true")`.
/// Matches the form emitted by `build.rs::emit_disks` regardless of
/// incidental whitespace.
fn device_property_is_boot_true(line: &str) -> bool {
    let Some(inner) = line
        .strip_prefix("add property (")
        .and_then(|s| s.strip_suffix(')'))
    else {
        return false;
    };
    let mut name = None;
    let mut value = None;
    for field in inner.split(',') {
        let field = field.trim();
        if let Some(v) = field.strip_prefix("name=") {
            name = Some(v.trim());
        } else if let Some(v) = field.strip_prefix("value=") {
            value = Some(v.trim().trim_matches('"'));
        }
    }
    name == Some("boot") && value == Some("true")
}

/// Strip the `/dev/zvol/rdsk/` or `/dev/zvol/dsk/` prefix from a device
/// `match=` path to recover the ZFS dataset name.
fn dataset_from_raw_path(path: &str) -> String {
    path.strip_prefix("/dev/zvol/rdsk/")
        .or_else(|| path.strip_prefix("/dev/zvol/dsk/"))
        .unwrap_or(path)
        .to_string()
}

/// The zoneroot dataset for a uuid, derived from its live `zonepath` (the
/// zonepath without its leading slash, matching `delete`). Used to read the
/// existing quota before a joyent-minimal re-install.
async fn dataset_from_zonepath<H: ZoneHost>(host: &H, uuid: Uuid) -> Result<String> {
    let info = host
        .zone_info(uuid)
        .await?
        .ok_or(VmadmError::NotFound(uuid))?;
    Ok(info.zonepath.trim_start_matches('/').to_string())
}

/// Read the zoneroot `quota` (bytes) and convert to whole GiB for `zoneadm
/// install -q`, defaulting to the OS default when the property is absent,
/// zero, or unparsable. WHY default-on-error: a re-install must not fail
/// just because the quota readback hiccupped; an OS zoneroot always wants a
/// sane quota.
async fn read_quota_gib<H: ZoneHost>(host: &H, dataset: &str) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    let props = match host.zfs_get_props(dataset, &["quota"]).await {
        Ok(p) => p,
        Err(_) => return crate::constants::DEFAULT_QUOTA_GIB,
    };
    props
        .get("quota")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|bytes| bytes / GIB)
        .filter(|gib| *gib > 0)
        .unwrap_or(crate::constants::DEFAULT_QUOTA_GIB)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeHost;
    use crate::backend::traits::ZoneRuntime;
    use std::collections::BTreeMap;

    fn uuid() -> Uuid {
        "11111111-1111-1111-1111-111111111111"
            .parse()
            .expect("uuid")
    }

    fn new_image() -> Uuid {
        "99999999-9999-9999-9999-999999999999"
            .parse()
            .expect("uuid")
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

    /// A bhyve `zonecfg export` device block for a bootable disk0.
    fn bhyve_export() -> String {
        format!(
            "create -b\nset zonepath=/zones/{u}\nadd device\n\
             set match=/dev/zvol/rdsk/zones/{u}/disk0\n\
             add property (name=boot, value=\"true\")\n\
             add property (name=model, value=\"virtio\")\nend\n",
            u = uuid()
        )
    }

    fn index_of(ops: &[String], needle: &str) -> Option<usize> {
        ops.iter().position(|o| o.contains(needle))
    }

    #[tokio::test]
    async fn reprovision_missing_is_not_found() {
        let host = FakeHost::new();
        assert!(matches!(
            reprovision_in(&host, uuid(), new_image(), "zones").await,
            Err(VmadmError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn joyent_minimal_halt_uninstall_install_boot_order() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("joyent-minimal"));
        // 25 GiB quota in bytes, so the re-install preserves it.
        let mut props = BTreeMap::new();
        props.insert(
            "quota".to_string(),
            (25u64 * 1024 * 1024 * 1024).to_string(),
        );
        host.set_zfs_props(format!("zones/{}", uuid()), props);

        reprovision_in(&host, uuid(), new_image(), "zones")
            .await
            .expect("reprovision");

        let ops = host.ops();
        let i_halt = index_of(&ops, "halt").expect("halt");
        let i_uninstall = index_of(&ops, "uninstall -F").expect("uninstall");
        let i_install = index_of(&ops, "install -q").expect("install");
        let i_boot = index_of(&ops, "boot").expect("boot");
        assert!(i_halt < i_uninstall, "halt before uninstall: {ops:?}");
        assert!(i_uninstall < i_install, "uninstall before install: {ops:?}");
        assert!(i_install < i_boot, "install before boot: {ops:?}");
        // The re-install targets the NEW image, preserves the read-back
        // quota, and keeps the zoneroot clone (`-x nodataset`).
        let install = ops
            .iter()
            .find(|o| o.contains("install -q"))
            .expect("install op");
        assert!(
            install.contains("install -q 25"),
            "preserved quota: {install}"
        );
        assert!(
            install.contains(&format!("-t {}", new_image())),
            "new image: {install}"
        );
        assert!(install.contains("-x nodataset"), "{install}");
    }

    #[tokio::test]
    async fn joyent_minimal_missing_quota_uses_default() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("joyent-minimal"));
        // No zfs props set -> default quota.
        reprovision_in(&host, uuid(), new_image(), "zones")
            .await
            .expect("reprovision");
        let ops = host.ops();
        let install = ops
            .iter()
            .find(|o| o.contains("install -q"))
            .expect("install");
        assert!(
            install.contains(&format!(
                "install -q {}",
                crate::constants::DEFAULT_QUOTA_GIB
            )),
            "{install}"
        );
    }

    #[tokio::test]
    async fn bhyve_halt_destroy_clone_boot_order() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("bhyve"));
        host.set_export(uuid(), bhyve_export());

        reprovision_in(&host, uuid(), new_image(), "zones")
            .await
            .expect("reprovision");

        let ops = host.ops();
        let disk0 = format!("zones/{}/disk0", uuid());
        let i_halt = index_of(&ops, "halt").expect("halt");
        let i_destroy = index_of(&ops, &format!("zfs destroy -rf {disk0}")).expect("destroy disk0");
        let i_clone = index_of(&ops, "zfs clone -F").expect("clone");
        let i_boot = index_of(&ops, "boot").expect("boot");
        assert!(i_halt < i_destroy, "halt before destroy: {ops:?}");
        assert!(i_destroy < i_clone, "destroy before clone: {ops:?}");
        assert!(i_clone < i_boot, "clone before boot: {ops:?}");
        // The clone re-images disk0 from the new image's @final snapshot.
        let clone = ops
            .iter()
            .find(|o| o.contains("zfs clone -F"))
            .expect("clone op");
        assert!(
            clone.contains(&format!("zones/{}@final", new_image())),
            "from @final of new image: {clone}"
        );
        assert!(clone.contains(&disk0), "into disk0: {clone}");
        assert!(clone.contains("refreservation=auto"), "{clone}");
        // The JM uninstall path must NOT run for bhyve.
        assert!(
            index_of(&ops, "uninstall -F").is_none(),
            "bhyve keeps the zone config"
        );
    }

    #[tokio::test]
    async fn bhyve_falls_back_to_disk0_convention_without_export() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("bhyve"));
        // No export set -> empty export -> convention fallback.
        reprovision_in(&host, uuid(), new_image(), "zones")
            .await
            .expect("reprovision");
        let disk0 = format!("zones/{}/disk0", uuid());
        assert!(
            host.ops()
                .iter()
                .any(|o| o.contains(&format!("zfs destroy -rf {disk0}"))),
            "{:?}",
            host.ops()
        );
    }

    #[test]
    fn boot_disk_parse_picks_boot_true_device() {
        let ds = boot_disk_dataset(&bhyve_export()).expect("boot disk");
        assert_eq!(ds, format!("zones/{}/disk0", uuid()));
    }

    #[test]
    fn boot_disk_parse_skips_non_boot_devices() {
        // A data disk (boot=false) followed by the boot disk: only the
        // bootable one is returned.
        let export = format!(
            "add device\nset match=/dev/zvol/rdsk/zones/{u}/disk1\n\
             add property (name=boot, value=\"false\")\nend\n\
             add device\nset match=/dev/zvol/rdsk/zones/{u}/disk0\n\
             add property (name=boot, value=\"true\")\nend\n",
            u = uuid()
        );
        assert_eq!(
            boot_disk_dataset(&export),
            Some(format!("zones/{}/disk0", uuid()))
        );
    }

    #[test]
    fn boot_disk_parse_none_when_no_boot_device() {
        let export = "create -b\nset zonepath=/zones/x\n".to_string();
        assert_eq!(boot_disk_dataset(&export), None);
    }
}
