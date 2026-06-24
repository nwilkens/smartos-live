// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The reprovision orchestrator: re-image a zone in place while keeping its
//! identity and configuration (uuid, NICs, metadata, resource caps). This
//! reproduces node `VM.js` `reprovision` / `reprovisionBhyve`: stop,
//! replace the boot disk's contents from a new image, update the image
//! identity, boot.
//!
//! WHY two brand paths: for joyent-minimal the zoneroot *is* the image
//! clone, so reprovision uninstalls and re-installs the zoneroot from the
//! new image (`zoneadm install -t`). A delegated `/data` dataset lives
//! UNDER the zoneroot (`<zfs_filesystem>/data`), so it would be destroyed
//! by `uninstall -F` — node renames it out to a sibling first and back
//! after (VM.js:11761/12119); we mirror that data-preservation dance. For
//! bhyve the identity lives in the persistent zone config and NICs, so we
//! only swap the contents of `disk0` (destroy + re-clone from the new image
//! snapshot) and update the device's `image-uuid` property.

use uuid::Uuid;

use crate::backend::traits::{InstallArgs, ZoneHost, ZoneRef};
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

    let zfs_filesystem = info.zonepath.trim_start_matches('/').to_string();
    let is_bhyve = info.brand == "bhyve";

    // Gate on live ZFS holds BEFORE any state transition (node refuses a
    // reprovision when indestructible_zoneroot is set and runs
    // `checkBhyveHolds` before stopping, VM.js:11550/11586). Trust the live
    // hold state, not persisted attrs.
    crate::lifecycle::holds::ensure_destructible(host, uuid, &zfs_filesystem, is_bhyve).await?;

    // Graceful stop before re-imaging: a running zone cannot have its
    // zoneroot or boot zvol replaced underneath it. node `VM.js` calls
    // `VM.stop` (a graceful in-guest shutdown that falls back to halt)
    // before reprovision (VM.js:11630); we reuse the same lifecycle verb so
    // an OS guest gets a clean shutdown rather than a hard power-off.
    if info.is_running() {
        let timeout = std::time::Duration::from_secs(crate::constants::STOP_TIMEOUT_SECS);
        crate::lifecycle::control::stop(host, uuid, false, timeout).await?;
        // `VM.stop` returns before the zone has fully transitioned out of
        // `running`; node then `waitForZoneState(installed)` and halts on
        // timeout (VM.js:11638-11675). Without this wait the subsequent
        // `zoneadm uninstall` races a still-running zone and fails with
        // "uninstall operation is invalid for running zones". Applies to
        // both JM and bhyve (bhyve stop is a halt but is still async).
        wait_until_halted(host, uuid, timeout).await?;
    }

    match info.brand.as_str() {
        // lx reprovisions through the OS-zone path identically to
        // joyent-minimal (uninstall the zoneroot, reinstall the new image
        // via `zoneadm install -t`); the kernel-version attr is preserved
        // in zonecfg across the uninstall/install.
        "joyent-minimal" | "lx" => {
            reprovision_os_zone(host, uuid, new_image_uuid, &zfs_filesystem).await
        }
        "bhyve" => reprovision_bhyve(host, uuid, new_image_uuid, &zfs_filesystem, zpool).await,
        other => Err(VmadmError::UnsupportedBrand(other.to_string())),
    }
}

/// Poll `zone_info` until the zone is no longer running (it has reached the
/// `installed`/halted state), bounded by `timeout`. node's
/// `waitForZoneState(installed)` after the stop (VM.js:11638); on timeout it
/// force-halts and re-checks. We mirror that: a final `zoneadm halt` then a
/// short re-poll so a wedged shutdown still converges before uninstall.
async fn wait_until_halted<H: ZoneHost>(
    host: &H,
    uuid: Uuid,
    timeout: std::time::Duration,
) -> Result<()> {
    if poll_halted(host, uuid, timeout).await? {
        return Ok(());
    }
    // Shutdown did not complete in time — force a halt and give it a short
    // window to settle (node falls back to `zoneadm halt` on ETIMEOUT).
    host.zoneadm_halt(uuid).await?;
    if poll_halted(host, uuid, std::time::Duration::from_secs(10)).await? {
        return Ok(());
    }
    Err(VmadmError::Payload(format!(
        "zone {uuid} did not reach the halted state before reprovision uninstall"
    )))
}

/// Poll `zone_info` until the zone reaches a terminal halted state, up to
/// `timeout`. Returns `true` once halted, `false` if the deadline passes
/// first. A zone that has disappeared (config gone) counts as halted.
///
/// node's `waitForZoneState(installed)` waits for EXACTLY `installed`. A
/// stopping zone passes through transient states (`shutting_down`, `down`,
/// `ready`, `mounted`) that are neither `running` nor halted — treating
/// merely-not-running as halted races `zoneadm uninstall` (which then fails
/// "uninstall operation is invalid for shutting_down zones"). So we wait for
/// a genuinely terminal state.
async fn poll_halted<H: ZoneHost>(
    host: &H,
    uuid: Uuid,
    timeout: std::time::Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match host.zone_info(uuid).await? {
            None => return Ok(true),
            Some(info) if is_halted_state(&info.state) => return Ok(true),
            Some(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Whether a `zoneadm` state is a terminal halted state safe to uninstall
/// from. `installed`/`configured`/`incomplete` are not running and not
/// transitioning; everything else (`running`, `shutting_down`, `down`,
/// `ready`, `mounted`) means the zone is still up or still stopping.
fn is_halted_state(state: &str) -> bool {
    matches!(state, "installed" | "configured" | "incomplete")
}

/// OS ZONES (joyent-minimal, lx): the zoneroot is the image clone, so
/// re-imaging means uninstalling the zoneroot and re-installing it from the
/// new image. A delegated `/data` dataset is a child of the zoneroot and
/// would be destroyed by `uninstall -F`, so it is renamed OUT of the subtree
/// (to a sibling) first and renamed back after the re-install — node's
/// data-preservation dance (VM.js:11713/11761/12119/12142).
async fn reprovision_os_zone<H: ZoneHost>(
    host: &H,
    uuid: Uuid,
    new_image_uuid: Uuid,
    zfs_filesystem: &str,
) -> Result<()> {
    // Preserve the existing zoneroot quota across the re-install. `zoneadm
    // install -q` takes GiB; the persisted `quota` property is bytes, so
    // convert (and fall back to the OS default when absent/unparsable).
    let quota_gib = read_quota_gib(host, zfs_filesystem).await;

    let delegated = format!("{zfs_filesystem}/data");
    let parked = format!("{zfs_filesystem}-reprovisioning-data");

    // Resolve the data-preservation state up front so a retried reprovision
    // after a crash mid-dance recovers rather than re-doing destructive
    // work. Two facts: is the delegated dataset live, and is the parked
    // sibling present from a prior crashed run?
    let data_live = host.zfs_dataset_exists(&delegated).await?;
    let data_parked = host.zfs_dataset_exists(&parked).await?;

    if (data_live || data_parked) && !delegated_is_standard(host, zfs_filesystem).await? {
        // node refuses reprovision with >1 or a non-standard delegated
        // dataset rather than risk losing it (VM.js:11556-11567). We do the
        // same: never silently destroy data we cannot safely move.
        return Err(VmadmError::Payload(format!(
            "VM {uuid} has a non-standard or multiple delegated datasets; \
             refusing reprovision to avoid data loss"
        )));
    }

    // Whether this zone has a delegated `/data` to preserve (live now, or
    // parked from a prior crashed run). Drives the dataset-resource dance.
    let has_delegated = data_live || data_parked;

    // Park the delegated dataset out of the zoneroot subtree BEFORE the
    // uninstall destroys the subtree. Skip if a prior crashed run already
    // parked it (idempotent recovery).
    if data_live && !data_parked {
        // zoned=off so the rename can touch the dataset from the GZ
        // (VM.js:11713).
        host.zfs_set_prop(&delegated, "zoned", "off").await?;
        host.zfs_rename(&delegated, &parked).await?; // VM.js:11761
    }

    // Remove the `dataset` resource from zonecfg BEFORE uninstall/install.
    // The parked dataset no longer exists at `zones/<uuid>/data`, so a
    // `zoneadm install` verify fails ("could not verify zfs dataset ... does
    // not exist") while the config still names it. node's rename dance keeps
    // the install consistent the same way (VM.js:11556-12151). Tolerant of an
    // already-absent resource, so a crash mid-flight strands nothing.
    if has_delegated {
        host.zonecfg_remove_dataset(uuid, &delegated).await?;
    }

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

    // Restore the delegated dataset to its original name. Idempotent: only
    // rename if the parked sibling is present and the target is not, so a
    // crash after this step (or a re-run) does not double-rename or fail
    // (VM.js:12119).
    let parked_now = host.zfs_dataset_exists(&parked).await?;
    let data_now = host.zfs_dataset_exists(&delegated).await?;
    if parked_now && !data_now {
        host.zfs_rename(&parked, &delegated).await?;
        host.zfs_set_prop(&delegated, "zoned", "on").await?; // VM.js:12142
    }

    // Re-add the `dataset` resource only AFTER `/data` is back at its
    // original name (and zoned=on), so the config once again matches the
    // live datasets. Idempotent: tolerant of an already-present resource on a
    // re-run (VM.js re-establishes the delegated dataset on the new root).
    if has_delegated {
        host.zonecfg_add_dataset(uuid, &delegated).await?;
    }

    // Update the zone's image identity to the new image: `dataset-uuid` is
    // the OS-zone `image_uuid` attr (node sets it before installZone,
    // VM.js:12158-12160).
    host.zonecfg_set_attr(uuid, "dataset-uuid", &new_image_uuid.to_string())
        .await?;

    host.zoneadm_boot(uuid).await
}

/// Whether the zone's only delegated dataset is the standard
/// `<zfs_filesystem>/data`. node treats a missing dataset, or exactly the
/// `/data` child, as standard; anything else (multiple datasets, or a
/// differently-named one) is refused (VM.js:11556-11567).
///
/// With the available primitives we can only probe the conventional `/data`
/// name. We additionally reject the presence of a `/data2` sibling as a
/// proxy for "more than one delegated dataset" so a multi-dataset guest is
/// not silently truncated to its first dataset.
async fn delegated_is_standard<H: ZoneHost>(host: &H, zfs_filesystem: &str) -> Result<bool> {
    // A second conventional delegated dataset would not survive our
    // single-dataset dance; treat its presence as non-standard.
    if host
        .zfs_dataset_exists(&format!("{zfs_filesystem}/data2"))
        .await?
    {
        return Ok(false);
    }
    Ok(true)
}

/// BHYVE: replace `disk0`'s contents in place. The persistent zone config
/// (NICs, CPU/RAM, device list) is untouched; only the bytes behind the
/// boot zvol change, plus the device's `image-uuid` property. Destroy the
/// old `disk0` then re-clone it from the new image snapshot, matching node
/// `reprovisionBhyve` (VM.js:11120).
async fn reprovision_bhyve<H: ZoneHost>(
    host: &H,
    uuid: Uuid,
    new_image_uuid: Uuid,
    zfs_filesystem: &str,
    zpool: &str,
) -> Result<()> {
    // Locate the boot zvol from the live zone config. node requires a
    // `boot=true` disk0 and fails the reprovision otherwise (VM.js:11138) —
    // we fail closed too rather than guessing a default dataset, since a
    // wrong guess would destroy the wrong zvol.
    let export = host.zonecfg_export(uuid).await?;
    let boot = boot_disk(&export).ok_or_else(|| {
        VmadmError::Payload(format!(
            "VM {uuid} has no boot=true disk0; reprovision requires a bootable disk0"
        ))
    })?;
    let boot_ds = boot.dataset;
    let boot_match = boot.match_path;

    // Sanity: the boot disk should be the zone's own disk0 (node matches
    // `/disk0$` and verifies the boot property, VM.js:11137).
    if !boot_ds.starts_with(&format!("{zfs_filesystem}/")) {
        return Err(VmadmError::Payload(format!(
            "VM {uuid} boot disk {boot_ds} is not under the zoneroot {zfs_filesystem}"
        )));
    }

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

    // Update disk0's `image-uuid` device property to the new image, before
    // boot. node reads the current value and does a remove+add; an
    // unconditional remove (tolerant) + add is equivalent and idempotent
    // (VM.js:11277-11291).
    let doc = format!(
        "select device match={boot_match}; \
         remove property (name=image-uuid); \
         add property (name=image-uuid, value=\"{new_image_uuid}\"); end\n"
    );
    host.zonecfg(ZoneRef::Uuid(uuid), &doc).await?;

    host.zoneadm_boot(uuid).await
}

/// A boot disk located in a `zonecfg export`: its backing dataset and the
/// raw `match=` path (kept verbatim for the device-property edit).
struct BootDisk {
    dataset: String,
    match_path: String,
}

/// Parse a `zonecfg export` and return the bootable disk: the `device`
/// block carrying `(name=boot, value="true")`. Returns `None` when no
/// bootable device is present (the caller fails closed rather than guessing).
fn boot_disk(export: &str) -> Option<BootDisk> {
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
                return Some(BootDisk {
                    dataset: dataset_from_raw_path(&path),
                    match_path: path,
                });
            }
            in_device = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("set match=") {
            // `zonecfg export` quotes the value (`set match="/dev/..."`);
            // strip the quotes so both the dataset strip and the
            // `select device match=` edit use the bare device path.
            match_path = Some(unquote(rest.trim()).to_string());
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

/// Strip a single pair of surrounding double quotes (mirrors
/// `get.rs::unquote`); `zonecfg export` quotes scalar `set` values.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
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
    async fn joyent_minimal_stop_uninstall_install_boot_order() {
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
        // A running OS zone is stopped gracefully (zlogin shutdown), not
        // hard-halted (L3).
        let i_stop = index_of(&ops, "shutdown").expect("graceful stop");
        let i_uninstall = index_of(&ops, "uninstall -F").expect("uninstall");
        let i_install = index_of(&ops, "install -q").expect("install");
        let i_boot = index_of(&ops, "boot").expect("boot");
        assert!(i_stop < i_uninstall, "stop before uninstall: {ops:?}");
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
        // M2: image identity updated to the new image before boot.
        let i_attr = ops
            .iter()
            .position(|o| {
                o.contains("select attr name=dataset-uuid")
                    && o.contains(&format!("set value=\"{}\"", new_image()))
            })
            .expect("dataset-uuid update");
        assert!(i_attr < i_boot, "image identity set before boot: {ops:?}");
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

    /// C1: a delegated `/data` dataset is parked out of the zoneroot subtree
    /// BEFORE uninstall and renamed back after install, so it survives.
    #[tokio::test]
    async fn joyent_minimal_preserves_delegated_data() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("joyent-minimal"));
        let delegated = format!("zones/{}/data", uuid());
        let parked = format!("zones/{}-reprovisioning-data", uuid());
        host.set_dataset_exists(&delegated);

        reprovision_in(&host, uuid(), new_image(), "zones")
            .await
            .expect("reprovision");

        let ops = host.ops();
        let i_zoned_off =
            index_of(&ops, &format!("zfs set zoned=off {delegated}")).expect("zoned off");
        let i_rename_out = index_of(&ops, &format!("zfs rename -f {delegated} {parked}"))
            .expect("rename out");
        let i_rm_ds = index_of(&ops, &format!("remove dataset name={delegated}"))
            .expect("remove dataset resource");
        let i_uninstall = index_of(&ops, "uninstall -F").expect("uninstall");
        let i_install = index_of(&ops, "install -q").expect("install");
        let i_rename_back = index_of(&ops, &format!("zfs rename -f {parked} {delegated}"))
            .expect("rename back");
        let i_zoned_on =
            index_of(&ops, &format!("zfs set zoned=on {delegated}")).expect("zoned on");
        let i_add_ds = index_of(&ops, &format!("add dataset; set name={delegated}; end"))
            .expect("re-add dataset resource");

        assert!(i_zoned_off < i_rename_out, "{ops:?}");
        assert!(i_rename_out < i_rm_ds, "park before remove resource: {ops:?}");
        // C1: the dataset resource is removed before uninstall (so install
        // verify does not fail on the parked-away dataset) and re-added only
        // after /data is restored.
        assert!(i_rm_ds < i_uninstall, "remove resource before uninstall: {ops:?}");
        assert!(i_install < i_rename_back, "restore after install: {ops:?}");
        assert!(i_rename_back < i_zoned_on, "{ops:?}");
        assert!(i_zoned_on < i_add_ds, "re-add resource after restore: {ops:?}");
        // The data dataset is back at its original name.
        assert!(host.dataset_exists(&delegated), "data survived");
        assert!(!host.dataset_exists(&parked), "no stranded parked data");
    }

    /// C1 recovery: a crash that left data parked at the sibling is
    /// recovered (renamed back) without re-parking or destroying it.
    #[tokio::test]
    async fn joyent_minimal_recovers_stranded_parked_data() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("joyent-minimal"));
        let delegated = format!("zones/{}/data", uuid());
        let parked = format!("zones/{}-reprovisioning-data", uuid());
        // Simulate a prior crashed run: data is already parked, not live.
        host.set_dataset_exists(&parked);

        reprovision_in(&host, uuid(), new_image(), "zones")
            .await
            .expect("reprovision");

        let ops = host.ops();
        // It must NOT rename the (absent) live dataset out again.
        assert!(
            !ops.iter()
                .any(|o| o.contains(&format!("zfs rename -f {delegated} {parked}"))),
            "no re-park: {ops:?}"
        );
        // It must rename the parked data back.
        assert!(
            ops.iter()
                .any(|o| o.contains(&format!("zfs rename -f {parked} {delegated}"))),
            "rename back: {ops:?}"
        );
        assert!(host.dataset_exists(&delegated), "data recovered");
    }

    /// C1 refusal: a non-standard (extra) delegated dataset is refused
    /// rather than risk losing it.
    #[tokio::test]
    async fn joyent_minimal_refuses_non_standard_delegated() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("joyent-minimal"));
        host.set_dataset_exists(format!("zones/{}/data", uuid()));
        host.set_dataset_exists(format!("zones/{}/data2", uuid()));

        assert!(matches!(
            reprovision_in(&host, uuid(), new_image(), "zones").await,
            Err(VmadmError::Payload(_))
        ));
        // Nothing destructive ran.
        assert!(
            !host.ops().iter().any(|o| o.contains("uninstall")),
            "{:?}",
            host.ops()
        );
    }

    #[tokio::test]
    async fn bhyve_stop_destroy_clone_boot_order() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("bhyve"));
        host.set_export(uuid(), bhyve_export());

        reprovision_in(&host, uuid(), new_image(), "zones")
            .await
            .expect("reprovision");

        let ops = host.ops();
        let disk0 = format!("zones/{}/disk0", uuid());
        // bhyve is hard-halted (no in-guest shutdown).
        let i_stop = index_of(&ops, "halt").expect("halt");
        let i_destroy = index_of(&ops, &format!("zfs destroy -rF {disk0}")).expect("destroy disk0");
        let i_clone = index_of(&ops, "zfs clone -F").expect("clone");
        let i_imgupd = ops
            .iter()
            .position(|o| o.contains("select device") && o.contains("image-uuid"))
            .expect("image-uuid device update");
        let i_boot = index_of(&ops, "boot").expect("boot");
        assert!(i_stop < i_destroy, "halt before destroy: {ops:?}");
        assert!(i_destroy < i_clone, "destroy before clone: {ops:?}");
        assert!(i_clone < i_imgupd, "clone before image update: {ops:?}");
        assert!(i_imgupd < i_boot, "image update before boot: {ops:?}");
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
        // The device update names the new image.
        assert!(
            ops.iter()
                .any(|o| o.contains(&format!("value=\"{}\"", new_image()))),
            "new image in device prop: {ops:?}"
        );
        // The JM uninstall path must NOT run for bhyve.
        assert!(
            index_of(&ops, "uninstall -F").is_none(),
            "bhyve keeps the zone config"
        );
    }

    /// L4: a bhyve zone with no boot=true disk0 fails closed (no destroy).
    #[tokio::test]
    async fn bhyve_no_boot_disk_fails_closed() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("bhyve"));
        // No export set -> empty export -> no boot disk found.
        assert!(matches!(
            reprovision_in(&host, uuid(), new_image(), "zones").await,
            Err(VmadmError::Payload(_))
        ));
        assert!(
            !host.ops().iter().any(|o| o.contains("zfs destroy")),
            "no destroy without a boot disk: {:?}",
            host.ops()
        );
    }

    /// M1: a held zoneroot `@indestructible` snapshot blocks reprovision
    /// before any stop/uninstall/destroy.
    #[tokio::test]
    async fn reprovision_indestructible_refuses() {
        let host = FakeHost::new();
        host.set_zone_info(uuid(), running("joyent-minimal"));
        host.set_holds(format!("zones/{}@indestructible", uuid()), &["do_not_destroy"]);
        assert!(matches!(
            reprovision_in(&host, uuid(), new_image(), "zones").await,
            Err(VmadmError::Indestructible(_))
        ));
        let ops = host.ops();
        assert!(!ops.iter().any(|o| o.contains("halt")), "{ops:?}");
        assert!(!ops.iter().any(|o| o.contains("shutdown")), "{ops:?}");
        assert!(!ops.iter().any(|o| o.contains("uninstall")), "{ops:?}");
        assert!(!ops.iter().any(|o| o.contains("zfs destroy")), "{ops:?}");
    }

    #[test]
    fn boot_disk_parse_picks_boot_true_device() {
        let b = boot_disk(&bhyve_export()).expect("boot disk");
        assert_eq!(b.dataset, format!("zones/{}/disk0", uuid()));
        assert_eq!(
            b.match_path,
            format!("/dev/zvol/rdsk/zones/{}/disk0", uuid())
        );
    }

    #[test]
    fn boot_disk_parse_skips_non_boot_devices() {
        let export = format!(
            "add device\nset match=/dev/zvol/rdsk/zones/{u}/disk1\n\
             add property (name=boot, value=\"false\")\nend\n\
             add device\nset match=/dev/zvol/rdsk/zones/{u}/disk0\n\
             add property (name=boot, value=\"true\")\nend\n",
            u = uuid()
        );
        assert_eq!(
            boot_disk(&export).map(|b| b.dataset),
            Some(format!("zones/{}/disk0", uuid()))
        );
    }

    #[test]
    fn boot_disk_parse_none_when_no_boot_device() {
        let export = "create -b\nset zonepath=/zones/x\n".to_string();
        assert!(boot_disk(&export).is_none());
    }

    /// The wait-for-halted gate must treat transient stopping states as
    /// NOT-halted (a `shutting_down` zone fails `zoneadm uninstall`).
    #[test]
    fn halted_state_classification() {
        assert!(is_halted_state("installed"));
        assert!(is_halted_state("configured"));
        assert!(is_halted_state("incomplete"));
        assert!(!is_halted_state("running"));
        assert!(!is_halted_state("shutting_down"));
        assert!(!is_halted_state("down"));
        assert!(!is_halted_state("ready"));
        assert!(!is_halted_state("mounted"));
    }

    /// The real `zonecfg export` quotes the device `match` value; the boot
    /// disk dataset and the (unquoted) match path must still parse out.
    #[test]
    fn boot_disk_parse_handles_quoted_match() {
        let u = uuid();
        let export = format!(
            "add device\nset match=\"/dev/zvol/rdsk/zones/{u}/disk0\"\n\
             add property (name=boot, value=\"true\")\nend\n"
        );
        let b = boot_disk(&export).expect("boot disk");
        assert_eq!(b.dataset, format!("zones/{u}/disk0"));
        assert_eq!(b.match_path, format!("/dev/zvol/rdsk/zones/{u}/disk0"));
    }
}
