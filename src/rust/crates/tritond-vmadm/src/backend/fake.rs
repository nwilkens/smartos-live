// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! In-memory test doubles for [`ZoneHost`] and [`NicTagRegistrar`]. The
//! `FakeHost` records an ordered op log so the create/rollback flow can be
//! asserted without a live CN, and can inject a failure at a chosen step.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use crate::backend::traits::{
    HostFacts, InstallArgs, NicTagRegistrar, ProvisionOutcome, SnapshotInfo, ZoneHost, ZoneKstat,
    ZoneRef, ZoneRuntime,
};
use crate::error::{Result, VmadmError};

/// Records every host call in order and fakes zone state.
#[derive(Default)]
pub struct FakeHost {
    ops: Mutex<Vec<String>>,
    configured: Mutex<HashSet<Uuid>>,
    /// Created zvols: (dataset, volsize bytes). Drives the quota math.
    volumes: Mutex<Vec<(String, u64)>>,
    /// When set, the first op whose tag contains this substring fails.
    fail_on: Option<String>,
    /// Outcome returned by `wait_provisioning` (default Success).
    provision: Option<ProvisionOutcome>,
    /// Canned `zone_info` / `zonecfg export` / `zfs get` responses for the
    /// get/list/lifecycle tests.
    zone_infos: Mutex<HashMap<Uuid, ZoneRuntime>>,
    exports: Mutex<HashMap<Uuid, String>>,
    zfs_props: Mutex<HashMap<String, BTreeMap<String, String>>>,
}

impl FakeHost {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inject a failure at the first op containing `tag`.
    #[must_use]
    pub fn failing_on(mut self, tag: &str) -> Self {
        self.fail_on = Some(tag.to_string());
        self
    }

    #[must_use]
    pub fn with_provision(mut self, outcome: ProvisionOutcome) -> Self {
        self.provision = Some(outcome);
        self
    }

    /// Pre-mark a zone as already configured (for the uniqueness test).
    pub fn mark_configured(&self, uuid: Uuid) {
        lock(&self.configured).insert(uuid);
    }

    /// Set the canned `zone_info` for a uuid (also marks it configured).
    pub fn set_zone_info(&self, uuid: Uuid, info: ZoneRuntime) {
        lock(&self.configured).insert(uuid);
        lock(&self.zone_infos).insert(uuid, info);
    }

    /// Set the canned `zonecfg export` for a uuid.
    pub fn set_export(&self, uuid: Uuid, export: impl Into<String>) {
        lock(&self.exports).insert(uuid, export.into());
    }

    /// Set canned `zfs get` properties for a dataset.
    pub fn set_zfs_props(&self, dataset: impl Into<String>, props: BTreeMap<String, String>) {
        lock(&self.zfs_props).insert(dataset.into(), props);
    }

    /// The ordered op log.
    #[must_use]
    pub fn ops(&self) -> Vec<String> {
        lock(&self.ops).clone()
    }

    fn record(&self, op: impl Into<String>) -> Result<()> {
        let op = op.into();
        lock(&self.ops).push(op.clone());
        if let Some(tag) = &self.fail_on
            && op.contains(tag.as_str())
        {
            return Err(VmadmError::Payload(format!("injected failure at {op}")));
        }
        Ok(())
    }
}

/// Lock a mutex, recovering the guard from a poisoned lock rather than
/// panicking (avoids `unwrap`/`expect` in this non-test-gated module).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Parse a `<n>M` size string into bytes (the `-o volsize=<n>M` form).
fn parse_mib_suffix(s: &str) -> Option<u64> {
    s.strip_suffix('M')
        .and_then(|n| n.parse::<u64>().ok())
        .map(|mib| mib * 1024 * 1024)
}

fn zoneref_str(z: &ZoneRef) -> String {
    match z {
        ZoneRef::Zonename(n) => format!("-z {n}"),
        ZoneRef::Uuid(u) => format!("-u {u}"),
    }
}

#[async_trait]
impl ZoneHost for FakeHost {
    async fn zone_configured(&self, uuid: Uuid) -> Result<bool> {
        self.record(format!("zone_configured {uuid}"))?;
        Ok(lock(&self.configured).contains(&uuid))
    }

    async fn zonecfg(&self, zone: ZoneRef, doc: &str) -> Result<()> {
        // Record the ref + the first line so order + intent are visible
        // without dumping the whole blob.
        let first = doc.lines().next().unwrap_or("").to_string();
        self.record(format!("zonecfg {} :: {first}", zoneref_str(&zone)))
    }

    async fn zoneadm_install(&self, args: &InstallArgs) -> Result<()> {
        let t = args
            .image_uuid
            .map(|u| format!(" -t {u}"))
            .unwrap_or_default();
        let x = if args.no_dataset { " -x nodataset" } else { "" };
        self.record(format!(
            "zoneadm -z {} install -q {}{t}{x}",
            args.zonename, args.quota_gib
        ))?;
        if let Ok(u) = args.zonename.parse::<Uuid>() {
            lock(&self.configured).insert(u);
        }
        Ok(())
    }

    async fn zoneadm_boot(&self, uuid: Uuid) -> Result<()> {
        self.record(format!("zoneadm -u {uuid} boot"))
    }

    async fn zfs_create(&self, dataset: &str, props: &[(String, String)]) -> Result<()> {
        let p = props
            .iter()
            .map(|(k, v)| format!(" -o {k}={v}"))
            .collect::<String>();
        self.record(format!("zfs create{p} {dataset}"))
    }

    async fn zfs_snapshot_exists(&self, snapshot: &str) -> Result<bool> {
        self.record(format!("zfs get name {snapshot}"))?;
        // Default: the canonical @final image snapshot exists.
        Ok(snapshot.ends_with("@final"))
    }

    async fn zfs_snapshot(&self, snapshot: &str) -> Result<()> {
        self.record(format!("zfs snapshot {snapshot}"))
    }

    async fn zfs_clone(
        &self,
        snapshot: &str,
        dataset: &str,
        props: &[(String, String)],
    ) -> Result<()> {
        let p = props
            .iter()
            .map(|(k, v)| format!(" -o {k}={v}"))
            .collect::<String>();
        self.record(format!("zfs clone -F{p} {snapshot} {dataset}"))?;
        // Track the resulting volume + its volsize (from the -o volsize=NM
        // prop) so the quota math can read it back.
        let volsize = props
            .iter()
            .find(|(k, _)| k == "volsize")
            .and_then(|(_, v)| parse_mib_suffix(v));
        lock(&self.volumes).push((dataset.to_string(), volsize.unwrap_or(0)));
        Ok(())
    }

    async fn zfs_create_volume(
        &self,
        dataset: &str,
        size_mib: u64,
        props: &[(String, String)],
    ) -> Result<()> {
        let p = props
            .iter()
            .map(|(k, v)| format!(" -o {k}={v}"))
            .collect::<String>();
        self.record(format!("zfs create{p} -V {size_mib}M {dataset}"))?;
        lock(&self.volumes).push((dataset.to_string(), size_mib * 1024 * 1024));
        Ok(())
    }

    async fn zfs_set(&self, dataset: &str, props: &[(String, String)]) -> Result<()> {
        let p = props
            .iter()
            .map(|(k, v)| format!("{k}={v} "))
            .collect::<String>();
        self.record(format!("zfs set {}{dataset}", p))
    }

    async fn zfs_list_volumes(&self, _dataset: &str) -> Result<Vec<(String, u64)>> {
        Ok(lock(&self.volumes).clone())
    }

    async fn zfs_sum_volume_refreservation(&self, _dataset: &str) -> Result<u64> {
        // Fake the auto refreservation as equal to the summed volsize.
        Ok(lock(&self.volumes).iter().map(|(_, sz)| *sz).sum())
    }

    async fn build_nocloud_seed(
        &self,
        raw_zvol_path: &str,
        _files: &crate::seed::SeedFiles,
    ) -> Result<()> {
        self.record(format!("build_nocloud_seed {raw_zvol_path}"))
    }

    async fn zone_info(&self, uuid: Uuid) -> Result<Option<ZoneRuntime>> {
        self.record(format!("zone_info {uuid}"))?;
        if let Some(info) = lock(&self.zone_infos).get(&uuid) {
            return Ok(Some(info.clone()));
        }
        if lock(&self.configured).contains(&uuid) {
            return Ok(Some(ZoneRuntime {
                zoneid: Some(1),
                state: "running".to_string(),
                brand: "joyent-minimal".to_string(),
                zonepath: format!("/zones/{uuid}"),
                zonedid: Some(1),
            }));
        }
        Ok(None)
    }

    async fn list_zone_uuids(&self) -> Result<Vec<Uuid>> {
        self.record("list_zone_uuids")?;
        Ok(lock(&self.configured).iter().copied().collect())
    }

    async fn zoneadm_halt(&self, uuid: Uuid) -> Result<()> {
        self.record(format!("zoneadm -u {uuid} halt"))
    }

    async fn zoneadm_reboot(&self, uuid: Uuid) -> Result<()> {
        self.record(format!("zoneadm -u {uuid} reboot"))
    }

    async fn zlogin_shutdown(&self, uuid: Uuid, _timeout: Duration) -> Result<()> {
        self.record(format!("zlogin {uuid} shutdown"))
    }

    async fn zonecfg_export(&self, uuid: Uuid) -> Result<String> {
        self.record(format!("zonecfg -u {uuid} export"))?;
        Ok(lock(&self.exports).get(&uuid).cloned().unwrap_or_default())
    }

    async fn zfs_get_props(
        &self,
        dataset: &str,
        _props: &[&str],
    ) -> Result<BTreeMap<String, String>> {
        self.record(format!("zfs get {dataset}"))?;
        Ok(lock(&self.zfs_props)
            .get(dataset)
            .cloned()
            .unwrap_or_default())
    }

    async fn zone_kstat(&self, zoneid: i64, _zonename: &str) -> Result<ZoneKstat> {
        self.record(format!("zone_kstat {zoneid}"))?;
        Ok(ZoneKstat::default())
    }

    async fn host_facts(&self) -> Result<HostFacts> {
        self.record("host_facts")?;
        Ok(HostFacts::default())
    }

    async fn zfs_list_snapshots(&self, dataset: &str) -> Result<Vec<SnapshotInfo>> {
        self.record(format!("zfs list snapshots {dataset}"))?;
        Ok(Vec::new())
    }

    async fn svccfg_mdata_exec_timeout(&self, zonepath: &str, secs: u64) -> Result<()> {
        self.record(format!("svccfg mdata_exec_timeout {zonepath} {secs}"))
    }

    async fn wait_provisioning(
        &self,
        zonepath: &str,
        _timeout: Duration,
    ) -> Result<ProvisionOutcome> {
        self.record(format!("wait_provisioning {zonepath}"))?;
        Ok(self.provision.clone().unwrap_or(ProvisionOutcome::Success))
    }

    async fn zfs_destroy_recursive(&self, dataset: &str) -> Result<()> {
        self.record(format!("zfs destroy -rf {dataset}"))
    }

    async fn zoneadm_uninstall_force(&self, uuid: Uuid) -> Result<()> {
        lock(&self.configured).remove(&uuid);
        self.record(format!("zoneadm -u {uuid} uninstall -F"))
    }

    async fn zonecfg_delete_force(&self, uuid: Uuid) -> Result<()> {
        self.record(format!("zonecfg -u {uuid} delete -F"))
    }
}

/// A nic_tag registrar that records ensured links and optionally fails.
#[derive(Default)]
pub struct FakeNicTags {
    ensured: Mutex<Vec<String>>,
    fail: bool,
}

impl FakeNicTags {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn failing() -> Self {
        Self {
            ensured: Mutex::new(Vec::new()),
            fail: true,
        }
    }

    #[must_use]
    pub fn ensured(&self) -> Vec<String> {
        lock(&self.ensured).clone()
    }
}

#[async_trait]
impl NicTagRegistrar for FakeNicTags {
    async fn ensure(&self, link: &str) -> Result<()> {
        lock(&self.ensured).push(link.to_string());
        if self.fail {
            return Err(VmadmError::Payload(format!(
                "injected nic_tag failure for {link}"
            )));
        }
        Ok(())
    }
}
