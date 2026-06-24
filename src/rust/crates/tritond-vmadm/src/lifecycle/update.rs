// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The in-place update verbs. Currently a single one: grow the boot disk.
//!
//! This is the native equivalent of the agent's two-step `vmadm update`
//! dance (`tritonagent::vmadm::grow_boot_disk`): first enlarge the
//! `flexible-disk-size` budget so the zoneroot quota can hold the bigger
//! zvol, then grow the boot zvol's `volsize`, then re-run the RFD-0148
//! quota so the zoneroot `quota`/`reservation`/`refreservation` track the
//! new total. We drive the host primitives directly rather than shelling
//! to node `vmadm`, so the whole flow is unit-testable against `FakeHost`.

use uuid::Uuid;

use crate::backend::traits::ZoneHost;
use crate::error::{Result, VmadmError};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;

/// Grow the boot disk of bhyve zone `uuid` to `new_size_bytes`.
///
/// Grow-only and idempotent: a target at or below the current `volsize` is
/// a no-op (info-logged). The growth path bumps the zonecfg
/// `flexible-disk-size` attr to cover the new total, sets the boot zvol's
/// `volsize`, then re-applies the RFD-0148 zoneroot quota (the same
/// `refquota` -> per-volume `refreservation=auto` readback ->
/// `quota`/`reservation`/`refreservation` sequence that create runs).
///
/// The running guest does NOT see the new capacity until it reboots —
/// bhyve reads the block-device size at boot, and cloud-init grows the
/// partition + filesystem from there. Callers that need the guest to act
/// on the new size must schedule a reboot after this returns.
pub async fn grow_boot_disk<H: ZoneHost>(host: &H, uuid: Uuid, new_size_bytes: u64) -> Result<()> {
    let _lock = crate::lock::ZoneLock::acquire(uuid).await?;
    grow_boot_disk_in(host, uuid, new_size_bytes).await
}

/// [`grow_boot_disk`] without acquiring the per-zone lock (tests, and
/// callers that already hold it).
pub async fn grow_boot_disk_in<H: ZoneHost>(
    host: &H,
    uuid: Uuid,
    new_size_bytes: u64,
) -> Result<()> {
    let new_mib = new_size_bytes / MIB;

    let export = host.zonecfg_export(uuid).await?;
    let boot_ds = boot_disk_dataset(&export)
        .ok_or_else(|| VmadmError::Payload(format!("zone {uuid} has no boot disk to resize")))?;

    // Current boot zvol size, for the grow-only guard.
    let props = host.zfs_get_props(&boot_ds, &["volsize"]).await?;
    let current_volsize: u64 = props
        .get("volsize")
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| VmadmError::Payload(format!("dataset {boot_ds} has no readable volsize")))?;

    if new_size_bytes <= current_volsize {
        tracing::info!(
            %uuid,
            current_volsize,
            target = new_size_bytes,
            "resize-disk: boot disk already at/above target; nothing to do",
        );
        return Ok(());
    }

    // The new flexible-disk-size budget must cover the new total: the new
    // boot disk plus every other (non-boot) zvol. We read the sibling
    // volumes off the zoneroot so a multi-disk guest still sizes the pool
    // correctly, and never shrink the existing budget.
    let zoneroot = zoneroot_of(&boot_ds);
    let volumes = host.zfs_list_volumes(&zoneroot).await?;
    let other_bytes: u64 = volumes
        .iter()
        .filter(|(name, _)| name != &boot_ds)
        .map(|(_, sz)| *sz)
        .sum();
    let new_total_mib = (new_mib).saturating_add(other_bytes / MIB);
    let current_flex_mib = flexible_disk_size_mib(&export);
    let new_flex_mib = new_total_mib.max(current_flex_mib.unwrap_or(0));

    // Step 1: enlarge the flexible-disk-size budget (only if it grows).
    if Some(new_flex_mib) != current_flex_mib {
        host.zonecfg(
            crate::backend::traits::ZoneRef::Uuid(uuid),
            &format!("select attr name=flexible-disk-size; set value=\"{new_flex_mib}\"; end\n"),
        )
        .await?;
    }

    // Step 2: grow the boot zvol. ZFS volsize only ever grows here (the
    // guard above rejected a shrink), so this is safe.
    host.zfs_set(&boot_ds, &[("volsize".to_string(), format!("{new_mib}M"))])
        .await?;

    // Step 3: re-run the RFD-0148 quota so the zoneroot tracks the bigger
    // disk. We reconstruct the `quota` GiB input from the zoneroot's
    // current `refquota` (create set it to `max(quota,DEFAULT)*GiB`); see
    // the summary note on the missing direct read.
    apply_bhyve_quota(host, &zoneroot, new_flex_mib).await
}

/// Re-apply the RFD-0148 zoneroot quota after a disk grow. This mirrors
/// `create::apply_bhyve_quota` step-for-step (the same op sequence), kept
/// local so the update path has no back-dependency on the create module.
async fn apply_bhyve_quota<H: ZoneHost>(host: &H, zoneroot: &str, flex_mib: u64) -> Result<()> {
    // Reconstruct the requested-quota GiB from the existing refquota. The
    // zoneroot's refquota was set to `max(quota_gib, DEFAULT)*GiB` at
    // create; reading it back and dividing recovers a quota_gib that the
    // `.max(DEFAULT)` inside compute_bhyve_quota leaves stable.
    let props = host.zfs_get_props(zoneroot, &["refquota"]).await?;
    let quota_gib = props
        .get("refquota")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|b| b / GIB)
        .unwrap_or(crate::constants::DEFAULT_BHYVE_QUOTA_GIB);

    let max_quota = quota_gib.max(crate::constants::DEFAULT_BHYVE_QUOTA_GIB) * GIB;

    host.zfs_set(
        zoneroot,
        &[
            ("refreservation".to_string(), "none".to_string()),
            ("refquota".to_string(), max_quota.to_string()),
        ],
    )
    .await?;

    let volumes = host.zfs_list_volumes(zoneroot).await?;
    let volsize: u64 = volumes.iter().map(|(_, s)| *s).sum();
    for (name, _) in &volumes {
        host.zfs_set(name, &[("refreservation".to_string(), "auto".to_string())])
            .await?;
    }
    let volrefres = host.zfs_sum_volume_refreservation(zoneroot).await?;

    let plan = crate::quota::compute_bhyve_quota(quota_gib, Some(flex_mib), volsize, volrefres);
    host.zfs_set(
        zoneroot,
        &[
            ("quota".to_string(), plan.quota.zfs_arg()),
            ("reservation".to_string(), plan.reservation.zfs_arg()),
            (
                "refreservation".to_string(),
                plan.refreservation_bytes.to_string(),
            ),
        ],
    )
    .await
}

/// Find the boot disk's backing dataset in a `zonecfg export`. The boot
/// device is an `add device` block whose `set match=` is the raw zvol path
/// (`/dev/zvol/rdsk/<ds>`) and which carries `boot` set true. Returns the
/// `<ds>` (e.g. `zones/<uuid>/disk0`).
///
/// We scan the export line by line, tracking the current `device` block's
/// `match` dataset and whether it is the boot device, because `zonecfg
/// export` emits properties in a flat, order-stable stream.
fn boot_disk_dataset(export: &str) -> Option<String> {
    let mut in_device = false;
    let mut match_ds: Option<String> = None;
    let mut is_boot = false;

    for raw in export.lines() {
        let line = raw.trim();
        if line == "add device" {
            in_device = true;
            match_ds = None;
            is_boot = false;
            continue;
        }
        if !in_device {
            continue;
        }
        if line == "end" {
            if is_boot && let Some(ds) = match_ds.take() {
                return Some(ds);
            }
            in_device = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("set match=") {
            match_ds = raw_zvol_to_dataset(rest.trim());
        } else if is_boot_property(line) {
            is_boot = true;
        }
    }
    None
}

/// `zonecfg export` renders a device property as
/// `add property (name=boot,value="true")` (whitespace within the parens
/// varies across platforms), so we match on the name + a truthy value
/// rather than an exact string.
fn is_boot_property(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains("name=boot,") && compact.contains("value=\"true\"")
}

/// Strip the `/dev/zvol/{rdsk,dsk}/` prefix from a device `match` to get
/// the bare ZFS dataset name.
fn raw_zvol_to_dataset(m: &str) -> Option<String> {
    m.strip_prefix("/dev/zvol/rdsk/")
        .or_else(|| m.strip_prefix("/dev/zvol/dsk/"))
        .map(|s| s.to_string())
}

/// The zoneroot dataset is the boot zvol's parent (`zones/<uuid>/disk0` ->
/// `zones/<uuid>`).
fn zoneroot_of(boot_ds: &str) -> String {
    boot_ds
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_else(|| boot_ds.to_string())
}

/// Read the current `flexible-disk-size` attr value (MiB) from a `zonecfg
/// export`. Rendered as the `add attr` triple
/// `add attr; set name=flexible-disk-size; set value="20480"; end` (or the
/// single-line `add attr (name=...,...)` form on some platforms), so we
/// match name + value across both.
fn flexible_disk_size_mib(export: &str) -> Option<u64> {
    let mut saw_name = false;
    for raw in export.lines() {
        let line = raw.trim();
        // Single-line form: `add attr (name=flexible-disk-size,
        // type=string, value="20480")`.
        if line.contains("name=flexible-disk-size")
            && line.contains("value=")
            && let Some(v) = attr_value_from_parenthesised(line)
        {
            return Some(v);
        }
        // Multi-line form.
        if line == "set name=flexible-disk-size" {
            saw_name = true;
            continue;
        }
        if saw_name {
            if let Some(rest) = line.strip_prefix("set value=") {
                return rest.trim().trim_matches('"').parse::<u64>().ok();
            }
            if line == "end" {
                saw_name = false;
            }
        }
    }
    None
}

/// Extract `value="<n>"` from a parenthesised `add attr (...)` line.
fn attr_value_from_parenthesised(line: &str) -> Option<u64> {
    let after = line.split("value=").nth(1)?;
    let digits: String = after
        .trim_start_matches('"')
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u64>().ok()
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

    /// A bhyve `zonecfg export` with a single boot disk (disk0) and a
    /// 20480 MiB flexible-disk-size, in the multi-line render form.
    fn bhyve_export() -> String {
        let u = uuid();
        format!(
            "create -b\n\
             set brand=bhyve\n\
             set zonepath=/zones/{u}\n\
             add attr\n\
             set name=flexible-disk-size\n\
             set type=int\n\
             set value=\"20480\"\n\
             end\n\
             add device\n\
             set match=/dev/zvol/rdsk/zones/{u}/disk0\n\
             add property (name=boot,value=\"true\")\n\
             add property (name=model,value=\"virtio\")\n\
             end\n"
        )
    }

    /// Seed the fake with a running bhyve zone whose boot zvol pre-exists.
    ///
    /// `FakeHost` only tracks volumes that were created through it (there is
    /// no public `volumes` setter — a gap for the update path, where the
    /// boot zvol pre-exists), so we materialise the boot zvol via a real
    /// `zfs_create_volume` call. That call lands in the op log BEFORE the
    /// grow runs, so the grow-ordering assertions (which index from the
    /// flex bump onward) are unaffected, and the per-volume `auto` op the
    /// quota step emits (`zfs set refreservation=auto ...`) does not collide
    /// with this `zfs create -V` seed op.
    async fn install_state(host: &FakeHost, volsize_bytes: u64, refquota_bytes: u64) {
        let u = uuid();
        host.set_zone_info(
            u,
            ZoneRuntime {
                zoneid: Some(3),
                state: "running".to_string(),
                brand: "bhyve".to_string(),
                zonepath: format!("/zones/{u}"),
                zonedid: Some(3),
            },
        );
        host.set_export(u, bhyve_export());

        let boot_ds = format!("zones/{u}/disk0");
        host.zfs_create_volume(&boot_ds, volsize_bytes / MIB, &[])
            .await
            .expect("seed boot zvol");
        let mut vol = BTreeMap::new();
        vol.insert("volsize".to_string(), volsize_bytes.to_string());
        host.set_zfs_props(boot_ds, vol);

        let zoneroot = format!("zones/{u}");
        let mut root = BTreeMap::new();
        root.insert("refquota".to_string(), refquota_bytes.to_string());
        host.set_zfs_props(zoneroot, root);
    }

    fn index_of(ops: &[String], needle: &str) -> Option<usize> {
        ops.iter().position(|o| o.contains(needle))
    }

    #[tokio::test]
    async fn grow_smaller_target_is_noop() {
        let host = FakeHost::new();
        // Boot zvol is 20 GiB; ask to "grow" to 10 GiB.
        install_state(&host, 20 * GIB, GIB).await;

        grow_boot_disk_in(&host, uuid(), 10 * GIB)
            .await
            .expect("noop ok");

        let ops = host.ops();
        // It read the export + volsize, then did nothing mutating.
        assert!(index_of(&ops, "zonecfg -u").is_some(), "read export");
        assert!(
            !ops.iter().any(|o| o.starts_with("zfs set")),
            "no zfs set on noop: {ops:?}"
        );
        assert!(
            !ops.iter().any(|o| o.contains("select attr")),
            "no flex bump on noop: {ops:?}"
        );
    }

    #[tokio::test]
    async fn grow_equal_target_is_noop() {
        let host = FakeHost::new();
        install_state(&host, 20 * GIB, GIB).await;
        grow_boot_disk_in(&host, uuid(), 20 * GIB)
            .await
            .expect("noop ok");
        assert!(
            !host.ops().iter().any(|o| o.starts_with("zfs set")),
            "equal target is a no-op",
        );
    }

    #[tokio::test]
    async fn grow_bumps_flex_then_volsize_then_quota() {
        let host = FakeHost::new();
        // 20 GiB boot zvol, refquota 1 GiB; grow to 40 GiB.
        install_state(&host, 20 * GIB, GIB).await;

        grow_boot_disk_in(&host, uuid(), 40 * GIB)
            .await
            .expect("grow ok");

        let ops = host.ops();
        let u = uuid();
        let root = format!("zones/{u}");
        let boot_ds = format!("zones/{u}/disk0");

        // Ordered: flex bump -> volsize set -> quota refquota/none ->
        // per-volume auto -> final quota set.
        let i_flex = index_of(&ops, "select attr name=flexible-disk-size").expect("flex bump");
        let i_volsize =
            index_of(&ops, &format!("zfs set volsize=40960M {boot_ds}")).expect("volsize grow");
        let i_refquota =
            index_of(&ops, "zfs set refreservation=none refquota=").expect("quota step 1");
        let i_auto = index_of(&ops, &format!("zfs set refreservation=auto {boot_ds}"))
            .expect("per-volume auto");
        let i_final = ops
            .iter()
            .position(|o| o.starts_with("zfs set quota=") && o.contains("reservation="))
            .expect("final quota");

        assert!(i_flex < i_volsize, "flex before volsize: {ops:?}");
        assert!(i_volsize < i_refquota, "volsize before quota: {ops:?}");
        assert!(
            i_refquota < i_auto && i_auto < i_final,
            "quota order: {ops:?}"
        );

        // The flex budget grew to cover the 40 GiB boot disk (40960 MiB);
        // no other disks, so the new budget is exactly 40960.
        assert!(
            ops.iter().any(|o| o.contains("set value=\"40960\"")),
            "flex set to new total: {ops:?}",
        );
        // refquota was recomputed from the reconstructed quota_gib (1 GiB
        // refquota -> quota_gib 1 -> max(1,DEFAULT)=1 GiB).
        assert!(
            ops.iter()
                .any(|o| o.contains(&format!("refquota={}", GIB)) && o.ends_with(&root)),
            "refquota reapplied: {ops:?}",
        );
    }

    #[test]
    fn parses_boot_dataset_and_flex_from_export() {
        let export = bhyve_export();
        assert_eq!(
            boot_disk_dataset(&export).as_deref(),
            Some(format!("zones/{}/disk0", uuid()).as_str())
        );
        assert_eq!(flexible_disk_size_mib(&export), Some(20480));
    }

    #[test]
    fn parses_single_line_attr_form() {
        let u = uuid();
        let export = format!(
            "add attr (name=flexible-disk-size, type=int, value=\"30720\")\n\
             add device\n\
             set match=/dev/zvol/rdsk/zones/{u}/disk0\n\
             add property (name=boot, value=\"true\")\n\
             end\n"
        );
        assert_eq!(flexible_disk_size_mib(&export), Some(30720));
        // Whitespace inside the boot property parens still matches.
        assert_eq!(
            boot_disk_dataset(&export).as_deref(),
            Some(format!("zones/{u}/disk0").as_str())
        );
    }

    #[test]
    fn ignores_non_boot_device() {
        // A data disk (boot=false) must not be picked as the boot disk.
        let u = uuid();
        let export = format!(
            "add device\n\
             set match=/dev/zvol/rdsk/zones/{u}/disk1\n\
             add property (name=boot,value=\"false\")\n\
             end\n"
        );
        assert_eq!(boot_disk_dataset(&export), None);
    }
}
