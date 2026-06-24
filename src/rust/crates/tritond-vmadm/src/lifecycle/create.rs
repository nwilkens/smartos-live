// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The create orchestrator. Reproduces node `VM.js` `createZone` ->
//! `installZone` for joyent-minimal, with a resource journal so a failure
//! at any step rolls the whole create back deterministically (the half
//! node only half-implements). bhyve create lands in V-8.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::backend::traits::{InstallArgs, NicTagRegistrar, ProvisionOutcome, ZoneHost, ZoneRef};
use crate::brand::Brand;
use crate::error::{Result, VmadmError};
use crate::files;
use crate::journal::{JOURNAL_DIR, ResourceJournal, UndoAction};
use crate::model::spec::VmCreateSpec;
use crate::zonecfg::build_create_zonecfg;

/// Create a zone from a normalized spec, journaling every mutating step.
/// Returns the zone uuid (== the instance id) on success. Holds the
/// per-zone config lock for the whole operation.
pub async fn create<H, N>(
    host: &H,
    nictags: &N,
    spec: &VmCreateSpec,
    now: DateTime<Utc>,
) -> Result<Uuid>
where
    H: ZoneHost,
    N: NicTagRegistrar,
{
    let _lock = crate::lock::ZoneLock::acquire(spec.uuid).await?;
    create_in(host, nictags, spec, now, Path::new(JOURNAL_DIR)).await
}

/// [`create`] with an explicit journal directory (tests).
pub async fn create_in<H, N>(
    host: &H,
    nictags: &N,
    spec: &VmCreateSpec,
    now: DateTime<Utc>,
    journal_dir: &Path,
) -> Result<Uuid>
where
    H: ZoneHost,
    N: NicTagRegistrar,
{
    // Uniqueness guard (createZone:12576): a configured zone with this
    // uuid is an error, never a silent overwrite.
    if host.zone_configured(spec.uuid).await? {
        return Err(VmadmError::AlreadyExists(spec.uuid));
    }

    let mut journal = ResourceJournal::open(spec.uuid, journal_dir).await?;

    let result = match spec.brand {
        Brand::JoyentMinimal => create_joyent_minimal(host, nictags, spec, now, &mut journal).await,
        Brand::Bhyve => create_bhyve(host, nictags, spec, now, &mut journal).await,
    };

    match result {
        Ok(CreateOutcome::Ok) => {
            journal.finish_ok().await?;
            Ok(spec.uuid)
        }
        // The infrastructure was built but the guest failed to provision;
        // node keeps the zone for inspection rather than rolling back.
        Ok(CreateOutcome::KeepZoneError(e)) => {
            journal.finish_ok().await?;
            Err(e)
        }
        Err(e) => {
            let failed = rollback(host, &journal).await;
            if failed == 0 {
                // Clean rollback: drop the journal so it does not leak.
                let _ = journal.finish_ok().await;
                Err(e)
            } else {
                // Some undo steps failed: host residue remains. Keep the
                // journal for diagnosis and surface that to the caller
                // rather than burying it in a log line.
                Err(VmadmError::RollbackResidue(format!(
                    "{e}; {failed} rollback step(s) failed; journal retained for {}",
                    spec.uuid
                )))
            }
        }
    }
}

/// The disposition of a brand create: clean success, or "the zone is
/// built but something guest-side failed — surface the error but leave the
/// zone in place (no rollback)".
enum CreateOutcome {
    Ok,
    KeepZoneError(VmadmError),
}

async fn create_joyent_minimal<H, N>(
    host: &H,
    nictags: &N,
    spec: &VmCreateSpec,
    now: DateTime<Utc>,
    journal: &mut ResourceJournal,
) -> Result<CreateOutcome>
where
    H: ZoneHost,
    N: NicTagRegistrar,
{
    let zonepath = Path::new(&spec.zonepath);

    // JM-2: write the zone configuration.
    let cfg_idx = journal
        .plan(
            "zone-config",
            UndoAction::ZonecfgDeleteForce { uuid: spec.uuid },
        )
        .await?;
    journal.mark_started(cfg_idx).await?;
    let doc = build_create_zonecfg(spec, now);
    host.zonecfg(ZoneRef::Zonename(spec.zonename.clone()), &doc)
        .await?;
    journal.mark_done(cfg_idx).await?;

    if spec.create_only {
        return Ok(CreateOutcome::Ok);
    }

    // JM-3: install (the brand's jinstall clones zones/<image>@final).
    let install_idx = journal
        .plan(
            "zone-install",
            UndoAction::ZoneadmUninstallForce { uuid: spec.uuid },
        )
        .await?;
    journal.mark_started(install_idx).await?;
    let image_uuid = spec.image_uuid.ok_or_else(|| {
        VmadmError::Payload("joyent-minimal create requires image_uuid".to_string())
    })?;
    host.zoneadm_install(&InstallArgs {
        zonename: spec.zonename.clone(),
        quota_gib: spec.quota_gib,
        image_uuid: Some(image_uuid),
        no_dataset: true,
    })
    .await?;
    journal.mark_done(install_idx).await?;

    // JM-5: delegated dataset (optional).
    if spec.delegate_dataset {
        let data_ds = format!("{}/data", spec.zfs_filesystem);
        let idx = journal
            .plan(
                "delegated-dataset",
                UndoAction::ZfsDestroyRecursive {
                    dataset: data_ds.clone(),
                },
            )
            .await?;
        journal.mark_started(idx).await?;
        host.zfs_create(&data_ds, &[]).await?;
        host.zonecfg(
            ZoneRef::Uuid(spec.uuid),
            &format!("add dataset; set name={data_ds}; end\n"),
        )
        .await?;
        journal.mark_done(idx).await?;
    }

    // JM-6/7: config files + provisioning marker (covered by the install
    // undo, which removes the zoneroot, so not separately journaled).
    files::write_create_config(
        zonepath,
        &spec.customer_metadata,
        &spec.internal_metadata,
        &spec.tags,
        &spec.routes,
    )
    .await?;
    files::write_provisioning_marker(zonepath).await?;

    // JM-8: mdata:execute SMF start timeout.
    host.svccfg_mdata_exec_timeout(&spec.zonepath, spec.mdata_exec_timeout_secs)
        .await?;

    // JM-9: zoneroot network files.
    let hostname = spec
        .hostname
        .clone()
        .unwrap_or_else(|| spec.zonename.clone());
    files::write_zone_netfiles(zonepath, &spec.nics, &hostname).await?;

    // The proteus nic-tag precondition: every NIC link must be a
    // registered nic_tag before boot, else the joyent brand statechange
    // hook bails "undefined VNIC". FAIL-CLOSED for joyent-minimal.
    ensure_nic_tags_fail_closed(nictags, spec).await?;

    // JM-12: drop the provisioning attr now that setup is complete.
    host.zonecfg(ZoneRef::Uuid(spec.uuid), "remove attr name=provisioning\n")
        .await?;

    // JM-13: boot + wait + clear transition.
    boot_and_finalize(host, spec).await
}

/// The shared boot tail for both brands: boot, optionally wait for the
/// in-zone provisioning marker to clear, then best-effort drop the
/// transition attr. After boot the zone is built, so nothing here returns
/// a hard `Err` that would roll back a good VM — a guest-side failure or a
/// cleanup error keeps the zone for inspection (matching node).
async fn boot_and_finalize<H: ZoneHost>(host: &H, spec: &VmCreateSpec) -> Result<CreateOutcome> {
    if !spec.autoboot {
        return Ok(CreateOutcome::Ok);
    }
    // The zone is fully built by now. A boot failure must NOT roll back the
    // installed zone (zoneadm boot can fail after the zone reaches running,
    // and tearing it down would destroy a good VM) — surface it but keep
    // the zone, matching node.
    if let Err(e) = host.zoneadm_boot(spec.uuid).await {
        return Ok(CreateOutcome::KeepZoneError(e));
    }

    if spec.nowait {
        // Caller opted out of waiting; node leaves the transition attr in
        // place so whoever confirms provisioning later can clear it.
        return Ok(CreateOutcome::Ok);
    }

    let outcome = match host
        .wait_provisioning(
            &spec.zonepath,
            Duration::from_secs(crate::constants::PROVISION_TIMEOUT_SECS),
        )
        .await
    {
        Ok(o) => o,
        Err(e) => return Ok(CreateOutcome::KeepZoneError(e)),
    };
    match outcome {
        ProvisionOutcome::Success => {}
        ProvisionOutcome::Failure(reason) => {
            return Ok(CreateOutcome::KeepZoneError(VmadmError::Payload(format!(
                "zone {} provisioning failed: {reason}",
                spec.uuid
            ))));
        }
        ProvisionOutcome::Timeout => {
            return Ok(CreateOutcome::KeepZoneError(VmadmError::Payload(format!(
                "zone {} timed out waiting for provisioning",
                spec.uuid
            ))));
        }
    }

    if let Err(e) = host
        .zonecfg(ZoneRef::Uuid(spec.uuid), "remove attr name=transition\n")
        .await
    {
        tracing::warn!(uuid = %spec.uuid, error = %e, "failed to clear transition attr; zone is up");
    }
    Ok(CreateOutcome::Ok)
}

/// bhyve create: zoneroot dataset -> per-disk clone/create -> RFD-0148
/// quota -> zonecfg -> install (no `-t`; root + disks pre-created) ->
/// config files -> nic-tag (best-effort; viona tolerates a raw link) ->
/// boot. The NoCloud cidata seed disk lands in V-9.
async fn create_bhyve<H, N>(
    host: &H,
    nictags: &N,
    spec: &VmCreateSpec,
    now: DateTime<Utc>,
    journal: &mut ResourceJournal,
) -> Result<CreateOutcome>
where
    H: ZoneHost,
    N: NicTagRegistrar,
{
    let zonepath = Path::new(&spec.zonepath);

    // Uniqueness on the DATASET, not just zonecfg: bhyve pre-creates
    // zones/<uuid>, and the rollback for a failed `zfs create` is
    // `zfs destroy -rf zones/<uuid>`. If the dataset already exists (an
    // orphan from a prior half-cleaned op, or a manual dataset), the
    // create would fail and rollback would destroy data this create never
    // owned. Refuse up front. (`zfs_snapshot_exists` runs `zfs get name`,
    // which is exit-0 for any existing zfs object, dataset or snapshot.)
    if host.zfs_snapshot_exists(&spec.zfs_filesystem).await? {
        return Err(VmadmError::AlreadyExists(spec.uuid));
    }

    // BH-2: zoneroot dataset (created before the disks so they are child
    // datasets zones/<uuid>/diskN).
    let root_idx = journal
        .plan(
            "zoneroot",
            UndoAction::ZfsDestroyRecursive {
                dataset: spec.zfs_filesystem.clone(),
            },
        )
        .await?;
    journal.mark_started(root_idx).await?;
    host.zfs_create(&spec.zfs_filesystem, &[]).await?;
    journal.mark_done(root_idx).await?;

    // BH-3: clone or create each disk zvol (the zoneroot undo covers them,
    // but journal each so a partial disk loop still rolls back cleanly).
    for disk in &spec.disks {
        if disk.effective_media() == crate::model::disk::DiskMedia::Cdrom || disk.nocreate {
            continue;
        }
        let idx = journal
            .plan(
                "zvol",
                UndoAction::ZfsDestroyRecursive {
                    dataset: disk.zfs_filesystem.clone(),
                },
            )
            .await?;
        journal.mark_started(idx).await?;
        if let Some(image) = disk.image_uuid {
            clone_image_disk(host, spec, disk, image).await?;
        } else {
            let size = disk.size_mib.or(disk.image_size_mib).ok_or_else(|| {
                VmadmError::Payload(format!("bhyve disk {} has no size", disk.disk_idx))
            })?;
            host.zfs_create_volume(&disk.zfs_filesystem, size, &blank_volume_props(disk))
                .await?;
        }
        // The NoCloud cidata seed: format the freshly-created zvol FAT16
        // and write the cloud-init files.
        if disk.cloudinit_seed {
            host.build_nocloud_seed(&disk.raw_path, &crate::seed::build_seed_files(spec))
                .await?;
        }
        journal.mark_done(idx).await?;
    }

    // BH-4: RFD-0148 quota on the zoneroot.
    apply_bhyve_quota(host, spec).await?;

    // BH-5: zone configuration.
    let cfg_idx = journal
        .plan(
            "zone-config",
            UndoAction::ZonecfgDeleteForce { uuid: spec.uuid },
        )
        .await?;
    journal.mark_started(cfg_idx).await?;
    host.zonecfg(
        ZoneRef::Zonename(spec.zonename.clone()),
        &build_create_zonecfg(spec, now),
    )
    .await?;
    journal.mark_done(cfg_idx).await?;

    if spec.create_only {
        return Ok(CreateOutcome::Ok);
    }

    // BH-6: install (no clone; `-t` only if a top-level image_uuid is set).
    let install_idx = journal
        .plan(
            "zone-install",
            UndoAction::ZoneadmUninstallForce { uuid: spec.uuid },
        )
        .await?;
    journal.mark_started(install_idx).await?;
    host.zoneadm_install(&InstallArgs {
        zonename: spec.zonename.clone(),
        quota_gib: spec.quota_gib,
        image_uuid: spec.image_uuid,
        no_dataset: true,
    })
    .await?;
    journal.mark_done(install_idx).await?;

    // BH-7: config files (covered by the install undo).
    files::write_create_config(
        zonepath,
        &spec.customer_metadata,
        &spec.internal_metadata,
        &spec.tags,
        &spec.routes,
    )
    .await?;

    // bhyve nic-tags are best-effort: viona attaches the raw link, so an
    // unregistered tag does not block boot (unlike a joyent-minimal vnic).
    ensure_nic_tags_best_effort(nictags, spec).await;

    host.zonecfg(ZoneRef::Uuid(spec.uuid), "remove attr name=provisioning\n")
        .await?;

    boot_and_finalize(host, spec).await
}

/// Clone an image-backed bhyve disk from `zones/<image>@final`, falling
/// back to snapshotting the image dataset when `@final` is absent.
async fn clone_image_disk<H: ZoneHost>(
    host: &H,
    spec: &VmCreateSpec,
    disk: &crate::model::disk::DiskSpec,
    image: uuid::Uuid,
) -> Result<()> {
    let mut snapshot = format!("{}/{image}@final", spec.zpool);
    if !host.zfs_snapshot_exists(&snapshot).await? {
        snapshot = format!("{}/{image}@{}", spec.zpool, disk.uuid);
        host.zfs_snapshot(&snapshot).await?;
    }
    host.zfs_clone(&snapshot, &disk.zfs_filesystem, &clone_props(disk))
        .await
}

/// `-o` props for a cloned image disk (volsize from the disk's tracked
/// size; refreservation defaults to `auto`).
fn clone_props(disk: &crate::model::disk::DiskSpec) -> Vec<(String, String)> {
    use crate::model::disk::Refreservation;
    let mut props = Vec::new();
    if let Some(c) = &disk.compression {
        props.push(("compression".to_string(), c.clone()));
    }
    if let Some(bs) = disk.block_size {
        props.push(("volblocksize".to_string(), bs.to_string()));
    }
    if let Some(sz) = disk.size_mib {
        props.push(("volsize".to_string(), format!("{sz}M")));
    }
    match disk.refreservation {
        Refreservation::Auto => props.push(("refreservation".to_string(), "auto".to_string())),
        Refreservation::Bytes(b) => props.push(("refreservation".to_string(), b.to_string())),
        Refreservation::None => {}
    }
    props
}

/// `-o` props for a blank data zvol (refreservation flag omitted when auto
/// so ZFS computes it).
fn blank_volume_props(disk: &crate::model::disk::DiskSpec) -> Vec<(String, String)> {
    use crate::model::disk::Refreservation;
    let mut props = Vec::new();
    if let Some(c) = &disk.compression {
        props.push(("compression".to_string(), c.clone()));
    }
    if let Some(bs) = disk.block_size {
        props.push(("volblocksize".to_string(), bs.to_string()));
    }
    if let Refreservation::Bytes(b) = disk.refreservation {
        props.push(("refreservation".to_string(), b.to_string()));
    }
    props
}

/// Apply the RFD-0148 quota to the bhyve zoneroot (`setQuotaBhyve`): set
/// `refquota` + per-volume `refreservation=auto`, read the auto values
/// back, then set the final `quota`/`reservation`/`refreservation`.
async fn apply_bhyve_quota<H: ZoneHost>(host: &H, spec: &VmCreateSpec) -> Result<()> {
    const GIB: u64 = 1024 * 1024 * 1024;
    let root = &spec.zfs_filesystem;
    let max_quota = spec
        .quota_gib
        .max(crate::constants::DEFAULT_BHYVE_QUOTA_GIB)
        * GIB;

    host.zfs_set(
        root,
        &[
            ("refreservation".to_string(), "none".to_string()),
            ("refquota".to_string(), max_quota.to_string()),
        ],
    )
    .await?;

    let volumes = host.zfs_list_volumes(root).await?;
    let volsize: u64 = volumes.iter().map(|(_, s)| *s).sum();
    for (name, _) in &volumes {
        host.zfs_set(name, &[("refreservation".to_string(), "auto".to_string())])
            .await?;
    }
    let volrefres = host.zfs_sum_volume_refreservation(root).await?;

    let plan = crate::quota::compute_bhyve_quota(
        spec.quota_gib,
        spec.flexible_disk_size_mib,
        volsize,
        volrefres,
    );
    host.zfs_set(
        root,
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

/// bhyve nic-tag registration: best-effort. A failure is logged but does
/// not block create — viona attaches the raw link regardless.
async fn ensure_nic_tags_best_effort<N: NicTagRegistrar>(nictags: &N, spec: &VmCreateSpec) {
    for link in spec.nic_tags() {
        if let Err(e) = nictags.ensure(link).await {
            tracing::warn!(link, error = %e, "nic_tag registration failed; bhyve viona tolerates the raw link");
        }
    }
}

/// Ensure every NIC's host link is a registered nic_tag, failing the whole
/// create for joyent-minimal if registration fails (the brand will not
/// boot otherwise). bhyve's best-effort variant lives in the bhyve path.
async fn ensure_nic_tags_fail_closed<N: NicTagRegistrar>(
    nictags: &N,
    spec: &VmCreateSpec,
) -> Result<()> {
    for link in spec.nic_tags() {
        if let Err(e) = nictags.ensure(link).await {
            tracing::error!(
                link,
                error = %e,
                "nic_tag registration failed; joyent-minimal zone would not boot",
            );
            return Err(VmadmError::NicTagUnregistered {
                link: link.to_string(),
                brand: spec.brand.as_str().to_string(),
            });
        }
    }
    Ok(())
}

/// Run the journal's undo actions in reverse, best-effort. Returns the
/// number of undo steps that FAILED (so the caller can decide whether to
/// drop the journal or keep it as a residue marker).
async fn rollback<H: ZoneHost>(host: &H, journal: &ResourceJournal) -> usize {
    let mut failed = 0;
    for action in journal.rollback_actions() {
        if let Err(e) = execute_undo(host, &action).await {
            tracing::warn!(?action, error = %e, "rollback step failed; host residue remains");
            failed += 1;
        }
    }
    failed
}

async fn execute_undo<H: ZoneHost>(host: &H, action: &UndoAction) -> Result<()> {
    match action {
        UndoAction::ZfsDestroyRecursive { dataset } => host.zfs_destroy_recursive(dataset).await,
        UndoAction::ZoneadmUninstallForce { uuid } => host.zoneadm_uninstall_force(*uuid).await,
        UndoAction::ZonecfgDeleteForce { uuid } => host.zonecfg_delete_force(*uuid).await,
        UndoAction::RemovePath { path } => {
            let _ = tokio::fs::remove_file(path).await;
            Ok(())
        }
        UndoAction::Noop => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::{FakeHost, FakeNicTags};
    use crate::backend::traits::ProvisionOutcome;
    use crate::payload::{VmadmCreatePayload, normalize};

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(0, 0).expect("epoch")
    }

    /// A joyent-minimal spec whose zonepath points into `tmp` so the file
    /// writes land in a sandbox instead of `/zones/...`.
    fn jm_spec(tmp: &Path) -> VmCreateSpec {
        let payload = VmadmCreatePayload::from_json_slice(
            serde_json::json!({
                "uuid": "11111111-1111-1111-1111-111111111111",
                "brand": "joyent-minimal",
                "image_uuid": "22222222-2222-2222-2222-222222222222",
                "alias": "fdb0", "hostname": "fdb0",
                "max_physical_memory": 2048, "max_locked_memory": 2048,
                "max_swap": 4096, "tmpfs": 1024,
                "cpu_cap": 200, "cpu_shares": 200, "quota": 25,
                "resolvers": ["10.199.199.14"],
                "nics": [{ "interface": "net0", "nic_tag": "admin",
                    "ip": "10.199.199.50", "netmask": "255.255.255.0",
                    "gateway": "10.199.199.1", "mtu": 1500,
                    "mac": "02:00:00:de:ad:01", "primary": true }]
            })
            .to_string()
            .as_bytes(),
        )
        .expect("payload");
        let mut spec = normalize(payload, now()).expect("normalize");
        spec.zonepath = tmp
            .join(spec.uuid.to_string())
            .to_string_lossy()
            .into_owned();
        spec
    }

    fn index_of(ops: &[String], needle: &str) -> Option<usize> {
        ops.iter().position(|o| o.contains(needle))
    }

    #[tokio::test]
    async fn joyent_minimal_create_boot_happy_path() {
        let tmp = tempfile::tempdir().expect("tmp");
        let jdir = tempfile::tempdir().expect("jdir");
        let spec = jm_spec(tmp.path());
        let host = FakeHost::new();
        let nics = FakeNicTags::new();

        let uuid = create_in(&host, &nics, &spec, now(), jdir.path())
            .await
            .expect("create");
        assert_eq!(uuid, spec.uuid);

        let ops = host.ops();
        // The ordered control flow.
        let i_cfg = index_of(&ops, "create -b").expect("zonecfg create");
        let i_install = index_of(&ops, "install -q 25").expect("install");
        let i_rmprov = index_of(&ops, "remove attr name=provisioning").expect("rm prov");
        let i_boot = index_of(&ops, "boot").expect("boot");
        let i_wait = index_of(&ops, "wait_provisioning").expect("wait");
        let i_rmtrans = index_of(&ops, "remove attr name=transition").expect("rm trans");
        assert!(i_cfg < i_install, "config before install");
        assert!(i_install < i_rmprov, "install before rm-provisioning");
        assert!(i_rmprov < i_boot, "rm-provisioning before boot");
        assert!(
            i_boot < i_wait && i_wait < i_rmtrans,
            "boot, wait, rm-transition"
        );
        // install carries the image clone source + nodataset.
        assert!(
            ops.iter()
                .any(|o| o.contains("-t 22222222-2222-2222-2222-222222222222")
                    && o.contains("-x nodataset"))
        );
        // nic_tag was ensured before boot (fail-closed precondition).
        assert_eq!(nics.ensured(), vec!["admin".to_string()]);
        // Config files landed under the sandbox zonepath.
        let cfg = Path::new(&spec.zonepath).join("config/metadata.json");
        assert!(cfg.exists(), "metadata.json written");
        assert!(Path::new(&spec.zonepath).join("root/etc/nodename").exists());
        // Success removes the journal.
        assert!(
            !jdir.path().join(format!("{}.journal", spec.uuid)).exists(),
            "journal removed on success"
        );
    }

    #[tokio::test]
    async fn duplicate_uuid_is_already_exists() {
        let tmp = tempfile::tempdir().expect("tmp");
        let jdir = tempfile::tempdir().expect("jdir");
        let spec = jm_spec(tmp.path());
        let host = FakeHost::new();
        host.mark_configured(spec.uuid);
        let err = create_in(&host, &FakeNicTags::new(), &spec, now(), jdir.path())
            .await
            .expect_err("should fail");
        assert!(matches!(err, VmadmError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn rollback_on_install_failure() {
        let tmp = tempfile::tempdir().expect("tmp");
        let jdir = tempfile::tempdir().expect("jdir");
        let spec = jm_spec(tmp.path());
        // "install -q" matches the install op but NOT the rollback's
        // `uninstall -F`, so rollback succeeds and the original error
        // surfaces (a bare "install" would also fail the uninstall undo).
        let host = FakeHost::new().failing_on("install -q");

        let err = create_in(&host, &FakeNicTags::new(), &spec, now(), jdir.path())
            .await
            .expect_err("install fails");
        assert!(matches!(err, VmadmError::Payload(_)));

        let ops = host.ops();
        // Rollback ran in reverse: uninstall (install undo) then zonecfg
        // delete (config undo).
        let i_uninstall = index_of(&ops, "uninstall -F").expect("uninstall");
        let i_delete = index_of(&ops, "delete -F").expect("zonecfg delete");
        assert!(i_uninstall < i_delete, "uninstall before delete");
        // Never booted.
        assert!(index_of(&ops, "boot").is_none());
    }

    #[tokio::test]
    async fn nic_tag_failure_is_fail_closed_for_joyent_minimal() {
        let tmp = tempfile::tempdir().expect("tmp");
        let jdir = tempfile::tempdir().expect("jdir");
        let spec = jm_spec(tmp.path());
        let host = FakeHost::new();
        let nics = FakeNicTags::failing();

        let err = create_in(&host, &nics, &spec, now(), jdir.path())
            .await
            .expect_err("nic tag fails closed");
        assert!(matches!(err, VmadmError::NicTagUnregistered { .. }));
        // The zone never booted, and create rolled back.
        let ops = host.ops();
        assert!(index_of(&ops, "boot").is_none(), "must not boot");
        assert!(index_of(&ops, "uninstall -F").is_some(), "rolled back");
    }

    fn bhyve_spec(tmp: &Path) -> VmCreateSpec {
        let payload = VmadmCreatePayload::from_json_slice(
            serde_json::json!({
                "uuid": "11111111-1111-1111-1111-111111111111",
                "brand": "bhyve", "alias": "web0", "hostname": "web0",
                "ram": 2048, "vcpus": 2, "bootrom": "uefi",
                "flexible_disk_size": 20480,
                "bhyve_extra_opts": "-o migrate.export=true",
                "disks": [{ "uuid": "44444444-4444-4444-4444-444444444444",
                    "boot": true, "model": "virtio",
                    "image_uuid": "33333333-3333-3333-3333-333333333333",
                    "size": 20480 }],
                "nics": [{ "interface": "net0", "nic_tag": "proteus49377",
                    "model": "virtio", "mac": "02:00:00:de:ad:02",
                    "ips": ["10.0.0.5/24"], "gateways": ["10.0.0.1"],
                    "mtu": 1500, "primary": true }]
            })
            .to_string()
            .as_bytes(),
        )
        .expect("payload");
        let mut spec = normalize(payload, now()).expect("normalize");
        spec.zonepath = tmp
            .join(spec.uuid.to_string())
            .to_string_lossy()
            .into_owned();
        spec
    }

    #[tokio::test]
    async fn bhyve_create_clones_disk_applies_quota_and_boots() {
        let tmp = tempfile::tempdir().expect("tmp");
        let jdir = tempfile::tempdir().expect("jdir");
        let spec = bhyve_spec(tmp.path());
        let host = FakeHost::new();
        let nics = FakeNicTags::new();

        let uuid = create_in(&host, &nics, &spec, now(), jdir.path())
            .await
            .expect("create");
        assert_eq!(uuid, spec.uuid);

        let ops = host.ops();
        let root = "zones/11111111-1111-1111-1111-111111111111";
        // zoneroot before disks.
        let i_root = index_of(&ops, &format!("zfs create {root}")).expect("zoneroot");
        let i_snap = index_of(
            &ops,
            "zfs get name zones/33333333-3333-3333-3333-333333333333@final",
        )
        .expect("snap probe");
        let i_clone = index_of(&ops, "zfs clone -F").expect("clone");
        assert!(
            i_root < i_snap && i_snap < i_clone,
            "root -> snap probe -> clone"
        );
        // clone targets the @final snapshot and disk0, with volsize.
        assert!(ops.iter().any(|o| o.contains("@final")
            && o.contains(&format!("{root}/disk0"))
            && o.contains("-o volsize=20480M")
            && o.contains("-o refreservation=auto")));
        // RFD-0148 quota sequence on the zoneroot.
        assert!(
            ops.iter()
                .any(|o| o.contains("zfs set refreservation=none refquota=") && o.ends_with(root))
        );
        assert!(
            ops.iter()
                .any(|o| o.contains(&format!("zfs set refreservation=auto {root}/disk0")))
        );
        assert!(
            ops.iter()
                .any(|o| o.starts_with("zfs set quota=") && o.contains("reservation="))
        );
        // install has NO -t (root + disks pre-created) and -x nodataset.
        let install = ops.iter().find(|o| o.contains("install")).expect("install");
        assert!(
            install.contains("-x nodataset") && !install.contains("-t "),
            "{install}"
        );
        // booted, nic-tag ensured (best-effort still tries).
        assert!(index_of(&ops, "boot").is_some());
        assert_eq!(nics.ensured(), vec!["proteus49377".to_string()]);
    }

    #[tokio::test]
    async fn bhyve_nocloud_adds_seed_disk_and_builds_cidata() {
        let tmp = tempfile::tempdir().expect("tmp");
        let jdir = tempfile::tempdir().expect("jdir");
        // A bhyve payload opting into NoCloud.
        let payload = VmadmCreatePayload::from_json_slice(
            serde_json::json!({
                "uuid": "11111111-1111-1111-1111-111111111111",
                "brand": "bhyve", "ram": 2048, "vcpus": 2,
                "flexible_disk_size": 20480,
                "internal_metadata": { "cloudinit_datasource": "nocloud" },
                "customer_metadata": {
                    "cloud-init:user-data": "#cloud-config\n{}\n",
                    "cloud-init:meta-data": "instance-id: x\n",
                    "cloud-init:network-config": "version: 2\n"
                },
                "disks": [{ "boot": true, "model": "virtio",
                    "image_uuid": "33333333-3333-3333-3333-333333333333",
                    "size": 20480 }],
                "nics": [{ "nic_tag": "proteus49377", "model": "virtio",
                    "mac": "02:00:00:de:ad:02", "ips": ["10.0.0.5/24"],
                    "gateways": ["10.0.0.1"], "primary": true }]
            })
            .to_string()
            .as_bytes(),
        )
        .expect("payload");
        let mut spec = normalize(payload, now()).expect("normalize");
        spec.zonepath = tmp
            .path()
            .join(spec.uuid.to_string())
            .to_string_lossy()
            .into_owned();

        // The seed disk was appended and flex bumped by 16.
        assert_eq!(spec.disks.len(), 2);
        assert!(spec.disks[1].cloudinit_seed);
        assert_eq!(spec.flexible_disk_size_mib, Some(20480 + 16));
        assert_eq!(
            spec.internal_metadata
                .get("cloudinit_nocloud_path")
                .and_then(|v| v.as_str()),
            Some("/dev/zvol/rdsk/zones/11111111-1111-1111-1111-111111111111/disk1")
        );

        let host = FakeHost::new();
        create_in(&host, &FakeNicTags::new(), &spec, now(), jdir.path())
            .await
            .expect("create");

        let ops = host.ops();
        let seed_ds = "zones/11111111-1111-1111-1111-111111111111/disk1";
        // The seed is a 16 MiB blank zvol that then gets the cidata image.
        assert!(
            ops.iter()
                .any(|o| o.contains("zfs create") && o.contains("-V 16M") && o.contains(seed_ds))
        );
        assert!(
            ops.iter()
                .any(|o| o == &format!("build_nocloud_seed /dev/zvol/rdsk/{seed_ds}"))
        );
    }

    /// A nocloud bhyve guest with NO flexible_disk_size must add the seed
    /// disk WITHOUT introducing a flex budget — otherwise the quota switches
    /// to the flex form and computes a zoneroot quota below the boot disk's
    /// own reservation (`size is less than current reserved`, caught live).
    #[tokio::test]
    async fn bhyve_nocloud_without_flex_stays_non_flex() {
        let payload = VmadmCreatePayload::from_json_slice(
            serde_json::json!({
                "uuid": "11111111-1111-1111-1111-111111111111",
                "brand": "bhyve", "ram": 1024, "vcpus": 1,
                "internal_metadata": { "cloudinit_datasource": "nocloud" },
                "disks": [{ "boot": true, "model": "virtio",
                    "image_uuid": "33333333-3333-3333-3333-333333333333",
                    "size": 2048 }],
                "nics": [{ "nic_tag": "admin", "ips": ["10.0.0.5/24"], "primary": true }]
            })
            .to_string()
            .as_bytes(),
        )
        .expect("payload");
        let spec = normalize(payload, now()).expect("normalize");
        // Seed disk added, but no flex budget introduced.
        assert_eq!(spec.disks.len(), 2);
        assert!(spec.disks[1].cloudinit_seed);
        assert_eq!(spec.flexible_disk_size_mib, None);
    }

    #[tokio::test]
    async fn bhyve_rolls_back_zoneroot_on_zonecfg_failure() {
        let tmp = tempfile::tempdir().expect("tmp");
        let jdir = tempfile::tempdir().expect("jdir");
        let spec = bhyve_spec(tmp.path());
        // Fail at zonecfg (after zoneroot + disk are created).
        let host = FakeHost::new().failing_on("create -b");
        let err = create_in(&host, &FakeNicTags::new(), &spec, now(), jdir.path())
            .await
            .expect_err("zonecfg fails");
        assert!(matches!(err, VmadmError::Payload(_)));
        // The zoneroot dataset is destroyed on rollback.
        let ops = host.ops();
        assert!(
            ops.iter()
                .any(|o| o.contains("zfs destroy -rF zones/11111111-1111-1111-1111-111111111111")),
            "{ops:?}"
        );
        assert!(index_of(&ops, "boot").is_none());
    }

    #[tokio::test]
    async fn provision_failure_surfaces_but_keeps_zone() {
        let tmp = tempfile::tempdir().expect("tmp");
        let jdir = tempfile::tempdir().expect("jdir");
        let spec = jm_spec(tmp.path());
        let host =
            FakeHost::new().with_provision(ProvisionOutcome::Failure("guest failed".to_string()));

        let err = create_in(&host, &FakeNicTags::new(), &spec, now(), jdir.path())
            .await
            .expect_err("provision failed");
        assert!(matches!(err, VmadmError::Payload(_)));
        // It booted (zone is built) and was NOT uninstalled.
        let ops = host.ops();
        assert!(index_of(&ops, "boot").is_some());
        assert!(
            index_of(&ops, "uninstall -F").is_none(),
            "zone kept for inspection"
        );
    }
}
